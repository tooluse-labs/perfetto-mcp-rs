// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

const TP_VERSION: &str = "v54.0";

pub const DEFAULT_ARTIFACTS_BASE_URL: &str =
    "https://commondatastorage.googleapis.com/perfetto-luci-artifacts";

#[cfg(windows)]
const BINARY_NAME: &str = "trace_processor_shell.exe";
#[cfg(not(windows))]
const BINARY_NAME: &str = "trace_processor_shell";

const MIN_EXPECTED_SIZE: u64 = 1_000_000;
const STALE_TEMP_MAX_AGE: Duration = Duration::from_secs(60 * 60);
/// Wall-clock ceiling for a single `download_binary` attempt. `read_timeout`
/// alone cannot stop a misbehaving mirror that drip-feeds bytes below the
/// per-read threshold forever, so we cap the entire download.
const DOWNLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub struct DownloadConfig {
    base_url: String,
}

impl DownloadConfig {
    pub fn from_override(override_url: Option<String>) -> Self {
        let base_url = override_url.unwrap_or_else(|| DEFAULT_ARTIFACTS_BASE_URL.to_string());
        Self { base_url }
    }

    /// Log-safe form of `base_url`: strips userinfo, query, and fragment so
    /// that presigned URLs (`?X-Amz-Signature=...`) and credentials
    /// (`https://user:pass@host`) cannot leak into stderr.
    pub fn redacted_base_url(&self) -> String {
        redact_url(&self.base_url)
    }

    #[cfg(test)]
    fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

fn redact_url(url: &str) -> String {
    // On parse failure we cannot reliably locate userinfo or query separators,
    // so replace the entire input rather than echo a possibly-credentialed
    // string into logs and error contexts.
    let Ok(mut parsed) = reqwest::Url::parse(url) else {
        return "<unparseable URL>".to_string();
    };
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.to_string()
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_ARTIFACTS_BASE_URL.to_string(),
        }
    }
}

/// Find or download trace_processor_shell.
///
/// Lookup order:
/// 1. `PERFETTO_TP_PATH` environment variable
/// 2. `trace_processor_shell` on `PATH`
/// 3. Cached download at `{data_local_dir}/perfetto-mcp-rs/<version>/trace_processor_shell`
/// 4. Download from the configured base URL (default: Perfetto LUCI artifacts)
pub async fn ensure_binary(config: &DownloadConfig) -> Result<PathBuf> {
    if let Ok(path) = std::env::var("PERFETTO_TP_PATH") {
        let p = PathBuf::from(&path);
        if p.exists() {
            return Ok(p);
        }
        bail!("PERFETTO_TP_PATH={path:?} does not exist");
    }

    if let Ok(path) = which::which("trace_processor_shell") {
        return Ok(path);
    }

    let cache_dir = cache_dir()?;
    let binary_path = cache_dir.join(BINARY_NAME);
    sweep_stale_temp_files(&cache_dir, STALE_TEMP_MAX_AGE);

    // No age gate on cache hit: TP_VERSION is pinned in source so a scheduled
    // refresh could never pull a newer build, the sidecar is a stricter
    // integrity check than mtime, and a periodic re-download would defeat
    // the offline heal path in the LegacyNoSidecar arm on air-gapped hosts.
    if binary_path.exists() {
        let bp = binary_path.clone();
        let outcome = tokio::task::spawn_blocking(move || verify_sidecar(&bp))
            .await
            .context("sidecar verify task panicked")?;
        match outcome {
            VerifyOutcome::Verified => return Ok(binary_path),
            VerifyOutcome::LegacyNoSidecar => {
                // Tradeoff: hash the existing bytes and write a sidecar from
                // them, rather than re-downloading to compare against upstream.
                // This lets air-gapped hosts upgrade from a pre-sidecar release
                // without network, but it blesses whatever is already on disk —
                // pre-existing bit rot on first upgrade is undetectable here.
                // Subsequent runs still catch corruption via the sidecar.
                tracing::warn!("cached trace_processor_shell has no sidecar; healing in place");
                let bp = binary_path.clone();
                let heal = tokio::task::spawn_blocking(move || -> Result<()> {
                    let digest = hash_file(&bp)?;
                    write_sidecar_atomically(&bp, &digest)
                })
                .await
                .context("sidecar heal task panicked")?;
                match heal {
                    Ok(()) => return Ok(binary_path),
                    Err(e) => {
                        tracing::warn!("failed to heal legacy sidecar: {e}; re-downloading");
                    }
                }
            }
            VerifyOutcome::Mismatch => {
                tracing::warn!(
                    "cached trace_processor_shell failed sidecar verification; re-downloading"
                );
            }
        }
    }

    tokio::time::timeout(
        DOWNLOAD_TOTAL_TIMEOUT,
        download_binary(config, &binary_path),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "trace_processor_shell download exceeded {DOWNLOAD_TOTAL_TIMEOUT:?} wall-clock deadline"
        )
    })??;
    Ok(binary_path)
}

