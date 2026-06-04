// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use lru::LruCache;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{watch, Mutex, OnceCell};

use crate::download::DownloadConfig;
use crate::proto::StatusResult;
use crate::tp_client::TraceProcessorClient;

const STDERR_TAIL_CAPACITY: usize = 100;
const READY_POLL_INTERVAL: Duration = Duration::from_millis(100);
const STATUS_FALLBACK_DELAY: Duration = Duration::from_millis(500);
const STATUS_FALLBACK_STABILITY: Duration = Duration::from_millis(300);

type SharedStderrTail = Arc<StdMutex<std::collections::VecDeque<String>>>;

#[derive(Debug, Clone, Copy)]
pub struct TraceProcessorConfig {
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
}

impl Default for TraceProcessorConfig {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum StartupState {
    #[default]
    Waiting,
    Ready,
    Ipv4BindFailed(String),
}

#[derive(Debug, Default)]
struct StartupLogState {
    saw_ipv4_start: bool,
    saw_ipv4_bind_failure: bool,
}

#[derive(Debug)]
enum WaitPhase {
    StderrGated,
    StatusFallback { ok_since: Option<Instant> },
}

/// A running trace_processor_shell instance bound to a specific trace file.
struct TraceProcessorInstance {
    process: Child,
    port: u16,
    client: TraceProcessorClient,
    stderr_tail: SharedStderrTail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TraceFileFingerprint {
    size_bytes: u64,
    modified: Option<SystemTime>,
}

impl TraceFileFingerprint {
    fn from_path(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("failed to stat trace file: {}", path.display()))?;
        Ok(Self {
            size_bytes: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

struct CachedTraceProcessorInstance {
    instance: TraceProcessorInstance,
    fingerprint: TraceFileFingerprint,
}

impl TraceProcessorInstance {
    /// Spawn trace_processor_shell in HTTP-RPC mode on the given port.
    async fn spawn(
        binary: &Path,
        trace_path: &Path,
        port: u16,
        config: TraceProcessorConfig,
    ) -> Result<Self> {
        let mut process = Command::new(binary)
            .arg("-D")
            .arg("--http-port")
            .arg(port.to_string())
            .arg(trace_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn {} for {}",
                    binary.display(),
                    trace_path.display(),
                )
            })?;

        let stderr = process
            .stderr
            .take()
            .context("failed to capture trace_processor_shell stderr")?;
        let stdout = process
            .stdout
            .take()
            .context("failed to capture trace_processor_shell stdout")?;
        let (mut startup_rx, stderr_tail) = spawn_output_drains(stderr, stdout, port);
        let client = TraceProcessorClient::new(port, config.request_timeout);

        let mut instance = Self {
            process,
            port,
            client,
            stderr_tail,
        };
        instance
            .wait_ready(trace_path, &mut startup_rx, config.startup_timeout)
            .await?;
        Ok(instance)
    }

    /// Poll the /status endpoint until the instance is ready.
    async fn wait_ready(
        &mut self,
        expected_trace: &Path,
        startup_rx: &mut watch::Receiver<StartupState>,
        startup_timeout: Duration,
    ) -> Result<()> {
        let client = self.client.clone();
        self.wait_ready_with_status(expected_trace, startup_rx, startup_timeout, || async {
            client.status().await
        })
        .await
    }

    async fn wait_ready_with_status<F, Fut>(
        &mut self,
        expected_trace: &Path,
        startup_rx: &mut watch::Receiver<StartupState>,
        startup_timeout: Duration,
        mut check_status: F,
    ) -> Result<()>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<StatusResult, crate::error::PerfettoError>>,
    {
        let start = Instant::now();
        let deadline = start + startup_timeout;
        let mut phase = WaitPhase::StderrGated;
        let mut emitted_wait_log = false;

        loop {
            if let Some(status) = self.process.try_wait()? {
                bail!(
                    "trace_processor_shell exited with {status} on port {}{}",
                    self.port,
                    format_stderr_tail(&self.stderr_tail),
                );
            }

            let startup_state = startup_rx.borrow().clone();
            match startup_state {
                StartupState::Waiting => {
                    if matches!(phase, WaitPhase::StderrGated)
                        && start.elapsed() >= STATUS_FALLBACK_DELAY
                    {
                        tracing::warn!(
                            "no recognized stderr readiness marker for trace_processor_shell on port {} after {:?}; falling back to /status + loaded_trace_name verification{}",
                            self.port,
                            STATUS_FALLBACK_DELAY,
                            format_stderr_tail(&self.stderr_tail),
                        );
                        phase = WaitPhase::StatusFallback { ok_since: None };
                    }

                    if let WaitPhase::StatusFallback { ok_since } = &mut phase {
                        match check_status().await {
                            Ok(status)
                                if status_matches_expected_trace(&status, expected_trace) =>
                            {
                                let first_ok = ok_since.get_or_insert_with(Instant::now);
                                if first_ok.elapsed() >= STATUS_FALLBACK_STABILITY {
                                    return Ok(());
                                }
                            }
                            Ok(_) | Err(_) => {
                                *ok_since = None;
                            }
                        }
                    }
                }
                StartupState::Ready => {
                    if check_status().await.is_ok() {
                        return Ok(());
                    }
                }
                StartupState::Ipv4BindFailed(line) => {
                    bail!(
                        "trace_processor_shell failed to bind 127.0.0.1:{}: {line}{}",
                        self.port,
                        format_stderr_tail(&self.stderr_tail),
                    );
                }
            }

            if Instant::now() >= deadline {
                bail!(
                    "trace_processor_shell on port {} did not become ready within {:?}{}",
                    self.port,
                    startup_timeout,
                    format_stderr_tail(&self.stderr_tail),
                );
            }

            if !emitted_wait_log && start.elapsed() >= startup_timeout / 2 {
                tracing::debug!(
                    "still waiting for trace_processor_shell on port {}",
                    self.port,
                );
                emitted_wait_log = true;
            }

            tokio::select! {
                changed = startup_rx.changed() => {
                    if changed.is_err() {
                        tokio::time::sleep(READY_POLL_INTERVAL).await;
                    }
                }
                _ = tokio::time::sleep(READY_POLL_INTERVAL) => {}
            }
        }
    }

    /// Check if the underlying process is still alive.
    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        self.process
            .try_wait()
            .context("failed to poll child process")
    }
}

impl Drop for TraceProcessorInstance {
    fn drop(&mut self) {
        // kill_on_drop handles cleanup, but log for observability.
        tracing::debug!("dropping trace_processor_shell on port {}", self.port);
    }
}

/// Manages a pool of trace_processor_shell instances, one per trace file,
/// with LRU eviction when the pool exceeds `max_instances`. Instances are
/// keyed by canonical path and invalidated when that file's size/mtime
/// fingerprint changes.
pub struct TraceProcessorManager {
    inner: Mutex<ManagerInner>,
    spawn_locks: Mutex<std::collections::HashMap<PathBuf, Arc<Mutex<()>>>>,
    binary_path: OnceCell<PathBuf>,
    config: TraceProcessorConfig,
    download_config: DownloadConfig,
}

impl std::fmt::Debug for TraceProcessorManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceProcessorManager")
            .field("binary_path", &self.binary_path.get())
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

struct ManagerInner {
    instances: LruCache<PathBuf, CachedTraceProcessorInstance>,
    next_port: u16,
    starting_port: u16,
}

impl TraceProcessorManager {
    pub const DEFAULT_STARTING_PORT: u16 = 9001;

    fn new_inner(
        max_instances: usize,
        starting_port: u16,
        config: TraceProcessorConfig,
        download_config: DownloadConfig,
    ) -> Self {
        let cap = NonZeroUsize::new(max_instances).unwrap_or(NonZeroUsize::MIN);
        Self {
            inner: Mutex::new(ManagerInner {
                instances: LruCache::new(cap),
                next_port: starting_port,
                starting_port,
            }),
            spawn_locks: Mutex::new(std::collections::HashMap::new()),
            binary_path: OnceCell::new(),
            config,
            download_config,
        }
    }

    pub fn new(max_instances: usize) -> Self {
        Self::new_inner(
            max_instances,
            Self::DEFAULT_STARTING_PORT,
            TraceProcessorConfig::default(),
            DownloadConfig::default(),
        )
    }

