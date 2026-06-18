// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

//! `update` subcommand: download and run the official installer.
//!
//! The installer scripts already own platform-specific replacement logic,
//! Windows file-lock handling, PATH setup, and MCP re-registration. Keeping
//! this command as a thin launcher avoids duplicating that logic in Rust.

use std::io::Write;
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};

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
        Ok(exit) if exit.success() => ExitCode::from(0),
        Ok(exit) => {
            eprintln!("update failed: {}", exit.failure_message());
            ExitCode::from(exit.code)
        }
        Err(e) => {
            eprintln!("update failed: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn run_inner(args: UpdateArgs) -> Result<InstallerExit> {
    let invocation = installer_invocation(current_platform(), &args);
    let script = fetch_installer(invocation.url).await?;
    run_installer(&invocation, &script)
}

async fn fetch_installer(url: &str) -> Result<String> {
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

    #[cfg(unix)]
    #[test]
    fn run_installer_streams_script_to_child_stdin() {
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("capture-stdin.sh");
        let captured = temp.path().join("captured-installer.sh");
        fs::write(&helper, "#!/bin/sh\ncat > \"$1\"\n").unwrap();
        let mut perms = fs::metadata(&helper).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&helper, perms).unwrap();

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
}