/// Unversioned cache root `<data-local>/perfetto-mcp-rs`. Install/uninstall
/// subcommands clean this root so all historical `TP_VERSION` subdirs are
/// swept regardless of which binary runs the cleanup.
pub(crate) fn cache_root() -> Result<PathBuf> {
    let base = dirs::data_local_dir().context("cannot determine local data directory")?;
    Ok(base.join("perfetto-mcp-rs"))
}

fn cache_dir() -> Result<PathBuf> {
    Ok(cache_root()?.join(TP_VERSION))
}

#[derive(Debug, PartialEq, Eq)]
enum VerifyOutcome {
    Verified,
    LegacyNoSidecar,
    Mismatch,
}

fn sidecar_path(binary: &Path) -> PathBuf {
    let mut p = binary.as_os_str().to_owned();
    p.push(".sha256");
    PathBuf::from(p)
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).with_context(|| format!("hashing {}", path.display()))?;
    Ok(hex::encode(hasher.finalize()))
}

fn verify_sidecar(binary: &Path) -> VerifyOutcome {
    let sp = sidecar_path(binary);
    let expected = match std::fs::read_to_string(&sp) {
        Ok(s) => s.trim().to_ascii_lowercase(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return VerifyOutcome::LegacyNoSidecar;
        }
        Err(_) => return VerifyOutcome::Mismatch,
    };
    let Ok(actual) = hash_file(binary) else {
        return VerifyOutcome::Mismatch;
    };
    if actual == expected {
        VerifyOutcome::Verified
    } else {
        VerifyOutcome::Mismatch
    }
}

fn write_sidecar_atomically(binary: &Path, hex_digest: &str) -> Result<()> {
    let parent = binary
        .parent()
        .context("sidecar parent directory missing")?;
    let mut tmp = NamedTempFile::new_in(parent)?;
    writeln!(tmp.as_file_mut(), "{hex_digest}")?;
    tmp.as_file().sync_all()?;
    tmp.persist(sidecar_path(binary))
        .map_err(|e| anyhow::Error::new(e.error).context("persisting sidecar"))?;
    Ok(())
}

fn sweep_stale_temp_files(cache_dir: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        // `NamedTempFile::new_in` uses a `.tmp` prefix on the filename.
        if !name.starts_with(".tmp") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified.elapsed().unwrap_or_default() <= max_age {
            continue;
        }
        let path = entry.path();
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::debug!("sweep: failed to remove {}: {e}", path.display());
        } else {
            tracing::debug!("sweep: removed stale temp {}", path.display());
        }
    }
}

fn binary_url(config: &DownloadConfig, arch: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(&config.base_url).with_context(|| {
        format!(
            "parsing artifacts base URL {}",
            redact_url(&config.base_url)
        )
    })?;
    url.set_fragment(None);
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("artifacts base URL is not hierarchical"))?
        .pop_if_empty()
        .extend([TP_VERSION, arch, BINARY_NAME]);
    Ok(url.to_string())
}

const DOWNLOAD_MAX_RETRIES: u32 = 2;
const DOWNLOAD_RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
const STREAM_ERROR_CONTEXT_PREFIX: &str = "streaming from ";