    pub fn new_with_configs(
        max_instances: usize,
        config: TraceProcessorConfig,
        download_config: DownloadConfig,
    ) -> Self {
        Self::new_inner(
            max_instances,
            Self::DEFAULT_STARTING_PORT,
            config,
            download_config,
        )
    }

    pub fn new_with_starting_port(max_instances: usize, starting_port: u16) -> Self {
        Self::new_inner(
            max_instances,
            starting_port,
            TraceProcessorConfig::default(),
            DownloadConfig::default(),
        )
    }

    pub fn new_with_starting_port_and_configs(
        max_instances: usize,
        starting_port: u16,
        config: TraceProcessorConfig,
        download_config: DownloadConfig,
    ) -> Self {
        Self::new_inner(max_instances, starting_port, config, download_config)
    }

    /// Create a manager with a pre-resolved binary path (tests only, avoids
    /// any download or PATH lookup).
    #[cfg(test)]
    pub fn new_with_binary(binary_path: PathBuf, max_instances: usize) -> Self {
        let this = Self::new(max_instances);
        this.binary_path
            .set(binary_path)
            .expect("binary_path not yet initialized");
        this
    }

    /// Resolve `trace_processor_shell`, downloading it on first call if needed.
    /// Errors are not cached, so a transient download failure can be retried.
    async fn ensure_binary(&self) -> Result<&Path> {
        let path = self
            .binary_path
            .get_or_try_init(|| async {
                let p = crate::download::ensure_binary(&self.download_config).await?;
                tracing::info!("using trace_processor_shell: {}", p.display());
                Ok::<PathBuf, anyhow::Error>(p)
            })
            .await?;
        Ok(path.as_path())
    }

    /// Get or create a `TraceProcessorClient` for the given trace file.
    ///
    /// If the instance already exists in the cache, it is returned (and
    /// promoted in LRU order). If the instance's process has died, it is
    /// respawned. If the cache is full, the least recently used instance
    /// is evicted (its process is killed via `kill_on_drop`).
    pub async fn get_client(&self, trace_path: &Path) -> Result<TraceProcessorClient> {
        let (canonical, fingerprint, shell_path) = resolve_trace_identity(trace_path)?;
        // Cache key stays canonical so two requests for the same trace
        // (regardless of which shell-friendly alias we pick) share an
        // instance. shell_path is what we hand the child process — usually
        // identical, but on Windows + non-ASCII we may rewrite it to the
        // 8.3 short name to dodge the ANSI-codepage argv mangling.
        let binary = self.ensure_binary().await?.to_path_buf();
        self.get_or_spawn_instance(canonical, fingerprint, move |port, _canonical| async move {
            TraceProcessorInstance::spawn(&binary, &shell_path, port, self.config).await
        })
        .await
    }

    async fn get_or_spawn_instance<F, Fut>(
        &self,
        canonical: PathBuf,
        fingerprint: TraceFileFingerprint,
        spawn: F,
    ) -> Result<TraceProcessorClient>
    where
        F: FnOnce(u16, PathBuf) -> Fut,
        Fut: std::future::Future<Output = Result<TraceProcessorInstance>>,
    {
        let path_lock = self.spawn_lock(canonical.clone()).await;
        let _path_guard = path_lock.lock().await;
        let result = self
            .get_or_spawn_instance_locked(canonical.clone(), fingerprint, spawn)
            .await;
        self.cleanup_spawn_lock(&canonical, &path_lock).await;
        result
    }

    async fn get_or_spawn_instance_locked<F, Fut>(
        &self,
        canonical: PathBuf,
        fingerprint: TraceFileFingerprint,
        spawn: F,
    ) -> Result<TraceProcessorClient>
    where
        F: FnOnce(u16, PathBuf) -> Fut,
        Fut: std::future::Future<Output = Result<TraceProcessorInstance>>,
    {
        if let Some(client) = self.cached_client(&canonical, &fingerprint).await? {
            return Ok(client);
        }

        let port = {
            let mut inner = self.inner.lock().await;
            allocate_next_port(&mut inner)?
        };

        let instance = spawn(port, canonical.clone()).await?;
        let client = instance.client.clone();

        let mut inner = self.inner.lock().await;
        if let Some(existing) = inner.instances.get(&canonical) {
            return Ok(existing.instance.client.clone());
        }
        inner.instances.put(
            canonical,
            CachedTraceProcessorInstance {
                instance,
                fingerprint,
            },
        );
        Ok(client)
    }

    async fn cached_client(
        &self,
        canonical: &Path,
        fingerprint: &TraceFileFingerprint,
    ) -> Result<Option<TraceProcessorClient>> {
        let mut inner = self.inner.lock().await;
        if let Some(cached) = inner.instances.get_mut(canonical) {
            if cached.fingerprint != *fingerprint {
                tracing::info!(
                    "trace file metadata changed for {}; respawning cached trace_processor_shell on port {} (old={:?}, new={:?})",
                    canonical.display(),
                    cached.instance.port,
                    cached.fingerprint,
                    fingerprint,
                );
                inner.instances.pop(canonical);
                return Ok(None);
            }

            match cached.instance.try_wait()? {
                None => return Ok(Some(cached.instance.client.clone())),
                Some(status) => {
                    tracing::warn!(
                        "trace_processor_shell on port {} exited with {status}; respawning{}",
                        cached.instance.port,
                        format_stderr_tail(&cached.instance.stderr_tail),
                    );
                    inner.instances.pop(canonical);
                }
            }
        }
        Ok(None)
    }

    async fn spawn_lock(&self, canonical: PathBuf) -> Arc<Mutex<()>> {
        let mut locks = self.spawn_locks.lock().await;
        locks
            .entry(canonical)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn cleanup_spawn_lock(&self, canonical: &Path, path_lock: &Arc<Mutex<()>>) {
        let mut locks = self.spawn_locks.lock().await;
        if locks.get(canonical).is_some_and(|existing| {
            Arc::ptr_eq(existing, path_lock) && Arc::strong_count(existing) == 2
        }) {
            locks.remove(canonical);
        }
    }
}

fn resolve_trace_identity(path: &Path) -> Result<(PathBuf, TraceFileFingerprint, PathBuf)> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("trace file not found: {}", path.display()))?;
    let fingerprint = TraceFileFingerprint::from_path(&canonical)?;
    let shell_path = resolve_trace_path_for_shell(&canonical)?;
    Ok((canonical, fingerprint, shell_path))
}

fn allocate_next_port(inner: &mut ManagerInner) -> Result<u16> {
    allocate_next_port_with_probe(inner, preflight_port_free)
}

/// Sweep the full port range starting from `inner.next_port`, returning the
/// first port where `probe` succeeds. Bails if the entire range is occupied
/// — the alternative (returning a port we just rejected) would defer the
/// same failure into `wait_ready` and report it as a confusing bind error.
fn allocate_next_port_with_probe<F>(inner: &mut ManagerInner, mut probe: F) -> Result<u16>
where
    F: FnMut(u16) -> bool,
{
    let sweep = u16::MAX as u32 - inner.starting_port as u32 + 1;
    for _ in 0..sweep {
        let port = advance_next_port(inner);
        if probe(port) {
            return Ok(port);
        }
    }
    bail!("no free port found in {}..=65535", inner.starting_port)
}

fn advance_next_port(inner: &mut ManagerInner) -> u16 {
    let port = inner.next_port;
    inner.next_port = inner.next_port.wrapping_add(1);
    if inner.next_port < inner.starting_port {
        inner.next_port = inner.starting_port;
    }
    port
}

/// Best-effort probe: if we can bind the port right now, it is (probably)
/// free. The listener is dropped immediately, leaving a microsecond-wide
/// TOCTOU window before the child spawns — small enough for our purposes.
fn preflight_port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn spawn_output_drains(
    stderr: tokio::process::ChildStderr,
    stdout: tokio::process::ChildStdout,
    port: u16,
) -> (watch::Receiver<StartupState>, SharedStderrTail) {
    let (startup_tx, startup_rx) = watch::channel(StartupState::Waiting);
    let tail = Arc::new(StdMutex::new(std::collections::VecDeque::with_capacity(
        STDERR_TAIL_CAPACITY,
    )));
    spawn_output_drain(
        stderr,
        port,
        Arc::clone(&tail),
        "stderr",
        "",
        Some(startup_tx),
    );
    spawn_output_drain(stdout, port, Arc::clone(&tail), "stdout", "[stdout] ", None);
    (startup_rx, tail)
}

/// Drain one output stream into the shared tail. `line_prefix` is prepended
/// in stored output (so `format_stderr_tail` can attribute lines back to
/// stderr vs stdout). `startup_tx` is `Some` only for stderr — stdout never
/// carries the readiness banner.
///
/// Lossy decode keeps the drain alive when trace_processor_shell emits
/// non-UTF-8 bytes (Windows ANSI codepage error text). ASCII tokens —
/// errno codes, ports, paths — survive intact, so startup needle matching
/// and LLM diagnostics keep working on any locale.
fn spawn_output_drain<R>(
    reader: R,
    port: u16,
    tail: SharedStderrTail,
    stream_label: &'static str,
    line_prefix: &'static str,
    mut startup_tx: Option<watch::Sender<StartupState>>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let needles = StartupNeedles::for_port(port);
        let mut startup_state = StartupLogState::default();
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::with_capacity(256);

        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    let line = decode_output_line(&buf);
                    tracing::info!("trace_processor_shell[{port}] {stream_label}: {line}");

                    if let Some(tx) = startup_tx.as_ref() {
                        if let Some(next_state) =
                            update_startup_state(&mut startup_state, &needles, &line)
                        {
                            let _ = tx.send(next_state);
                            startup_tx = None;
                        }
                    }

                    let stored = if line_prefix.is_empty() {
                        line
                    } else {
                        format!("{line_prefix}{line}")
                    };
                    push_stderr_line(&tail, stored);
                }
                Err(err) => {
                    tracing::warn!(
                        "failed reading trace_processor_shell[{port}] {stream_label}: {err}",
                    );
                    push_stderr_line(
                        &tail,
                        format!("{line_prefix}{stream_label} read error: {err}"),
                    );
                    break;
                }
            }
        }
    });
}

