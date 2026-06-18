// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

//! `check-update` subcommand: compare local version against latest GitHub release.
//!
//! Three concerns are deliberately split into pure and impure halves so the
//! decision logic is unit-testable without HTTP:
//! - `parse_release`: pure JSON → `LatestRelease`.
//! - `compare`: pure (current, latest) → `Outcome`.
//! - `render`: pure (Result<Outcome, CheckError>) → `(stdout, stderr, u8)`.
//! - `fetch_latest`: impure HTTP GET (the only piece exercised at runtime).
//! - `run`: glues `fetch_latest` + `compare` + `render` and prints to real I/O.

use std::process::ExitCode;
use std::time::Duration;

use semver::Version;
use serde::Deserialize;

const RELEASES_LATEST_URL: &str =
    "https://api.github.com/repos/tooluse-labs/perfetto-mcp-rs/releases/latest";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// The `tag_name` and `published_at` fields from the GitHub /releases/latest
/// payload. Other fields are ignored by `serde(default)`-on-missing semantics
/// (we only deserialize what we use).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct LatestRelease {
    pub tag_name: String,
    pub published_at: String,
}

/// Result of comparing the local version with the latest release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Local version equals the latest release.
    UpToDate { current: Version },
    /// Local version is newer than the latest release (developer build).
    Ahead { current: Version, latest: Version },
    /// Local version is older than the latest release.
    Behind {
        current: Version,
        latest: Version,
        published_at: String,
    },
}

/// Reasons `check-update` couldn't determine a verdict. Distinct variants so
/// error messages name the actual failure mode rather than a generic "failed".
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    #[error("failed to query GitHub releases API: {0}")]
    Network(String),
    #[error("GitHub release JSON parse failed: {0}")]
    JsonParse(String),
    #[error("could not parse semver from tag {tag:?}: {source}")]
    SemverParse { tag: String, source: semver::Error },
    #[error("could not parse local CARGO_PKG_VERSION {version:?}: {source}")]
    LocalSemverParse {
        version: String,
        source: semver::Error,
    },
}

pub async fn run() -> ExitCode {
    let outcome = check().await;
    let (stdout, stderr, code) = render(outcome);
    if let Some(s) = stdout {
        println!("{s}");
    }
    if let Some(s) = stderr {
        eprintln!("{s}");
    }
    ExitCode::from(code)
}