/// Download trace_processor_shell atomically: stream into a
/// `NamedTempFile` co-located with the cache dir, hash on the fly, then
/// atomic-rename into place and write the sidecar.
///
/// Transient network failures (connect timeout, stream interruption, HTTP
/// 429/5xx) are retried up to `DOWNLOAD_MAX_RETRIES` times with exponential
/// backoff. Permanent client errors (400/401/403/404) and post-download
/// validation failures fail immediately.
async fn download_binary(config: &DownloadConfig, dest: &Path) -> Result<()> {
    let arch = platform_arch()?;
    let url = binary_url(config, arch)?;
    let redacted_url = redact_url(&url);

    tracing::info!("downloading trace_processor_shell {TP_VERSION} ({arch}) from {redacted_url}");

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(120))
        .build()?;

    let parent = dest
        .parent()
        .context("cache dir missing from binary path")?;
    tokio::fs::create_dir_all(parent).await?;

    let (tmp, hex_digest) =
        download_with_retry(&client, &url, &redacted_url, parent, DOWNLOAD_MAX_RETRIES).await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755))?;
    }

    persist_with_retry(tmp, dest).await?;
    write_sidecar_atomically(dest, &hex_digest)?;

    tracing::info!("saved trace_processor_shell to {}", dest.display());
    Ok(())
}

/// Attempt the HTTP fetch + stream loop, retrying on transient errors.
/// Returns the completed temp file and its SHA-256 hex digest on success.
async fn download_with_retry(
    client: &reqwest::Client,
    url: &str,
    redacted_url: &str,
    tmp_dir: &Path,
    max_retries: u32,
) -> Result<(NamedTempFile, String)> {
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 0..=max_retries {
        if attempt > 0 {
            let delay = DOWNLOAD_RETRY_BASE_DELAY * 2u32.saturating_pow(attempt - 1);
            tracing::warn!(
                "retrying download (attempt {}/{}) after {delay:?}",
                attempt + 1,
                max_retries + 1,
            );
            tokio::time::sleep(delay).await;
        }

        match download_once(client, url, redacted_url, tmp_dir).await {
            Ok(result) => return Ok(result),
            Err(e) => {
                if !is_transient_download_error(&e) {
                    return Err(e);
                }
                tracing::warn!("transient download error: {e:#}");
                last_err = Some(e);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("download failed after retries")))
}

/// Single download attempt: HTTP GET → stream chunks → hash → temp file.
async fn download_once(
    client: &reqwest::Client,
    url: &str,
    redacted_url: &str,
    tmp_dir: &Path,
) -> Result<(NamedTempFile, String)> {
    let resp = client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(reqwest::Error::without_url)
        .with_context(|| format!("fetching {redacted_url}"))?;

    if let Some(len) = resp.content_length() {
        if len < MIN_EXPECTED_SIZE {
            bail!(
                "unexpected Content-Length {len} from {redacted_url} \
                 (expected >={MIN_EXPECTED_SIZE} for trace_processor_shell)"
            );
        }
    }

    let mut tmp = NamedTempFile::new_in(tmp_dir)?;
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("{STREAM_ERROR_CONTEXT_PREFIX}{redacted_url}"))?;
        hasher.update(&chunk);
        total += chunk.len() as u64;
        tmp.as_file_mut().write_all(&chunk)?;
    }
    tmp.as_file().sync_all()?;

    if total < MIN_EXPECTED_SIZE {
        bail!("download from {redacted_url} too small: {total} bytes");
    }

    let hex_digest = hex::encode(hasher.finalize());
    Ok((tmp, hex_digest))
}

/// Classify whether a download error is transient (worth retrying) or
/// permanent (fail immediately). Permanent errors: 400, 401, 403, 404,
/// and post-download Content-Length/size validation failures.
fn is_transient_download_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}");

    if msg.contains("unexpected Content-Length") || msg.contains("too small") {
        return false;
    }

    // Stream I/O errors (connection reset, EOF) produced by download_once.
    // Must check before the reqwest downcast chain — body decode errors are
    // reqwest::Error variants where is_connect/is_timeout are both false,
    // so the chain loop would short-circuit to `false` if we checked later.
    if msg.contains(STREAM_ERROR_CONTEXT_PREFIX) {
        return true;
    }

    if let Some(reqwest_err) = err.downcast_ref::<reqwest::Error>() {
        if let Some(status) = reqwest_err.status() {
            return is_retryable_status(status);
        }
        return reqwest_err.is_connect() || reqwest_err.is_timeout();
    }

    for cause in err.chain() {
        if let Some(reqwest_err) = cause.downcast_ref::<reqwest::Error>() {
            if let Some(status) = reqwest_err.status() {
                return is_retryable_status(status);
            }
            return reqwest_err.is_connect() || reqwest_err.is_timeout();
        }
    }

    false
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Atomic rename with a small retry loop for Windows, where antivirus can
/// briefly hold a read handle on a freshly written executable and cause
/// `MoveFileExW` to return `ERROR_ACCESS_DENIED`.
async fn persist_with_retry(mut tmp: NamedTempFile, dest: &Path) -> Result<()> {
    const MAX_ATTEMPTS: usize = 5;
    const BACKOFF: Duration = Duration::from_millis(100);
    for attempt in 1..=MAX_ATTEMPTS {
        match tmp.persist(dest) {
            Ok(_file) => return Ok(()),
            Err(e) => {
                let io_kind = e.error.kind();
                let retryable = io_kind == std::io::ErrorKind::PermissionDenied;
                if !retryable || attempt == MAX_ATTEMPTS {
                    return Err(anyhow::Error::new(e.error)
                        .context(format!("persisting download to {}", dest.display())));
                }
                tmp = e.file;
                tracing::debug!(
                    "persist retry {attempt}/{MAX_ATTEMPTS} after {io_kind:?}: {}",
                    dest.display()
                );
                tokio::time::sleep(BACKOFF).await;
            }
        }
    }
    unreachable!()
}