fn decode_output_line(buf: &[u8]) -> String {
    let trimmed = match buf.last() {
        Some(b'\n') => {
            let body = &buf[..buf.len() - 1];
            if body.last() == Some(&b'\r') {
                &body[..body.len() - 1]
            } else {
                body
            }
        }
        _ => buf,
    };
    String::from_utf8_lossy(trimmed).into_owned()
}

struct StartupNeedles {
    ipv4_start: String,
    ipv4_bound: String,
}

impl StartupNeedles {
    fn for_port(port: u16) -> Self {
        Self {
            ipv4_start: format!("[HTTP] Starting HTTP server on 127.0.0.1:{port}"),
            ipv4_bound: format!("127.0.0.1:{port}"),
        }
    }
}

fn update_startup_state(
    state: &mut StartupLogState,
    needles: &StartupNeedles,
    line: &str,
) -> Option<StartupState> {
    if line.contains(&needles.ipv4_start) {
        state.saw_ipv4_start = true;
    }

    if line.contains("Failed to listen on IPv4 socket") && line.contains(&needles.ipv4_bound) {
        state.saw_ipv4_bind_failure = true;
        return Some(StartupState::Ipv4BindFailed(line.to_owned()));
    }

    if state.saw_ipv4_bind_failure {
        return None;
    }

    if state.saw_ipv4_start && line.contains("[HTTP] This server can be used") {
        return Some(StartupState::Ready);
    }

    None
}

fn status_matches_expected_trace(status: &StatusResult, expected_trace: &Path) -> bool {
    let Some(loaded_trace_name) = status.loaded_trace_name.as_deref() else {
        // Some trace_processor_shell builds leave `loaded_trace_name` unset.
        // The allocator preflights every port right before spawn, so a
        // successful /status response on a port that was free microseconds
        // ago is almost certainly from our child. Trust it.
        return true;
    };
    loaded_name_matches(loaded_trace_name, expected_trace)
}

/// Does `/status`'s `loaded_trace_name` refer to the trace at `expected`?
/// Lossy-decodes the bytes (trace_processor echoes argv as raw OEM-codepage
/// bytes; on CJK locales with non-ASCII path components those bytes are
/// invalid UTF-8), strips Perfetto's `" (NN MB)"` size annotation,
/// normalizes `\` → `/`, and matches on full path, bare filename, or
/// `/<filename>` suffix.
pub(crate) fn loaded_name_matches(loaded: &[u8], expected: &Path) -> bool {
    let loaded_lossy = String::from_utf8_lossy(loaded);
    let loaded_norm = normalize_status_path(strip_size_suffix(&loaded_lossy));
    let expected_norm = normalize_status_path(&expected.to_string_lossy());
    if loaded_norm == expected_norm {
        return true;
    }
    expected
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            // Bare filename, or full path ending in `/<filename>` — the
            // latter rescues CJK paths whose directory components came
            // back mojibake'd from `String::from_utf8_lossy` but whose
            // ASCII basename survived intact. Literal `/` separator
            // prevents `foo.tracebin/round13_2_trace.bin` style collisions.
            loaded_norm == name
                || (loaded_norm.ends_with(name)
                    && loaded_norm[..loaded_norm.len() - name.len()].ends_with('/'))
        })
}

fn normalize_status_path(path: &str) -> String {
    path.replace('\\', "/")
}

/// Strip Perfetto's trailing `" (NN MB)"` size annotation so exact
/// equality works against our canonical trace path.
pub(crate) fn strip_size_suffix(loaded: &str) -> &str {
    if !loaded.ends_with(')') {
        return loaded;
    }
    match loaded.rfind(" (") {
        Some(idx) => &loaded[..idx],
        None => loaded,
    }
}

/// Pick a path for `trace_processor_shell.exe`'s argv that survives its
/// legacy `int main(int, char*[])` entry point.
///
/// trace_processor's argv is decoded via the active ANSI codepage
/// (CP_ACP); chars outside it become `?` and fopen fails. The property
/// we need is "round-trips through CP_ACP", not "is ASCII" — on a cp936
/// system, `…\低端机traces\…` round-trips fine and works as-is. The
/// `\\?\` prefix from `canonicalize` is stripped because trace_processor
/// doesn't accept it.
///
/// On Unix this is a pass-through: argv is byte-exact.
fn resolve_trace_path_for_shell(canonical: &Path) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let stripped = strip_verbatim_prefix(canonical);
        if windows_cp_acp_lossless(&stripped) {
            return Ok(stripped);
        }
        if let Some(short) = windows_short_path(&stripped) {
            if windows_cp_acp_lossless(&short) {
                tracing::info!(
                    "rewrote trace path {} -> 8.3 short name {} for trace_processor_shell argv",
                    stripped.display(),
                    short.display(),
                );
                return Ok(short);
            }
        }
        bail!(
            "trace_processor_shell on Windows cannot read this path: it \
             contains characters outside the active ANSI codepage, and \
             8.3 short-name fallback is also unavailable.\n\
             Path: {}\n\n\
             Workarounds:\n\
             - Move/copy the trace to an ASCII-only path, e.g. \
               `Copy-Item <src> C:\\traces\\my.trace`\n\
             - Re-enable 8.3 names: `fsutil 8dot3name set <volume> 0` \
               (admin) — only affects directories created after the change\n\
             - Pass an existing 8.3 short name from `cmd /c dir /x \
               \"<parent>\"`",
            stripped.display(),
        );
    }
    #[cfg(not(windows))]
    {
        Ok(canonical.to_path_buf())
    }
}

/// Strip Windows verbatim prefix; UNC form preserved.
#[cfg(windows)]
fn strip_verbatim_prefix(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    p.to_path_buf()
}

/// True iff the path round-trips through CP_ACP without best-fit
/// substitution. Silent best-fit (e.g. fullwidth `Ａ` U+FF21 → `A`,
/// em-dash U+2014 → `-` on cp1252) counts as failure: the substituted
/// string names a file that doesn't exist on disk.
#[cfg(windows)]
fn windows_cp_acp_lossless(p: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;

    let wide: Vec<u16> = p.as_os_str().encode_wide().collect();
    if wide.is_empty() {
        return true;
    }

    let mut used_default: i32 = 0;
    let written = unsafe {
        win32::WideCharToMultiByte(
            win32::CP_ACP,
            win32::WC_NO_BEST_FIT_CHARS,
            wide.as_ptr(),
            wide.len() as i32,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            &mut used_default,
        )
    };
    written > 0 && used_default == 0
}

