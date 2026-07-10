// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

//! `update` subcommand: download and run the official installer.
//!
//! The installer scripts already own platform-specific replacement logic,
//! Windows file-lock handling, PATH setup, and MCP re-registration. Keeping
//! this command as a thin launcher avoids duplicating that logic in Rust.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use semver::Version;

use crate::check_update;
use crate::install::ClaudeScope;

const INSTALL_SH_URL: &str =
    "https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh";
const INSTALL_PS1_URL: &str =
    "https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.ps1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const POWERSHELL_STDIN_COMMAND: &str = "-";

#[derive(clap::Args, Clone, Debug, PartialEq, Eq)]
pub struct UpdateArgs {
    /// Release tag to install. Defaults to latest.
    #[arg(long, short = 'V', value_name = "TAG")]
    pub version: Option<String>,

    /// Claude scope to re-register under. Defaults to installer/SCOPE env behavior.
    ///
    /// For `--scope local` / `project`, run from the target project directory.
    #[arg(long, value_enum)]
    pub scope: Option<ClaudeScope>,
}

pub async fn run(args: UpdateArgs) -> ExitCode {
    match run_inner(args).await {
        Ok(UpdateOutcome::NoUpdate { current, latest }) if current == latest => {
            println!("perfetto-mcp-rs is already on the latest release (v{current}).");
            ExitCode::from(0)
        }
        Ok(UpdateOutcome::NoUpdate { current, latest }) => {
            println!("No update needed: running v{current}, ahead of latest release v{latest}.");
            ExitCode::from(0)
        }
        Ok(UpdateOutcome::Installed { version, path }) => {
            println!("Update complete: {} is now v{version}.", path.display());
            ExitCode::from(0)
        }
        Ok(UpdateOutcome::InstallerFailed(exit)) => {
            eprintln!("update failed: {}", exit.failure_message());
            ExitCode::from(exit.code)
        }
        Err(e) => {
            eprintln!("update failed: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn run_inner(mut args: UpdateArgs) -> Result<UpdateOutcome> {
    let requested_version = args.version.as_deref();
    let target = resolve_target_version(requested_version).await?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("the running binary has an invalid embedded version")?;

    if requested_version.is_none() && target <= current {
        return Ok(UpdateOutcome::NoUpdate {
            current,
            latest: target,
        });
    }

    args.version = Some(format!("v{target}"));
    let platform = current_platform();
    let invocation = installer_invocation(platform, &args);

    println!("Updating perfetto-mcp-rs from v{current} to v{target}...");
    println!("Downloading the official installer from {}", invocation.url);
    let script = fetch_installer(platform, invocation.url).await?;
    let exit = execute_installer(platform, &invocation, &script)?;
    if !exit.success() {
        return Ok(UpdateOutcome::InstallerFailed(exit));
    }

    let path = installed_binary_path(platform)?;
    let installed = verify_installed_version(&path, &target)?;
    Ok(UpdateOutcome::Installed {
        version: installed,
        path,
    })
}

async fn resolve_target_version(requested: Option<&str>) -> Result<Version> {
    if let Some(tag) = requested {
        return Version::parse(tag.strip_prefix('v').unwrap_or(tag))
            .with_context(|| format!("invalid release version {tag:?}"));
    }

    check_update::latest_version()
        .await
        .map(|latest| latest.version)
        .context("failed to determine the latest release; retry or pass --version <TAG>")
}

async fn fetch_installer(platform: InstallerPlatform, url: &'static str) -> Result<String> {
    match platform {
        InstallerPlatform::Unix => {
            tokio::task::spawn_blocking(move || fetch_installer_with_curl(OsStr::new("curl"), url))
                .await
                .context("installer download task failed")?
        }
        InstallerPlatform::Windows => fetch_installer_with_http(url).await,
    }
}

fn fetch_installer_with_curl(program: &OsStr, url: &str) -> Result<String> {
    let output = Command::new(program)
        .args([
            "-fsSL",
            "--retry",
            "3",
            "--retry-delay",
            "1",
            "--connect-timeout",
            "20",
            "--max-time",
            "60",
            url,
        ])
        .output()
        .with_context(|| format!("failed to start {}", program.to_string_lossy()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        if detail.is_empty() {
            bail!("curl failed with {} while downloading {url}", output.status);
        }
        bail!(
            "curl failed with {} while downloading {url}: {detail}",
            output.status
        );
    }

    String::from_utf8(output.stdout).context("downloaded installer was not valid UTF-8")
}

async fn fetch_installer_with_http(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(format!("perfetto-mcp-rs/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build HTTP client")?;

    client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to download installer from {url}"))?
        .error_for_status()
        .with_context(|| format!("installer download returned an error status from {url}"))?
        .text()
        .await
        .context("failed to read installer response body")
}

fn execute_installer(
    platform: InstallerPlatform,
    invocation: &InstallerInvocation,
    script: &str,
) -> Result<InstallerExit> {
    validate_installer(platform, script)?;
    run_installer(invocation, script)
}

fn validate_installer(platform: InstallerPlatform, script: &str) -> Result<()> {
    let markers: &[&str] = match platform {
        InstallerPlatform::Unix => &[
            "#!/usr/bin/env sh",
            "BIN_NAME=\"perfetto-mcp-rs\"",
            "main \"$@\"",
        ],
        InstallerPlatform::Windows => &[
            "function Install-PerfettoMcp",
            "perfetto-mcp-rs-windows-amd64.exe",
            "Invoke-WebRequest",
        ],
    };

    if script.len() < 512 || markers.iter().any(|marker| !script.contains(marker)) {
        bail!(
            "downloaded installer was empty or invalid; refusing to execute it ({} bytes)",
            script.len()
        );
    }
    Ok(())
}

fn run_installer(invocation: &InstallerInvocation, script: &str) -> Result<InstallerExit> {
    let mut child = Command::new(invocation.program)
        .args(&invocation.args)
        .envs(invocation.env.iter().map(|(k, v)| (*k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to start {}", invocation.program))?;

    {
        let mut stdin = child
            .stdin
            .take()
            .context("failed to open installer stdin")?;
        stdin
            .write_all(script.as_bytes())
            .context("failed to write installer script to stdin")?;
    }

    let status = child.wait().context("failed to wait for installer")?;
    Ok(InstallerExit::from_status(status))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InstallerExit {
    code: u8,
    status_text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum UpdateOutcome {
    NoUpdate { current: Version, latest: Version },
    Installed { version: Version, path: PathBuf },
    InstallerFailed(InstallerExit),
}

impl InstallerExit {
    fn from_status(status: ExitStatus) -> Self {
        Self {
            code: exit_status_code(status),
            status_text: status.to_string(),
        }
    }

    fn success(&self) -> bool {
        self.code == 0
    }

    fn failure_message(&self) -> String {
        format!(
            "installer exited with {}; any installer output should appear above",
            self.status_text
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstallerPlatform {
    Unix,
    Windows,
}

fn current_platform() -> InstallerPlatform {
    if cfg!(windows) {
        InstallerPlatform::Windows
    } else {
        InstallerPlatform::Unix
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InstallerInvocation {
    url: &'static str,
    program: &'static str,
    args: Vec<String>,
    env: Vec<(&'static str, String)>,
}

fn installer_invocation(
    platform: InstallerPlatform,
    update_args: &UpdateArgs,
) -> InstallerInvocation {
    let mut env = Vec::new();
    if let Some(scope) = update_args.scope {
        env.push(("SCOPE", scope.to_string()));
    }

    match platform {
        InstallerPlatform::Unix => {
            let mut args = vec!["-s".to_owned(), "--".to_owned()];
            if let Some(version) = &update_args.version {
                args.extend(["--version".to_owned(), version.clone()]);
            }
            InstallerInvocation {
                url: INSTALL_SH_URL,
                program: "sh",
                args,
                env,
            }
        }
        InstallerPlatform::Windows => {
            if let Some(version) = &update_args.version {
                env.push(("VERSION", version.clone()));
            }
            InstallerInvocation {
                url: INSTALL_PS1_URL,
                program: "powershell.exe",
                args: vec![
                    "-NoProfile".to_owned(),
                    "-NonInteractive".to_owned(),
                    "-ExecutionPolicy".to_owned(),
                    "Bypass".to_owned(),
                    "-Command".to_owned(),
                    POWERSHELL_STDIN_COMMAND.to_owned(),
                ],
                env,
            }
        }
    }
}

fn installed_binary_path(platform: InstallerPlatform) -> Result<PathBuf> {
    let install_dir = std::env::var_os("INSTALL_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("bin")))
        .context("could not determine installer destination; set INSTALL_DIR explicitly")?;
    let name = match platform {
        InstallerPlatform::Unix => "perfetto-mcp-rs",
        InstallerPlatform::Windows => "perfetto-mcp-rs.exe",
    };
    Ok(install_dir.join(name))
}

fn verify_installed_version(path: &Path, expected: &Version) -> Result<Version> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .with_context(|| {
            format!(
                "installer exited successfully but {} cannot be run",
                path.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "installer exited successfully but {} --version returned {}",
            path.display(),
            output.status
        );
    }

    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("{} --version returned non-UTF-8 output", path.display()))?;
    let raw = stdout
        .trim()
        .strip_prefix("perfetto-mcp-rs ")
        .with_context(|| {
            format!(
                "unexpected --version output from {}: {stdout:?}",
                path.display()
            )
        })?;
    let actual = Version::parse(raw)
        .with_context(|| format!("invalid installed version from {}: {raw:?}", path.display()))?;
    if &actual != expected {
        bail!(
            "installer exited successfully but {} reports v{actual}; expected v{expected}",
            path.display()
        );
    }
    Ok(actual)
}

fn exit_status_code(status: ExitStatus) -> u8 {
    match status.code() {
        Some(code) if (0..=255).contains(&code) => code as u8,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn args(version: Option<&str>, scope: Option<ClaudeScope>) -> UpdateArgs {
        UpdateArgs {
            version: version.map(str::to_owned),
            scope,
        }
    }

    #[test]
    fn unix_invocation_runs_install_sh_from_stdin() {
        let invocation = installer_invocation(InstallerPlatform::Unix, &args(None, None));

        assert_eq!(invocation.url, INSTALL_SH_URL);
        assert_eq!(invocation.program, "sh");
        assert_eq!(invocation.args, ["-s", "--"]);
        assert!(invocation.env.is_empty());
    }

    #[test]
    fn unix_invocation_passes_version_as_installer_arg() {
        let invocation =
            installer_invocation(InstallerPlatform::Unix, &args(Some("v0.16.2"), None));

        assert_eq!(invocation.args, ["-s", "--", "--version", "v0.16.2"]);
        assert!(invocation.env.is_empty());
    }

    #[test]
    fn invocation_passes_scope_through_env() {
        let invocation = installer_invocation(
            InstallerPlatform::Unix,
            &args(None, Some(ClaudeScope::Local)),
        );

        assert_eq!(invocation.env, [("SCOPE", "local".to_owned())]);
    }

    #[test]
    fn windows_invocation_runs_install_ps1_from_stdin() {
        let invocation = installer_invocation(InstallerPlatform::Windows, &args(None, None));

        assert_eq!(invocation.url, INSTALL_PS1_URL);
        assert_eq!(invocation.program, "powershell.exe");
        assert_eq!(
            invocation.args,
            [
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                POWERSHELL_STDIN_COMMAND,
            ]
        );
        assert!(invocation.env.is_empty());
    }

    #[test]
    fn windows_invocation_passes_version_through_env() {
        let invocation =
            installer_invocation(InstallerPlatform::Windows, &args(Some("v0.16.2"), None));

        assert_eq!(invocation.env, [("VERSION", "v0.16.2".to_owned())]);
    }

    #[tokio::test]
    async fn explicit_target_version_accepts_v_prefix() {
        assert_eq!(
            resolve_target_version(Some("v0.17.0")).await.unwrap(),
            Version::parse("0.17.0").unwrap()
        );
    }

    #[tokio::test]
    async fn explicit_target_version_rejects_invalid_tag() {
        let error = resolve_target_version(Some("latest"))
            .await
            .expect_err("non-semver tags must be rejected");
        assert!(error.to_string().contains("invalid release version"));
    }

    #[test]
    fn checked_in_installers_pass_payload_validation() {
        validate_installer(InstallerPlatform::Unix, include_str!("../install.sh")).unwrap();
        validate_installer(InstallerPlatform::Windows, include_str!("../install.ps1")).unwrap();
    }

    #[test]
    fn empty_and_html_payloads_are_rejected() {
        for body in ["", "<html><body>proxy error</body></html>"] {
            let error = validate_installer(InstallerPlatform::Unix, body)
                .expect_err("non-installer response must be rejected");
            assert!(error.to_string().contains("refusing to execute"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn invalid_payload_never_starts_the_shell() {
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("record-start.sh");
        let marker = temp.path().join("shell-started");
        write_executable(&helper, "#!/bin/sh\n: > \"$1\"\ncat >/dev/null\n");
        let invocation = InstallerInvocation {
            url: INSTALL_SH_URL,
            program: "/bin/sh",
            args: vec![
                helper.to_string_lossy().to_string(),
                marker.to_string_lossy().to_string(),
            ],
            env: Vec::new(),
        };

        execute_installer(InstallerPlatform::Unix, &invocation, "")
            .expect_err("empty response must fail before process creation");
        assert!(!marker.exists(), "shell was started for an invalid payload");
    }

    #[cfg(unix)]
    #[test]
    fn curl_failure_preserves_status_and_stderr() {
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("failing-curl.sh");
        write_executable(
            &helper,
            "#!/bin/sh\nprintf 'proxy refused request\\n' >&2\nexit 17\n",
        );

        let error = fetch_installer_with_curl(helper.as_os_str(), INSTALL_SH_URL)
            .expect_err("download failure must not be treated as an empty installer");
        let message = error.to_string();
        assert!(message.contains("exit status: 17"), "got: {message}");
        assert!(message.contains("proxy refused request"), "got: {message}");
    }

    #[cfg(unix)]
    #[test]
    fn successful_installer_with_stale_binary_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("perfetto-mcp-rs");
        write_executable(&binary, "#!/bin/sh\nprintf 'perfetto-mcp-rs 0.16.3\\n'\n");

        let error = verify_installed_version(&binary, &Version::parse("0.17.0").unwrap())
            .expect_err("a stale binary must fail post-install verification");
        let message = error.to_string();
        assert!(message.contains("reports v0.16.3"), "got: {message}");
        assert!(message.contains("expected v0.17.0"), "got: {message}");
    }

    #[cfg(unix)]
    #[test]
    fn post_install_verification_accepts_exact_target() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("perfetto-mcp-rs");
        write_executable(&binary, "#!/bin/sh\nprintf 'perfetto-mcp-rs 0.17.0\\n'\n");

        assert_eq!(
            verify_installed_version(&binary, &Version::parse("0.17.0").unwrap()).unwrap(),
            Version::parse("0.17.0").unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_installer_streams_script_to_child_stdin() {
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("capture-stdin.sh");
        let captured = temp.path().join("captured-installer.sh");
        write_executable(&helper, "#!/bin/sh\ncat > \"$1\"\n");

        let invocation = InstallerInvocation {
            url: INSTALL_SH_URL,
            program: "/bin/sh",
            args: vec![
                helper.to_string_lossy().to_string(),
                captured.to_string_lossy().to_string(),
            ],
            env: Vec::new(),
        };

        let exit = run_installer(&invocation, "echo installer body\n").unwrap();

        assert!(exit.success());
        assert_eq!(
            fs::read_to_string(captured).unwrap(),
            "echo installer body\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_installer_returns_nonzero_status_for_silent_child_failure() {
        let invocation = InstallerInvocation {
            url: INSTALL_SH_URL,
            program: "/bin/sh",
            args: vec!["-c".to_owned(), "exit 7".to_owned()],
            env: Vec::new(),
        };

        let exit = run_installer(&invocation, "ignored").unwrap();

        assert_eq!(exit.code, 7);
        assert!(!exit.success());
        assert!(
            exit.failure_message()
                .contains("installer exited with exit status: 7"),
            "got: {}",
            exit.failure_message()
        );
    }

    #[test]
    fn installer_exit_maps_signal_or_unknown_status_to_one() {
        let exit = InstallerExit {
            code: 1,
            status_text: "signal: 9 (SIGKILL)".to_owned(),
        };

        assert_eq!(exit.code, 1);
        assert_eq!(
            exit.failure_message(),
            "installer exited with signal: 9 (SIGKILL); any installer output should appear above"
        );
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }
}