async fn check() -> Result<Outcome, CheckError> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(format!("perfetto-mcp-rs/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| CheckError::Network(e.to_string()))?;
    let release = fetch_latest(&client, RELEASES_LATEST_URL).await?;
    let current = parse_local_version()?;
    let latest = parse_release_tag(&release.tag_name)?;
    Ok(compare(current, latest, release.published_at))
}

async fn fetch_latest(client: &reqwest::Client, url: &str) -> Result<LatestRelease, CheckError> {
    let body = client
        .get(url)
        .send()
        .await
        .map_err(|e| CheckError::Network(e.to_string()))?
        .error_for_status()
        .map_err(|e| CheckError::Network(e.to_string()))?
        .text()
        .await
        .map_err(|e| CheckError::Network(e.to_string()))?;
    parse_release(&body)
}

fn parse_release(body: &str) -> Result<LatestRelease, CheckError> {
    serde_json::from_str(body).map_err(|e| CheckError::JsonParse(e.to_string()))
}

fn parse_local_version() -> Result<Version, CheckError> {
    let raw = env!("CARGO_PKG_VERSION");
    Version::parse(raw).map_err(|source| CheckError::LocalSemverParse {
        version: raw.to_owned(),
        source,
    })
}

fn parse_release_tag(tag: &str) -> Result<Version, CheckError> {
    let stripped = tag.strip_prefix('v').unwrap_or(tag);
    Version::parse(stripped).map_err(|source| CheckError::SemverParse {
        tag: tag.to_owned(),
        source,
    })
}

fn compare(current: Version, latest: Version, published_at: String) -> Outcome {
    match current.cmp(&latest) {
        std::cmp::Ordering::Equal => Outcome::UpToDate { current },
        std::cmp::Ordering::Greater => Outcome::Ahead { current, latest },
        std::cmp::Ordering::Less => Outcome::Behind {
            current,
            latest,
            published_at,
        },
    }
}

fn upgrade_hint() -> String {
    "Run `perfetto-mcp-rs update` to upgrade.".to_owned()
}

fn render(result: Result<Outcome, CheckError>) -> (Option<String>, Option<String>, u8) {
    match result {
        Ok(Outcome::UpToDate { current }) => {
            (Some(format!("You're on v{current} (latest).")), None, 0)
        }
        Ok(Outcome::Ahead { current, latest }) => (
            Some(format!(
                "You're on v{current}, ahead of latest release v{latest} (local dev build)."
            )),
            None,
            0,
        ),
        Ok(Outcome::Behind {
            current,
            latest,
            published_at,
        }) => (
            Some(format!(
                "You're on v{current}. Latest is v{latest} (released {published_at}).\n{}",
                upgrade_hint(),
            )),
            None,
            2,
        ),
        Err(e) => (None, Some(format!("check-update failed: {e}")), 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).expect("test version literal must parse")
    }

    #[test]
    fn compare_equal_versions_is_up_to_date() {
        let outcome = compare(v("0.12.0"), v("0.12.0"), "2026-04-30T00:00:00Z".to_owned());
        assert_eq!(
            outcome,
            Outcome::UpToDate {
                current: v("0.12.0")
            }
        );
    }

    #[test]
    fn compare_current_greater_is_ahead() {
        let outcome = compare(v("0.12.1"), v("0.12.0"), "2026-04-30T00:00:00Z".to_owned());
        assert_eq!(
            outcome,
            Outcome::Ahead {
                current: v("0.12.1"),
                latest: v("0.12.0"),
            }
        );
    }

    #[test]
    fn compare_current_less_is_behind_with_published_at() {
        let outcome = compare(v("0.11.3"), v("0.12.0"), "2026-04-30T12:34:56Z".to_owned());
        assert_eq!(
            outcome,
            Outcome::Behind {
                current: v("0.11.3"),
                latest: v("0.12.0"),
                published_at: "2026-04-30T12:34:56Z".to_owned(),
            }
        );
    }

    #[test]
    fn render_up_to_date_prints_to_stdout_exits_zero() {
        let (stdout, stderr, code) = render(Ok(Outcome::UpToDate {
            current: v("0.12.0"),
        }));
        assert_eq!(stdout, Some("You're on v0.12.0 (latest).".to_owned()));
        assert_eq!(stderr, None);
        assert_eq!(code, 0);
    }

    #[test]
    fn render_ahead_prints_dev_build_note_exits_zero() {
        let (stdout, stderr, code) = render(Ok(Outcome::Ahead {
            current: v("0.12.1"),
            latest: v("0.12.0"),
        }));
        let s = stdout.expect("ahead must produce stdout");
        assert!(s.contains("v0.12.1"), "got: {s}");
        assert!(s.contains("v0.12.0"), "got: {s}");
        assert!(s.contains("local dev build"), "got: {s}");
        assert_eq!(stderr, None);
        assert_eq!(code, 0);
    }

    #[test]
    fn render_behind_includes_upgrade_command_exits_two() {
        let (stdout, stderr, code) = render(Ok(Outcome::Behind {
            current: v("0.11.3"),
            latest: v("0.12.0"),
            published_at: "2026-04-30T12:34:56Z".to_owned(),
        }));
        let s = stdout.expect("behind must produce stdout");
        assert!(s.contains("v0.11.3"), "got: {s}");
        assert!(s.contains("v0.12.0"), "got: {s}");
        assert!(s.contains("2026-04-30"), "got: {s}");
        assert!(s.contains("perfetto-mcp-rs update"), "got: {s}");
        assert_eq!(stderr, None);
        assert_eq!(code, 2);
    }

    #[test]
    fn upgrade_hint_names_update_subcommand() {
        let hint = upgrade_hint();
        assert!(hint.contains("perfetto-mcp-rs update"), "got: {hint}");
    }

    #[test]
    fn render_error_writes_to_stderr_exits_one() {
        let err = CheckError::Network("connection refused".to_owned());
        let (stdout, stderr, code) = render(Err(err));
        assert_eq!(stdout, None);
        let s = stderr.expect("error must produce stderr");
        assert!(s.starts_with("check-update failed:"), "got: {s}");
        assert!(s.contains("connection refused"), "got: {s}");
        assert_eq!(code, 1);
    }

    /// Pin the three exit codes (0 / 1 / 2) per branch. The four `render_*`
    /// branch tests above also pin codes individually; this test exists as a
    /// single-place smoke that verifies all branches produce distinct codes
    /// matching the §3.3 contract.
    #[test]
    fn render_exit_codes_pin_contract() {
        let (_, _, code) = render(Ok(Outcome::UpToDate {
            current: v("1.0.0"),
        }));
        assert_eq!(code, 0);
        let (_, _, code) = render(Ok(Outcome::Ahead {
            current: v("1.0.1"),
            latest: v("1.0.0"),
        }));
        assert_eq!(code, 0);
        let (_, _, code) = render(Ok(Outcome::Behind {
            current: v("1.0.0"),
            latest: v("2.0.0"),
            published_at: "2026-04-30T00:00:00Z".to_owned(),
        }));
        assert_eq!(code, 2);
        let (_, _, code) = render(Err(CheckError::Network(String::new())));
        assert_eq!(code, 1);
    }

    const FIXTURE: &str = include_str!("../tests/fixtures/github_release_response.json");

    #[test]
    fn parse_release_accepts_real_github_payload() {
        let release = parse_release(FIXTURE).expect("fixture must parse");
        assert!(
            release.tag_name.starts_with('v'),
            "got: {}",
            release.tag_name
        );
        assert!(!release.published_at.is_empty());
    }

    #[test]
    fn parse_release_rejects_missing_tag_name() {
        let body = r#"{"published_at":"2026-04-30T00:00:00Z"}"#;
        let err = parse_release(body).expect_err("missing tag_name must error");
        match err {
            CheckError::JsonParse(msg) => assert!(
                msg.contains("tag_name"),
                "JSON parse error must name missing field, got: {msg}",
            ),
            other => panic!("expected JsonParse, got: {other:?}"),
        }
    }

    #[test]
    fn parse_release_rejects_garbage() {
        let err = parse_release("not json").expect_err("garbage must error");
        assert!(matches!(err, CheckError::JsonParse(_)));
    }

    #[test]
    fn parse_release_tag_strips_v_prefix() {
        let v = parse_release_tag("v0.12.0").expect("v-prefix must strip");
        assert_eq!(v, Version::parse("0.12.0").unwrap());
    }

    #[test]
    fn parse_release_tag_accepts_no_prefix() {
        let v = parse_release_tag("0.12.0").expect("plain semver must parse");
        assert_eq!(v, Version::parse("0.12.0").unwrap());
    }

    #[test]
    fn parse_release_tag_rejects_garbage() {
        let err = parse_release_tag("not-a-version").expect_err("non-semver must error");
        match err {
            CheckError::SemverParse { tag, .. } => assert_eq!(tag, "not-a-version"),
            other => panic!("expected SemverParse, got: {other:?}"),
        }
    }
}