/// Wrap `GetShortPathNameW`. `None` if the path has no short alias
/// (8.3 disabled on volume, missing component, etc.).
#[cfg(windows)]
fn windows_short_path(long_path: &Path) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let wide: Vec<u16> = long_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // Probe required buffer size (return value includes the NUL).
    let required = unsafe { win32::GetShortPathNameW(wide.as_ptr(), std::ptr::null_mut(), 0) };
    if required == 0 {
        return None;
    }

    let mut buf: Vec<u16> = vec![0; required as usize];
    let written = unsafe { win32::GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), required) };
    if written == 0 || written >= required {
        return None;
    }
    buf.truncate(written as usize);
    Some(PathBuf::from(OsString::from_wide(&buf)))
}

#[cfg(windows)]
mod win32 {
    pub const CP_ACP: u32 = 0;
    pub const WC_NO_BEST_FIT_CHARS: u32 = 0x00000400;

    extern "system" {
        pub fn WideCharToMultiByte(
            code_page: u32,
            flags: u32,
            wide_char_str: *const u16,
            wide_char_len: i32,
            multi_byte_str: *mut u8,
            multi_byte_len: i32,
            default_char: *const u8,
            used_default_char: *mut i32,
        ) -> i32;

        pub fn GetShortPathNameW(
            lpsz_long_path: *const u16,
            lpsz_short_path: *mut u16,
            cch_buffer: u32,
        ) -> u32;
    }
}

fn push_stderr_line(stderr_tail: &SharedStderrTail, line: String) {
    let mut tail = stderr_tail.lock().expect("stderr tail poisoned");
    if tail.len() == STDERR_TAIL_CAPACITY {
        tail.pop_front();
    }
    tail.push_back(line);
}