fn platform_arch() -> Result<&'static str> {
    let arch = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-amd64",
        ("linux", "aarch64") => "linux-arm64",
        ("macos", "x86_64") => "mac-amd64",
        ("macos", "aarch64") => "mac-arm64",
        ("windows", "x86_64") => "windows-amd64",
        (os, arch) => bail!("unsupported platform: {os}/{arch}"),
    };
    Ok(arch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread::sleep;

    #[test]
    fn cache_path_includes_version() {
        let dir = cache_dir().unwrap();
        let path_str = dir.to_string_lossy();
        assert!(
            path_str.contains(TP_VERSION),
            "cache path should contain version: {path_str}",
        );
    }

    #[test]
    fn platform_arch_returns_known() {
        let arch = platform_arch().unwrap();
        assert!([
            "linux-amd64",
            "linux-arm64",
            "mac-amd64",
            "mac-arm64",
            "windows-amd64"
        ]
        .contains(&arch),);
    }

    #[test]
    fn download_config_default_uses_upstream() {
        let cfg = DownloadConfig::default();
        let url = binary_url(&cfg, "linux-amd64").unwrap();
        assert!(url.starts_with(DEFAULT_ARTIFACTS_BASE_URL), "got: {url}");
    }

    #[test]
    fn download_config_from_override_none_uses_default() {
        let cfg = DownloadConfig::from_override(None);
        let url = binary_url(&cfg, "linux-amd64").unwrap();
        assert!(url.starts_with(DEFAULT_ARTIFACTS_BASE_URL), "got: {url}");
    }

    #[test]
    fn binary_url_uses_configured_base() {
        let cfg = DownloadConfig::new("https://mirror.example");
        let url = binary_url(&cfg, "linux-amd64").unwrap();
        assert!(url.contains("https://mirror.example"), "got: {url}");
        assert!(url.contains(TP_VERSION), "got: {url}");
        assert!(url.contains("linux-amd64"), "got: {url}");
        assert!(url.contains(BINARY_NAME), "got: {url}");
    }

    #[test]
    fn redact_strips_userinfo() {
        assert_eq!(
            redact_url("https://user:pass@mirror.corp/perfetto"),
            "https://mirror.corp/perfetto"
        );
    }

    #[test]
    fn redact_strips_query_and_fragment() {
        assert_eq!(
            redact_url("https://mirror.corp/tp?X-Amz-Signature=abc&expires=1#frag"),
            "https://mirror.corp/tp"
        );
    }

    #[test]
    fn redact_strips_userinfo_and_query_together() {
        assert_eq!(
            redact_url("https://u:p@host.example:8443/path?token=xyz"),
            "https://host.example:8443/path"
        );
    }

    #[test]
    fn redact_passthrough_on_clean_url() {
        assert_eq!(
            redact_url("https://commondatastorage.googleapis.com/perfetto-luci-artifacts"),
            "https://commondatastorage.googleapis.com/perfetto-luci-artifacts"
        );
    }

    #[test]
    fn redact_replaces_unparseable_with_placeholder() {
        // Inputs that fail to parse as a URL (no scheme, etc.) may still
        // carry credentials or tokens — replace wholesale rather than echo.
        assert_eq!(redact_url("not-a-url"), "<unparseable URL>");
        assert_eq!(
            redact_url("mirror.corp/tp?token=secret"),
            "<unparseable URL>"
        );
    }

    #[tokio::test]
    async fn download_binary_error_scrubs_query_token() {
        // Bind a localhost listener and close every incoming connection
        // immediately. This forces reqwest to error out with a Request-kind
        // error whose `.url` originally contains our fake token — if
        // `without_url()` is removed from the propagation path, the token
        // will leak into the anyhow error chain and this test will fail.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });

        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("trace_processor_shell");
        let cfg = DownloadConfig::new(format!("http://{addr}/perfetto?token=SECRET_DO_NOT_LEAK"));
        let err = download_binary(&cfg, &dest).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("SECRET_DO_NOT_LEAK"),
            "URL query token leaked in error chain: {msg}"
        );
    }

    #[tokio::test]
    async fn download_binary_surfaces_http_5xx_status() {
        // Exercises the `.error_for_status()` + URL-scrub branch the drop-connection test cannot reach.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 1];
                let _ = stream.read(&mut buf).await;
                let body = b"upstream unavailable";
                let headers = format!(
                    "HTTP/1.1 500 Internal Server Error\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\
                     \r\n",
                    body.len(),
                );
                let _ = stream.write_all(headers.as_bytes()).await;
                let _ = stream.write_all(body).await;
                let _ = stream.shutdown().await;
            }
        });

        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("trace_processor_shell");
        let cfg = DownloadConfig::new(format!(
            "http://{addr}/perfetto?token=HTTP500_SHOULD_NOT_LEAK"
        ));
        let err = download_binary(&cfg, &dest).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fetching"),
            "error should surface the fetch context, got: {msg}"
        );
        assert!(
            msg.contains("500"),
            "error should mention the upstream HTTP status, got: {msg}"
        );
        assert!(
            !msg.contains("HTTP500_SHOULD_NOT_LEAK"),
            "URL query token leaked in error chain: {msg}"
        );
        assert!(
            !dest.exists(),
            "a failed download must not leave the cached binary in place"
        );
    }

    #[test]
    fn binary_url_tolerates_trailing_slash_in_base() {
        let cfg = DownloadConfig::from_override(Some("https://mirror.example/".to_string()));
        let url = binary_url(&cfg, "linux-amd64").unwrap();
        assert!(!url.contains("//v54"), "double slash leaked: {url}");
    }

    #[test]
    fn binary_url_preserves_query_token() {
        let cfg = DownloadConfig::new("https://mirror.example/perfetto?token=abc");
        let url = binary_url(&cfg, "linux-amd64").unwrap();
        let (path, query) = url.split_once('?').expect("query preserved");
        assert_eq!(query, "token=abc");
        assert!(
            path.ends_with(&format!("/{TP_VERSION}/linux-amd64/{BINARY_NAME}")),
            "path segments inserted before query: {path}"
        );
        assert!(path.starts_with("https://mirror.example/perfetto/"));
    }

    #[test]
    fn binary_url_preserves_query_with_trailing_slash_in_path() {
        let cfg = DownloadConfig::new("https://mirror.example/perfetto/?token=abc");
        let url = binary_url(&cfg, "linux-amd64").unwrap();
        assert!(!url.contains("//v54"), "double slash leaked: {url}");
        assert!(url.ends_with("?token=abc"), "query not at end: {url}");
    }

    #[test]
    fn binary_url_strips_fragment() {
        let cfg = DownloadConfig::new("https://mirror.example/perfetto#anchor");
        let url = binary_url(&cfg, "linux-amd64").unwrap();
        assert!(!url.contains('#'), "fragment leaked: {url}");
    }

    #[test]
    fn binary_url_preserves_userinfo() {
        let cfg = DownloadConfig::new("https://user:pass@mirror.example/perfetto");
        let url = binary_url(&cfg, "linux-amd64").unwrap();
        assert!(url.starts_with("https://user:pass@mirror.example/perfetto/"));
    }

    #[test]
    fn sidecar_path_appends_suffix() {
        let p = sidecar_path(Path::new("/cache/trace_processor_shell"));
        assert_eq!(p, PathBuf::from("/cache/trace_processor_shell.sha256"));
    }

    fn digest_of(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        hex::encode(h.finalize())
    }

    #[test]
    fn sidecar_roundtrip_verifies_clean_file() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake_binary");
        let content = b"hello world binary payload";
        std::fs::write(&bin, content).unwrap();
        write_sidecar_atomically(&bin, &digest_of(content)).unwrap();
        assert_eq!(verify_sidecar(&bin), VerifyOutcome::Verified);
    }

    #[test]
    fn sidecar_detects_tampered_binary() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake_binary");
        let original = b"original bytes";
        std::fs::write(&bin, original).unwrap();
        write_sidecar_atomically(&bin, &digest_of(original)).unwrap();
        std::fs::write(&bin, b"tampered bytes!").unwrap();
        assert_eq!(verify_sidecar(&bin), VerifyOutcome::Mismatch);
    }

    #[test]
    fn sidecar_missing_returns_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake_binary");
        std::fs::write(&bin, b"legacy bytes").unwrap();
        assert_eq!(verify_sidecar(&bin), VerifyOutcome::LegacyNoSidecar);
    }

    #[test]
    fn legacy_cache_self_heals_by_hashing_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake_binary");
        let content = b"already downloaded, no sidecar";
        std::fs::write(&bin, content).unwrap();
        assert_eq!(verify_sidecar(&bin), VerifyOutcome::LegacyNoSidecar);

        let digest = hash_file(&bin).unwrap();
        write_sidecar_atomically(&bin, &digest).unwrap();

        assert_eq!(verify_sidecar(&bin), VerifyOutcome::Verified);
    }

    #[test]
    fn hash_file_matches_inline_digest() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("payload");
        let content = b"hash me";
        std::fs::write(&bin, content).unwrap();
        assert_eq!(hash_file(&bin).unwrap(), digest_of(content));
    }

    #[test]
    fn write_sidecar_atomically_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake_binary");
        std::fs::write(&bin, b"x").unwrap();
        write_sidecar_atomically(&bin, "deadbeef").unwrap();
        let contents = std::fs::read_to_string(sidecar_path(&bin)).unwrap();
        assert_eq!(contents.trim(), "deadbeef");
    }

    #[test]
    fn sweep_stale_temp_files_removes_old_tmp() {
        let dir = tempfile::tempdir().unwrap();
        // NamedTempFile uses a `.tmp` prefix by default.
        let tmp = NamedTempFile::new_in(dir.path()).unwrap();
        let tmp_path = tmp.path().to_path_buf();
        // Keep the file on disk after the guard drops.
        let (_file, persisted) = tmp.keep().unwrap();
        assert_eq!(persisted, tmp_path);
        assert!(persisted.exists(), "precondition: temp file exists");

        // Sleep a hair so `elapsed()` is strictly positive, then sweep with
        // a sub-ms `max_age` to mark everything stale.
        sleep(Duration::from_millis(5));
        sweep_stale_temp_files(dir.path(), Duration::from_millis(1));

        assert!(!persisted.exists(), "sweep did not remove stale temp file");
    }

    #[test]
    fn sweep_stale_temp_files_keeps_young_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = NamedTempFile::new_in(dir.path()).unwrap();
        let tmp_path = tmp.path().to_path_buf();
        let (_file, persisted) = tmp.keep().unwrap();
        assert_eq!(persisted, tmp_path);

        // max_age well into the future — nothing should be swept.
        sweep_stale_temp_files(dir.path(), Duration::from_secs(3600));

        assert!(persisted.exists(), "sweep removed a young temp file");
    }

    #[test]
    fn sweep_stale_temp_files_ignores_non_tmp_entries() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("trace_processor_shell");
        std::fs::write(&keep, b"x").unwrap();

        sleep(Duration::from_millis(5));
        sweep_stale_temp_files(dir.path(), Duration::from_millis(1));

        assert!(keep.exists(), "sweep removed a non-tmp file");
    }

    #[test]
    fn is_transient_classifies_stream_error_as_retryable() {
        // The classifier recognizes STREAM_ERROR_CONTEXT_PREFIX (injected by
        // download_once) as a transient I/O error (connection reset, EOF, etc.).
        let err = anyhow::anyhow!("error streaming from https://example.com: connection reset");
        assert!(
            is_transient_download_error(&err),
            "stream errors should be transient"
        );
    }

    #[test]
    fn is_transient_classifies_stream_context_over_io_error_as_retryable() {
        // Regression: download_once wraps chunk errors as:
        //   anyhow context("streaming from ...") -> inner error
        // The STREAM_ERROR_CONTEXT_PREFIX check must fire before the reqwest
        // downcast chain to avoid short-circuiting on false for body errors.
        let inner = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset");
        let err = anyhow::Error::from(inner)
            .context(format!("{STREAM_ERROR_CONTEXT_PREFIX}https://example.com"));
        assert!(
            is_transient_download_error(&err),
            "stream error wrapping an I/O cause must be transient"
        );
    }

    #[test]
    fn is_transient_rejects_content_length_error() {
        let err = anyhow::anyhow!(
            "unexpected Content-Length 42 from https://example.com \
             (expected >=1000000 for trace_processor_shell)"
        );
        assert!(
            !is_transient_download_error(&err),
            "Content-Length validation failure must not retry"
        );
    }

    #[test]
    fn is_transient_rejects_too_small_error() {
        let err = anyhow::anyhow!("download from https://example.com too small: 500 bytes");
        assert!(
            !is_transient_download_error(&err),
            "too-small validation failure must not retry"
        );
    }

    #[test]
    fn is_retryable_status_accepts_429_and_5xx() {
        assert!(is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(is_retryable_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        ));
        assert!(is_retryable_status(reqwest::StatusCode::GATEWAY_TIMEOUT));
    }

    #[test]
    fn is_retryable_status_rejects_4xx() {
        assert!(!is_retryable_status(reqwest::StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(reqwest::StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(reqwest::StatusCode::FORBIDDEN));
        assert!(!is_retryable_status(reqwest::StatusCode::NOT_FOUND));
    }

    // The 503 is caught by error_for_status() (classified as retryable 5xx),
    // not by the post-download size validator — the response body never reaches
    // the streaming loop on non-2xx status codes.
    #[tokio::test]
    async fn download_with_retry_succeeds_after_transient_failure() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let attempt_count = Arc::new(AtomicU32::new(0));
        let attempt_count_server = Arc::clone(&attempt_count);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 512];
                let _ = stream.read(&mut buf).await;
                let n = attempt_count_server.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    // First attempt: return 503
                    let body = b"service unavailable";
                    let headers = format!(
                        "HTTP/1.1 503 Service Unavailable\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\
                         \r\n",
                        body.len(),
                    );
                    let _ = stream.write_all(headers.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                } else {
                    // Subsequent: return 200 with a valid-size body
                    let payload = vec![0x42u8; MIN_EXPECTED_SIZE as usize];
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\
                         \r\n",
                        payload.len(),
                    );
                    let _ = stream.write_all(headers.as_bytes()).await;
                    let _ = stream.write_all(&payload).await;
                }
                let _ = stream.shutdown().await;
            }
        });

        let tmp = tempfile::tempdir().unwrap();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://{addr}/binary");
        let redacted = redact_url(&url);

        let result = download_with_retry(&client, &url, &redacted, tmp.path(), 2).await;

        assert!(result.is_ok(), "should succeed on retry, got: {result:?}");
        assert_eq!(
            attempt_count.load(Ordering::SeqCst),
            2,
            "should have made exactly 2 attempts (1 failure + 1 success)"
        );
    }

    #[tokio::test]
    async fn download_with_retry_fails_fast_on_404() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let attempt_count = Arc::new(AtomicU32::new(0));
        let attempt_count_server = Arc::clone(&attempt_count);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 512];
                let _ = stream.read(&mut buf).await;
                attempt_count_server.fetch_add(1, Ordering::SeqCst);
                let body = b"not found";
                let headers = format!(
                    "HTTP/1.1 404 Not Found\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\
                     \r\n",
                    body.len(),
                );
                let _ = stream.write_all(headers.as_bytes()).await;
                let _ = stream.write_all(body).await;
                let _ = stream.shutdown().await;
            }
        });

        let tmp = tempfile::tempdir().unwrap();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let url = format!("http://{addr}/binary");
        let redacted = redact_url(&url);

        let result = download_with_retry(&client, &url, &redacted, tmp.path(), 2).await;

        assert!(result.is_err(), "404 must not be retried");
        assert_eq!(
            attempt_count.load(Ordering::SeqCst),
            1,
            "404 must fail on first attempt without retry"
        );
    }
}
