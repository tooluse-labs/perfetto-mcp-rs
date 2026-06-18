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
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("update failed: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn run_inner(args: UpdateArgs) -> Result<u8> {
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

fn run_installer(invocation: &InstallerInvocation, script: &str) -> Result<u8> {
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
    Ok(exit_status_code(status))
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
}