fn format_stderr_tail(stderr_tail: &SharedStderrTail) -> String {
    let tail = stderr_tail.lock().expect("stderr tail poisoned");
    if tail.is_empty() {
        String::new()
    } else {
        let body = tail
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\noutput tail (stderr + stdout, last {} lines):\n{body}",
            tail.len(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::process::Command as TokioCommand;

    #[test]
    fn lru_evicts_oldest_when_full() {
        let mut cache: LruCache<String, u16> = LruCache::new(NonZeroUsize::new(2).unwrap());
        cache.put("a".into(), 1);
        cache.put("b".into(), 2);
        cache.put("c".into(), 3);

        assert!(cache.get(&"a".to_string()).is_none(), "a should be evicted");
        assert!(cache.get(&"b".to_string()).is_some());
        assert!(cache.get(&"c".to_string()).is_some());
    }

    #[test]
    fn lru_access_refreshes_entry() {
        let mut cache: LruCache<String, u16> = LruCache::new(NonZeroUsize::new(2).unwrap());
        cache.put("a".into(), 1);
        cache.put("b".into(), 2);
        // Access "a" to refresh it.
        let _ = cache.get(&"a".to_string());
        // Insert "c" — should evict "b" (oldest unreferenced).
        cache.put("c".into(), 3);

        assert!(cache.get(&"a".to_string()).is_some(), "a was refreshed");
        assert!(cache.get(&"b".to_string()).is_none(), "b should be evicted");
        assert!(cache.get(&"c".to_string()).is_some());
    }

    #[test]
    fn allocate_next_port_wraps_back_to_starting_port() {
        let mut inner = ManagerInner {
            instances: LruCache::new(NonZeroUsize::new(1).unwrap()),
            next_port: u16::MAX,
            starting_port: 19_001,
        };

        assert_eq!(
            allocate_next_port_with_probe(&mut inner, |_| true).unwrap(),
            u16::MAX,
        );
        assert_eq!(inner.next_port, 19_001);
    }

    #[test]
    fn allocate_next_port_skips_occupied_ports_via_probe() {
        let mut inner = ManagerInner {
            instances: LruCache::new(NonZeroUsize::new(1).unwrap()),
            next_port: 20_000,
            starting_port: 20_000,
        };

        let occupied = [20_000u16, 20_001u16];
        let probe = |port: u16| !occupied.contains(&port);
        assert_eq!(
            allocate_next_port_with_probe(&mut inner, probe).unwrap(),
            20_002
        );
        assert_eq!(inner.next_port, 20_003);
    }

    #[test]
    fn allocate_next_port_bails_when_all_probes_fail() {
        // starting_port near u16::MAX keeps the full-range sweep cheap in test.
        let mut inner = ManagerInner {
            instances: LruCache::new(NonZeroUsize::new(1).unwrap()),
            next_port: 65_530,
            starting_port: 65_530,
        };

        let err = allocate_next_port_with_probe(&mut inner, |_| false)
            .expect_err("exhausted sweep must surface a clear error");
        assert!(
            err.to_string().contains("no free port"),
            "error message should explain the exhaustion, got: {err}",
        );
    }

    #[test]
    fn preflight_port_free_rejects_real_bound_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral listener");
        let port = listener.local_addr().expect("local addr").port();
        assert!(
            !preflight_port_free(port),
            "actively bound port {port} must probe as occupied",
        );
    }

    #[test]
    fn allocate_next_port_skips_real_bound_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral listener");
        let bound_port = listener.local_addr().expect("local addr").port();

        let mut inner = ManagerInner {
            instances: LruCache::new(NonZeroUsize::new(1).unwrap()),
            next_port: bound_port,
            starting_port: bound_port,
        };
        let allocated = allocate_next_port_with_probe(&mut inner, preflight_port_free)
            .expect("allocator should sweep past a real bound port");
        assert_ne!(
            allocated, bound_port,
            "allocator must not hand out a port that is actively bound",
        );
    }

    #[test]
    fn status_matches_rejects_suffix_collision() {
        let status = status_result(Some("/tmp/foo.perfetto-trace.1 (0 MB)"));
        let expected = PathBuf::from("/tmp/foo.perfetto-trace");
        assert!(
            !status_matches_expected_trace(&status, &expected),
            "a longer trace name must not match a shorter expected path",
        );
    }

    #[test]
    fn status_matches_accepts_missing_name() {
        let status = status_result(None);
        let expected = PathBuf::from("/tmp/foo.perfetto-trace");
        assert!(status_matches_expected_trace(&status, &expected));
    }

    #[test]
    fn status_matches_accepts_bare_basename() {
        let status = status_result(Some("foo.perfetto-trace (0 MB)"));
        let expected = PathBuf::from("/abs/path/to/foo.perfetto-trace");
        assert!(status_matches_expected_trace(&status, &expected));
    }

    /// Regression for v0.8.7 → v0.8.8: trace_processor on a cp936 host
    /// returns `loaded_trace_name` as **raw cp936 bytes** inside the
    /// protobuf field. As `optional string` (v0.8.7) prost rejected those
    /// bytes as invalid UTF-8 and the entire `decode()` returned `Err` —
    /// `wait_ready` then never saw `is_ok()` and timed out. As `optional
    /// bytes` (v0.8.8) decode must succeed and surface the raw bytes.
    /// Locks the schema so a future revert to `string` would fail CI.
    #[test]
    fn status_result_decodes_when_loaded_trace_name_has_invalid_utf8() {
        use prost::Message;
        // Wire bytes captured from a real cp936 Win11 host running
        // `trace_processor_shell -D --http-port 9099 "...低端机..."`:
        // tag=(field 1 << 3) | 2 = 0x0A, length-delimited; payload is
        // "C:/Users/admin3/Downloads/<低端机:cp936>traces/traces/round13_2_trace.bin (28 MB)".
        let payload = b"C:/Users/admin3/Downloads/\xb5\xcd\xb6\xcb\xbb\xfatraces/traces/round13_2_trace.bin (28 MB)";
        let mut wire = Vec::with_capacity(2 + payload.len());
        wire.push(0x0A);
        wire.push(payload.len() as u8); // varint OK for len < 128
        wire.extend_from_slice(payload);
        let status = StatusResult::decode(&wire[..])
            .expect("StatusResult must decode cp936 bytes after the optional bytes retype");
        let name = status
            .loaded_trace_name
            .expect("loaded_trace_name must round-trip through the wire");
        assert!(
            name.windows(6)
                .any(|w| w == [0xB5, 0xCD, 0xB6, 0xCB, 0xBB, 0xFA]),
            "raw cp936 bytes must survive intact, got {name:?}",
        );
    }

    #[test]
    fn strip_size_suffix_removes_trailing_annotation() {
        assert_eq!(
            strip_size_suffix("/tmp/trace.perfetto-trace (0 MB)"),
            "/tmp/trace.perfetto-trace",
        );
        assert_eq!(
            strip_size_suffix("/tmp/trace.perfetto-trace (123 MB)"),
            "/tmp/trace.perfetto-trace",
        );
    }

    #[test]
    fn strip_size_suffix_passes_through_when_no_annotation() {
        assert_eq!(
            strip_size_suffix("/tmp/trace.perfetto-trace"),
            "/tmp/trace.perfetto-trace",
        );
    }

    #[test]
    fn strip_size_suffix_uses_rightmost_boundary() {
        // A user's trace path can legitimately contain " (" — the rightmost
        // occurrence is the one Perfetto appended, so only strip that.
        assert_eq!(
            strip_size_suffix("/tmp/weird (old).perfetto-trace (0 MB)"),
            "/tmp/weird (old).perfetto-trace",
        );
    }

    #[test]
    fn strip_size_suffix_requires_trailing_paren() {
        // Has " (" but does not end in ")" — treat as no suffix.
        assert_eq!(strip_size_suffix("foo (oops"), "foo (oops");
    }

    #[test]
    fn new_with_starting_port_and_configs_wires_all_fields() {
        let tp_config = TraceProcessorConfig {
            startup_timeout: Duration::from_millis(4_321),
            request_timeout: Duration::from_millis(7_654),
        };
        let download_config =
            DownloadConfig::from_override(Some("https://mirror.example/tp".to_string()));

        let manager = TraceProcessorManager::new_with_starting_port_and_configs(
            5,
            19_500,
            tp_config,
            download_config,
        );

        assert_eq!(manager.config.startup_timeout, Duration::from_millis(4_321));
        assert_eq!(manager.config.request_timeout, Duration::from_millis(7_654));
        assert_eq!(
            manager.download_config.redacted_base_url(),
            "https://mirror.example/tp"
        );

        let inner = manager
            .inner
            .try_lock()
            .expect("freshly built manager is uncontended");
        assert_eq!(inner.starting_port, 19_500);
        assert_eq!(inner.next_port, 19_500);
        assert_eq!(inner.instances.cap().get(), 5);
    }

    #[tokio::test]
    async fn wait_ready_blocks_status_until_stderr_gate_opens() {
        let mut instance = fake_instance(19_111);
        let expected_trace = expected_trace_path(19_111);
        let expected_trace_for_status = expected_trace.clone();
        let (startup_tx, mut startup_rx) = watch::channel(StartupState::Waiting);
        let status_calls = Arc::new(AtomicUsize::new(0));
        let status_calls_task = Arc::clone(&status_calls);

        let wait = tokio::spawn(async move {
            instance
                .wait_ready_with_status(
                    &expected_trace,
                    &mut startup_rx,
                    Duration::from_millis(400),
                    move || {
                        let status_calls = Arc::clone(&status_calls_task);
                        let expected_trace = expected_trace_for_status.clone();
                        async move {
                            status_calls.fetch_add(1, Ordering::SeqCst);
                            Ok(status_for_trace(&expected_trace))
                        }
                    },
                )
                .await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            status_calls.load(Ordering::SeqCst),
            0,
            "status polling must not start before our stderr readiness marker",
        );

        startup_tx
            .send(StartupState::Ready)
            .expect("send startup ready");

        wait.await.expect("join wait_ready").expect("wait_ready ok");
        assert!(
            status_calls.load(Ordering::SeqCst) >= 1,
            "status should be polled once the readiness gate opens",
        );
    }

    #[tokio::test]
    async fn wait_ready_fails_on_ipv4_bind_error_without_polling_status() {
        let mut instance = fake_instance(19_112);
        let expected_trace = expected_trace_path(19_112);
        let expected_trace_for_status = expected_trace.clone();
        push_stderr_line(
            &instance.stderr_tail,
            "[HTTP] Starting HTTP server on 127.0.0.1:19112".to_owned(),
        );
        push_stderr_line(
            &instance.stderr_tail,
            "[HTTP] Failed to listen on IPv4 socket: \"127.0.0.1:19112\"".to_owned(),
        );
        let (_startup_tx, mut startup_rx) = watch::channel(StartupState::Ipv4BindFailed(
            "Failed to listen on IPv4 socket: \"127.0.0.1:19112\"".to_owned(),
        ));
        let status_calls = Arc::new(AtomicUsize::new(0));
        let status_calls_task = Arc::clone(&status_calls);

        let err = instance
            .wait_ready_with_status(
                &expected_trace,
                &mut startup_rx,
                Duration::from_millis(200),
                move || {
                    let status_calls = Arc::clone(&status_calls_task);
                    let expected_trace = expected_trace_for_status.clone();
                    async move {
                        status_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(status_for_trace(&expected_trace))
                    }
                },
            )
            .await
            .expect_err("bind failure must abort startup");

        assert_eq!(
            status_calls.load(Ordering::SeqCst),
            0,
            "foreign /status must not be consulted after our own bind failure",
        );
        assert!(
            err.to_string().contains("failed to bind 127.0.0.1:19112"),
            "error should surface the bind failure, got: {err}",
        );
    }

    #[tokio::test]
    async fn wait_ready_falls_back_to_status_for_unrecognized_external_binary_output() {
        let mut instance = fake_instance(19_114);
        let expected_trace = expected_trace_path(19_114);
        let expected_trace_for_status = expected_trace.clone();
        let (_startup_tx, mut startup_rx) = watch::channel(StartupState::Waiting);
        let status_calls = Arc::new(AtomicUsize::new(0));
        let status_calls_task = Arc::clone(&status_calls);

        instance
            .wait_ready_with_status(
                &expected_trace,
                &mut startup_rx,
                Duration::from_millis(1_500),
                move || {
                    let status_calls = Arc::clone(&status_calls_task);
                    let expected_trace = expected_trace_for_status.clone();
                    async move {
                        status_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(status_for_trace(&expected_trace))
                    }
                },
            )
            .await
            .expect("fallback /status should keep external binaries working");

        assert!(
            status_calls.load(Ordering::SeqCst) >= 2,
            "fallback path should require sustained /status health before succeeding",
        );
    }

    #[tokio::test]
    async fn wait_ready_fallback_stability_window_resets_on_failure() {
        let mut instance = fake_instance(19_116);
        let expected_trace = expected_trace_path(19_116);
        let expected_trace_for_status = expected_trace.clone();
        let (_startup_tx, mut startup_rx) = watch::channel(StartupState::Waiting);

        // Sequence of /status results once the fallback path opens.
        // The first Ok starts the stability timer; the Err that follows
        // must clear it so the timer cannot "carry over" pre-failure time.
        let results = Arc::new(StdMutex::new(std::collections::VecDeque::from([
            Ok(status_for_trace(&expected_trace)),
            Err(crate::error::PerfettoError::QueryError {
                kind: crate::error::QueryErrorKind::Other,
                message: "simulated transient /status failure".to_owned(),
            }),
            Ok(status_for_trace(&expected_trace)),
            Ok(status_for_trace(&expected_trace)),
            Ok(status_for_trace(&expected_trace)),
            Ok(status_for_trace(&expected_trace)),
            Ok(status_for_trace(&expected_trace)),
            Ok(status_for_trace(&expected_trace)),
        ])));
        let results_task = Arc::clone(&results);
        let ok_timestamps = Arc::new(StdMutex::new(Vec::<Instant>::new()));
        let ok_timestamps_task = Arc::clone(&ok_timestamps);

        let ready_at = Instant::now();
        instance
            .wait_ready_with_status(
                &expected_trace,
                &mut startup_rx,
                Duration::from_millis(2_500),
                move || {
                    let results = Arc::clone(&results_task);
                    let ok_timestamps = Arc::clone(&ok_timestamps_task);
                    let expected_trace = expected_trace_for_status.clone();
                    async move {
                        let next = results
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or_else(|| Ok(status_for_trace(&expected_trace)));
                        if next.is_ok() {
                            ok_timestamps.lock().unwrap().push(Instant::now());
                        }
                        next
                    }
                },
            )
            .await
            .expect("fallback must eventually succeed after stability re-accumulates");
        let total_elapsed = ready_at.elapsed();

        // The stability timer must not have latched onto the pre-failure
        // Ok: success requires STATUS_FALLBACK_STABILITY (300ms) of
        // *sustained* Ok after the failure, on top of the 500ms fallback
        // delay. Allow slack for the poll interval and scheduler jitter.
        assert!(
            total_elapsed
                >= STATUS_FALLBACK_DELAY + STATUS_FALLBACK_STABILITY - Duration::from_millis(50),
            "stability window must re-accumulate after a failure; elapsed={total_elapsed:?}",
        );

        // And the final Ok run must itself span the stability window,
        // meaning at least two post-failure Ok samples that are
        // STATUS_FALLBACK_STABILITY apart.
        let timestamps = ok_timestamps.lock().unwrap().clone();
        assert!(
            timestamps.len() >= 3,
            "expected at least pre-fail Ok + two post-fail Oks, got {}",
            timestamps.len(),
        );
        let post_fail = &timestamps[1..];
        let spanned = post_fail
            .last()
            .unwrap()
            .duration_since(*post_fail.first().unwrap());
        assert!(
            spanned >= STATUS_FALLBACK_STABILITY - Duration::from_millis(50),
            "post-failure Ok streak must span the stability window; spanned={spanned:?}",
        );
    }

    #[tokio::test]
    async fn wait_ready_fallback_rejects_foreign_trace_status() {
        let mut instance = fake_instance(19_117);
        let expected_trace = expected_trace_path(19_117);
        let foreign_trace = PathBuf::from("/tmp/foreign-trace.perfetto-trace");
        let (_startup_tx, mut startup_rx) = watch::channel(StartupState::Waiting);
        let status_calls = Arc::new(AtomicUsize::new(0));
        let status_calls_task = Arc::clone(&status_calls);

        let err = instance
            .wait_ready_with_status(
                &expected_trace,
                &mut startup_rx,
                Duration::from_millis(1_200),
                move || {
                    let status_calls = Arc::clone(&status_calls_task);
                    let foreign_trace = foreign_trace.clone();
                    async move {
                        status_calls.fetch_add(1, Ordering::SeqCst);
                        Ok(status_for_trace(&foreign_trace))
                    }
                },
            )
            .await
            .expect_err("fallback must not accept a foreign trace identity");

        assert!(
            err.to_string().contains("did not become ready"),
            "expected timeout for foreign trace identity, got: {err}",
        );
        assert!(
            status_calls.load(Ordering::SeqCst) >= 2,
            "fallback should have polled status repeatedly before timing out",
        );
    }

    #[tokio::test]
    async fn wait_ready_fallback_rejects_suffix_collision() {
        let mut instance = fake_instance(19_118);
        let expected_trace = PathBuf::from("/tmp/perfetto-test-19118.perfetto-trace");
        let collision = status_result(Some("/tmp/perfetto-test-19118.perfetto-trace.1 (0 MB)"));
        let (_startup_tx, mut startup_rx) = watch::channel(StartupState::Waiting);

        let err = instance
            .wait_ready_with_status(
                &expected_trace,
                &mut startup_rx,
                Duration::from_millis(1_200),
                move || {
                    let collision = collision.clone();
                    async move { Ok(collision) }
                },
            )
            .await
            .expect_err("suffix-extended foreign path must not satisfy identity");
        assert!(
            err.to_string().contains("did not become ready"),
            "expected timeout for suffix-collision status, got: {err}",
        );
    }

    #[tokio::test]
    async fn wait_ready_fallback_accepts_missing_trace_name() {
        let mut instance = fake_instance(19_119);
        let expected_trace = expected_trace_path(19_119);
        let (_startup_tx, mut startup_rx) = watch::channel(StartupState::Waiting);

        instance
            .wait_ready_with_status(
                &expected_trace,
                &mut startup_rx,
                Duration::from_millis(2_500),
                || async { Ok(status_result(None)) },
            )
            .await
            .expect("external binary with missing loaded_trace_name must pass");
    }

    #[tokio::test]
    async fn concurrent_same_trace_requests_only_spawn_once() {
        let manager = Arc::new(TraceProcessorManager::new(2));
        let canonical = PathBuf::from("/tmp/fake-trace.perfetto-trace");
        let spawn_count = Arc::new(AtomicUsize::new(0));

        let task1 = {
            let manager = Arc::clone(&manager);
            let canonical = canonical.clone();
            let spawn_count = Arc::clone(&spawn_count);
            tokio::spawn(async move {
                manager
                    .get_or_spawn_instance(
                        canonical,
                        test_fingerprint(1),
                        move |port, _| async move {
                            spawn_count.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            Ok(fake_instance(port))
                        },
                    )
                    .await
            })
        };

        let task2 = {
            let manager = Arc::clone(&manager);
            let canonical = canonical.clone();
            let spawn_count = Arc::clone(&spawn_count);
            tokio::spawn(async move {
                manager
                    .get_or_spawn_instance(
                        canonical,
                        test_fingerprint(1),
                        move |port, _| async move {
                            spawn_count.fetch_add(1, Ordering::SeqCst);
                            Ok(fake_instance(port))
                        },
                    )
                    .await
            })
        };

        task1.await.expect("join task1").expect("client1");
        task2.await.expect("join task2").expect("client2");

        assert_eq!(
            spawn_count.load(Ordering::SeqCst),
            1,
            "same trace should use a single in-flight spawn",
        );
    }

    #[tokio::test]
    async fn concurrent_different_trace_requests_spawn_independently() {
        let manager = Arc::new(TraceProcessorManager::new(4));
        let spawn_count = Arc::new(AtomicUsize::new(0));

        let task_a = {
            let manager = Arc::clone(&manager);
            let spawn_count = Arc::clone(&spawn_count);
            tokio::spawn(async move {
                manager
                    .get_or_spawn_instance(
                        PathBuf::from("/tmp/concurrent-a.perfetto-trace"),
                        test_fingerprint(1),
                        move |port, _| async move {
                            spawn_count.fetch_add(1, Ordering::SeqCst);
                            Ok(fake_instance(port))
                        },
                    )
                    .await
            })
        };

        let task_b = {
            let manager = Arc::clone(&manager);
            let spawn_count = Arc::clone(&spawn_count);
            tokio::spawn(async move {
                manager
                    .get_or_spawn_instance(
                        PathBuf::from("/tmp/concurrent-b.perfetto-trace"),
                        test_fingerprint(1),
                        move |port, _| async move {
                            spawn_count.fetch_add(1, Ordering::SeqCst);
                            Ok(fake_instance(port))
                        },
                    )
                    .await
            })
        };

        task_a.await.expect("join task_a").expect("client a");
        task_b.await.expect("join task_b").expect("client b");

        assert_eq!(
            spawn_count.load(Ordering::SeqCst),
            2,
            "different traces must spawn independently and not share locks",
        );
    }

    #[tokio::test]
    async fn manager_evicts_oldest_instance_when_capacity_exceeded() {
        let manager = Arc::new(TraceProcessorManager::new(2));
        let trace_a = PathBuf::from("/tmp/lru-a.perfetto-trace");
        let trace_b = PathBuf::from("/tmp/lru-b.perfetto-trace");
        let trace_c = PathBuf::from("/tmp/lru-c.perfetto-trace");
        let spawn_count = Arc::new(AtomicUsize::new(0));

        for path in [&trace_a, &trace_b, &trace_c] {
            let counter = Arc::clone(&spawn_count);
            manager
                .get_or_spawn_instance(
                    path.clone(),
                    test_fingerprint(1),
                    move |port, _| async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(fake_instance(port))
                    },
                )
                .await
                .expect("initial spawn");
        }
        assert_eq!(
            spawn_count.load(Ordering::SeqCst),
            3,
            "three distinct traces must trigger three spawns",
        );

        // trace_a was the LRU entry; re-fetching it must respawn.
        {
            let counter = Arc::clone(&spawn_count);
            manager
                .get_or_spawn_instance(trace_a, test_fingerprint(1), move |port, _| async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(fake_instance(port))
                })
                .await
                .expect("respawn evicted trace");
        }
        assert_eq!(
            spawn_count.load(Ordering::SeqCst),
            4,
            "evicted instance must respawn on next request",
        );

        // trace_c was inserted most recently and is still cached.
        {
            let counter = Arc::clone(&spawn_count);
            manager
                .get_or_spawn_instance(trace_c, test_fingerprint(1), move |port, _| async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(fake_instance(port))
                })
                .await
                .expect("cached client");
        }
        assert_eq!(
            spawn_count.load(Ordering::SeqCst),
            4,
            "cached instance must reuse, not respawn",
        );
    }

    #[tokio::test]
    async fn get_or_spawn_instance_respawns_when_trace_fingerprint_changes() {
        let manager = Arc::new(TraceProcessorManager::new(2));
        let canonical = PathBuf::from("/tmp/changed-trace.perfetto-trace");
        let spawn_count = Arc::new(AtomicUsize::new(0));

        {
            let counter = Arc::clone(&spawn_count);
            manager
                .get_or_spawn_instance(
                    canonical.clone(),
                    test_fingerprint(1),
                    move |port, _| async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(fake_instance(port))
                    },
                )
                .await
                .expect("initial spawn");
        }

        {
            let counter = Arc::clone(&spawn_count);
            manager
                .get_or_spawn_instance(canonical, test_fingerprint(2), move |port, _| async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(fake_instance(port))
                })
                .await
                .expect("respawn changed trace");
        }

        assert_eq!(
            spawn_count.load(Ordering::SeqCst),
            2,
            "same path with changed size/mtime fingerprint must respawn",
        );
    }

    #[tokio::test]
    async fn same_canonical_path_respawns_after_real_file_metadata_changes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let trace = tmp.path().join("same-name.perfetto-trace");
        let nested = tmp.path().join("nested");
        std::fs::create_dir(&nested).expect("create nested dir");
        std::fs::write(&trace, b"old").expect("write initial trace");

        let aliased_path = nested.join("..").join("same-name.perfetto-trace");
        let expected_canonical = trace.canonicalize().expect("canonical trace");
        let manager = Arc::new(TraceProcessorManager::new(2));
        let spawn_count = Arc::new(AtomicUsize::new(0));

        let (canonical, fingerprint, _shell_path) =
            resolve_trace_identity(&aliased_path).expect("resolve initial trace identity");
        let metadata = std::fs::metadata(&trace).expect("initial trace metadata");
        assert_eq!(canonical, expected_canonical);
        assert_eq!(fingerprint.size_bytes, metadata.len());
        assert_eq!(fingerprint.modified, metadata.modified().ok());

        {
            let counter = Arc::clone(&spawn_count);
            manager
                .get_or_spawn_instance(
                    canonical.clone(),
                    fingerprint.clone(),
                    move |port, observed_canonical| async move {
                        assert_eq!(observed_canonical, expected_canonical);
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(fake_instance(port))
                    },
                )
                .await
                .expect("initial spawn");
        }

        let (canonical_again, fingerprint_again, _shell_path) =
            resolve_trace_identity(&aliased_path).expect("resolve unchanged trace identity");
        assert_eq!(canonical_again, canonical);
        assert_eq!(fingerprint_again, fingerprint);
        {
            let counter = Arc::clone(&spawn_count);
            manager
                .get_or_spawn_instance(
                    canonical.clone(),
                    fingerprint_again,
                    move |port, _| async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(fake_instance(port))
                    },
                )
                .await
                .expect("unchanged cache hit");
        }
        assert_eq!(
            spawn_count.load(Ordering::SeqCst),
            1,
            "unchanged canonical path + metadata must reuse cached instance",
        );

        std::fs::write(&trace, b"new trace contents are larger").expect("overwrite trace");
        let (canonical_changed, changed_fingerprint, _shell_path) =
            resolve_trace_identity(&aliased_path).expect("resolve changed trace identity");
        assert_eq!(canonical_changed, canonical);
        assert_ne!(
            changed_fingerprint.size_bytes, fingerprint.size_bytes,
            "test setup must change file size",
        );
        assert_ne!(
            changed_fingerprint, fingerprint,
            "real size/mtime fingerprint must change after overwrite",
        );

        {
            let counter = Arc::clone(&spawn_count);
            manager
                .get_or_spawn_instance(
                    canonical_changed,
                    changed_fingerprint,
                    move |port, _| async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(fake_instance(port))
                    },
                )
                .await
                .expect("changed metadata respawn");
        }
        assert_eq!(
            spawn_count.load(Ordering::SeqCst),
            2,
            "same canonical path with changed real file metadata must respawn",
        );
    }

    #[tokio::test]
    async fn get_or_spawn_instance_recovers_after_process_death() {
        let manager = Arc::new(TraceProcessorManager::new(2));
        let canonical = PathBuf::from("/tmp/auto-recovery-trace.perfetto-trace");
        let spawn_count = Arc::new(AtomicUsize::new(0));

        // First spawn: insert a process that exits immediately.
        {
            let counter = Arc::clone(&spawn_count);
            manager
                .get_or_spawn_instance(
                    canonical.clone(),
                    test_fingerprint(1),
                    move |port, _| async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(fake_instance_with_process(port, spawn_quick_exit_process()))
                    },
                )
                .await
                .expect("initial spawn");
        }

        // Give the kernel a moment to reap the exited child so try_wait observes it.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Second call: cached_client must detect the dead process and respawn.
        {
            let counter = Arc::clone(&spawn_count);
            manager
                .get_or_spawn_instance(canonical, test_fingerprint(1), move |port, _| async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(fake_instance(port))
                })
                .await
                .expect("auto-recovery respawn");
        }

        assert_eq!(
            spawn_count.load(Ordering::SeqCst),
            2,
            "dead cached instance must trigger a respawn on next request",
        );
    }

    #[tokio::test]
    async fn get_client_returns_clear_error_for_missing_trace() {
        let manager = TraceProcessorManager::new_with_binary(
            PathBuf::from("/nonexistent/trace_processor_shell"),
            1,
        );
        let missing = PathBuf::from("/nonexistent/this-trace-does-not-exist.perfetto-trace");

        let err = manager
            .get_client(&missing)
            .await
            .expect_err("missing trace must error");
        let msg = err.to_string();
        assert!(
            msg.contains("trace file not found"),
            "error should call out the missing trace, got: {msg}",
        );
        assert!(
            msg.contains("this-trace-does-not-exist.perfetto-trace"),
            "error should include the trace path, got: {msg}",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn get_client_surfaces_spawn_error_for_non_executable_binary() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let non_exec = tmp.path().join("fake_tp_shell");
        std::fs::write(&non_exec, b"fake").expect("write fake binary");
        std::fs::set_permissions(&non_exec, std::fs::Permissions::from_mode(0o644))
            .expect("strip execute bit");

        let trace = tmp.path().join("fake.perfetto-trace");
        std::fs::write(&trace, b"not a real trace").expect("write trace placeholder");

        let manager = TraceProcessorManager::new_with_binary(non_exec.clone(), 1);
        let err = manager
            .get_client(&trace)
            .await
            .expect_err("non-executable binary must fail at spawn");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to spawn"),
            "error should surface the spawn-failure context, got: {msg}",
        );
        assert!(
            msg.contains("fake_tp_shell"),
            "error should include the binary name, got: {msg}",
        );
        assert!(
            msg.to_lowercase().contains("permission denied"),
            "error chain should surface the OS-level permission denial, got: {msg}",
        );
    }

    #[test]
    fn startup_parser_never_overrides_ipv4_bind_failure_with_ready() {
        let mut state = StartupLogState::default();
        let needles = StartupNeedles::for_port(19_113);

        assert_eq!(
            update_startup_state(
                &mut state,
                &needles,
                "[HTTP] Starting HTTP server on 127.0.0.1:19113",
            ),
            None,
        );
        assert_eq!(
            update_startup_state(
                &mut state,
                &needles,
                "Failed to listen on IPv4 socket: \"127.0.0.1:19113\" (errno: 98, Address already in use)",
            ),
            Some(StartupState::Ipv4BindFailed(
                "Failed to listen on IPv4 socket: \"127.0.0.1:19113\" (errno: 98, Address already in use)"
                    .to_owned(),
            )),
        );
        assert_eq!(
            update_startup_state(
                &mut state,
                &needles,
                "[HTTP] This server can be used by reloading https://ui.perfetto.dev",
            ),
            None,
            "ready banner must not erase a prior bind failure",
        );
    }

    #[tokio::test]
    async fn wait_ready_fails_if_process_exits_before_ready() {
        let mut instance = fake_instance_with_process(19_115, spawn_quick_exit_process());
        let expected_trace = expected_trace_path(19_115);
        let expected_trace_for_status = expected_trace.clone();
        let (_startup_tx, mut startup_rx) = watch::channel(StartupState::Waiting);

        let err = instance
            .wait_ready_with_status(
                &expected_trace,
                &mut startup_rx,
                Duration::from_millis(500),
                move || {
                    let expected_trace = expected_trace_for_status.clone();
                    async move { Ok(status_for_trace(&expected_trace)) }
                },
            )
            .await
            .expect_err("exited child must fail startup");

        assert!(
            err.to_string().contains("exited with"),
            "expected early-exit failure, got: {err}",
        );
    }

    fn spawn_hold_process() -> Child {
        // Windows `timeout` refuses redirected stdin; use `ping` as a tty-less sleep.
        #[cfg(windows)]
        let mut cmd = {
            let mut cmd = TokioCommand::new("ping");
            cmd.args(["-n", "31", "127.0.0.1"]);
            cmd
        };

        #[cfg(not(windows))]
        let mut cmd = {
            let mut cmd = TokioCommand::new("sleep");
            cmd.arg("30");
            cmd
        };

        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn hold process")
    }

    fn spawn_quick_exit_process() -> Child {
        #[cfg(windows)]
        let mut cmd = {
            let mut cmd = TokioCommand::new("cmd");
            cmd.args(["/C", "exit", "0"]);
            cmd
        };

        #[cfg(not(windows))]
        let mut cmd = TokioCommand::new("true");

        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn quick-exit process")
    }

    fn fake_instance(port: u16) -> TraceProcessorInstance {
        fake_instance_with_process(port, spawn_hold_process())
    }

    fn fake_instance_with_process(port: u16, process: Child) -> TraceProcessorInstance {
        TraceProcessorInstance {
            process,
            port,
            client: TraceProcessorClient::new(port, Duration::from_secs(1)),
            stderr_tail: Arc::new(StdMutex::new(std::collections::VecDeque::new())),
        }
    }

    fn test_fingerprint(version: u64) -> TraceFileFingerprint {
        TraceFileFingerprint {
            size_bytes: 42,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(version)),
        }
    }

    fn expected_trace_path(port: u16) -> PathBuf {
        PathBuf::from(format!("/tmp/test-trace-{port}.perfetto-trace"))
    }

    fn status_result(loaded_trace_name: Option<&str>) -> StatusResult {
        StatusResult {
            loaded_trace_name: loaded_trace_name.map(|s| s.as_bytes().to_vec()),
            human_readable_version: None,
            api_version: None,
            version_code: None,
        }
    }

    fn status_for_trace(trace_path: &Path) -> StatusResult {
        status_result(Some(&format!("{} (0 MB)", trace_path.display())))
    }

    #[test]
    fn decode_output_line_strips_lf_crlf_and_keeps_ascii() {
        assert_eq!(decode_output_line(b"hello"), "hello");
        assert_eq!(decode_output_line(b"hello\n"), "hello");
        assert_eq!(decode_output_line(b"hello\r\n"), "hello");
        // Bare \r is preserved (not a terminator on its own).
        assert_eq!(decode_output_line(b"a\rb\n"), "a\rb");
    }

    #[test]
    fn decode_output_line_replaces_invalid_utf8_with_replacement_char() {
        // GBK-encoded "你好" — the shape of payload Windows trace_processor_shell
        // emits via the active ANSI codepage. Must NOT poison the stream.
        let bytes = b"open failed: \xc4\xe3\xba\xc3\n";
        let decoded = decode_output_line(bytes);
        assert!(
            decoded.starts_with("open failed: "),
            "ASCII prefix must survive lossy decode, got: {decoded:?}",
        );
        assert!(
            decoded.contains('\u{FFFD}'),
            "invalid bytes must become U+FFFD, got: {decoded:?}",
        );
    }

    #[test]
    fn format_stderr_tail_labels_combined_streams() {
        let tail: SharedStderrTail =
            Arc::new(StdMutex::new(std::collections::VecDeque::from(vec![
                "[HTTP] starting".to_owned(),
                "[stdout] ready hint".to_owned(),
            ])));
        let formatted = format_stderr_tail(&tail);
        assert!(
            formatted.contains("stderr + stdout"),
            "label must signal both streams are folded in, got: {formatted}",
        );
        assert!(
            formatted.contains("[stdout] ready hint"),
            "stdout-tagged lines must surface, got: {formatted}",
        );
    }

    #[test]
    fn resolve_trace_path_for_shell_passes_through_ascii() {
        let p = PathBuf::from("/tmp/ascii-only.perfetto-trace");
        let resolved = resolve_trace_path_for_shell(&p).expect("ascii path must pass");
        assert_eq!(resolved, p, "ASCII path must be returned unchanged");
    }

    #[cfg(not(windows))]
    #[test]
    fn resolve_trace_path_for_shell_passes_through_non_ascii_on_unix() {
        let p = PathBuf::from("/tmp/低端机/trace.bin");
        let resolved = resolve_trace_path_for_shell(&p)
            .expect("non-ASCII path must pass on Unix (argv is byte-exact)");
        assert_eq!(resolved, p);
    }

    #[cfg(windows)]
    #[test]
    fn windows_cp_acp_lossless_accepts_ascii() {
        assert!(windows_cp_acp_lossless(Path::new(
            "C:\\Users\\test\\foo.trace"
        )));
        assert!(windows_cp_acp_lossless(Path::new("")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_cp_acp_lossless_rejects_chars_outside_codepage() {
        // On systems configured for UTF-8 ACP, every Unicode scalar can round
        // trip through CP_ACP, so there is no "outside the active codepage"
        // character to assert against.
        if windows_cp_acp_lossless(Path::new("\u{1F600}")) {
            return;
        }

        // U+1F600 is outside legacy Windows ANSI codepages.
        assert!(!windows_cp_acp_lossless(Path::new("\u{1F600}")));
        assert!(!windows_cp_acp_lossless(Path::new(
            "C:\\foo\\\u{1F600}.trace"
        )));
    }

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_prefix_handles_drive_letter_and_unc() {
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\C:\Users\foo")),
            PathBuf::from(r"C:\Users\foo"),
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\UNC\server\share\foo")),
            PathBuf::from(r"\\server\share\foo"),
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"C:\Users\foo")),
            PathBuf::from(r"C:\Users\foo"),
        );
    }

    #[cfg(windows)]
    #[test]
    fn resolve_trace_path_for_shell_yields_codepage_compatible_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("低端机traces");
        std::fs::create_dir_all(&dir).expect("create non-ASCII dir");
        let trace = dir.join("round13_2_trace.bin");
        std::fs::write(&trace, b"not a real trace").expect("write file");

        let canonical = trace.canonicalize().expect("canonicalize");
        let resolved = resolve_trace_path_for_shell(&canonical)
            .expect("a freshly-created non-ASCII dir must resolve via canonical or 8.3 short name");
        assert!(
            windows_cp_acp_lossless(&resolved),
            "resolver must return a CP_ACP-clean path, got: {}",
            resolved.display(),
        );
        let s = resolved.to_string_lossy();
        assert!(
            !s.starts_with(r"\\?\"),
            "verbatim prefix must be stripped, got: {s}",
        );
    }

    #[cfg(windows)]
    #[test]
    fn resolve_trace_path_for_shell_rejects_when_codepage_incompatible_and_no_short_name() {
        if windows_cp_acp_lossless(Path::new("\u{1F600}")) {
            return;
        }

        let p = PathBuf::from("C:\\Users\\nonexistent\\\u{1F600}\\trace.bin");
        let err = resolve_trace_path_for_shell(&p)
            .expect_err("unrepresentable non-existent path must surface an error");
        let msg = err.to_string();
        assert!(
            msg.contains("active ANSI codepage"),
            "error must explain the cause, got: {msg}",
        );
        assert!(
            msg.contains("ASCII-only path") || msg.contains("8dot3name"),
            "error must offer workarounds, got: {msg}",
        );
    }
}
