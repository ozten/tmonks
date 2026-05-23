//! Startup probe of `claude --version`, `codex --version`, `opencode --version`.
//!
//! Logs a single `info!` per CLI noting the calibrated vs. detected version.
//! Mismatch → `warn!`. CLI not on PATH → `info!` (it's normal not to have all
//! three installed). No SIGHUP reload — when a CLI updates, ship a new tmons.

use std::time::Duration;

use tokio::process::Command;

use crate::status::matchers::CALIBRATION;

pub async fn probe_all() {
    for (cli, calibrated) in CALIBRATION {
        match probe_one(cli).await {
            Ok(detected) => {
                if version_compatible(calibrated, &detected) {
                    tracing::info!(cli = cli, calibrated = calibrated, detected = %detected, "status calibration ok");
                } else {
                    tracing::warn!(
                        cli = cli,
                        calibrated = calibrated,
                        detected = %detected,
                        "status calibration drift: status detection may misreport for this CLI; please file an issue if badges are wrong"
                    );
                }
            }
            Err(_) => {
                tracing::info!(cli = cli, "not on PATH; if you run it, status detection will be limited to Unknown");
            }
        }
    }
}

async fn probe_one(cli: &str) -> std::io::Result<String> {
    let output = tokio::time::timeout(
        Duration::from_millis(500),
        Command::new(cli).arg("--version").output(),
    )
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "probe timed out"))??;
    if !output.status.success() {
        return Err(std::io::Error::other(
            "CLI --version exited non-zero",
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Loose semver compatibility check: same major version is "ok".
///
/// The matchers are written against marker text that historically only changes
/// across major versions. A minor-version mismatch is fine; a major mismatch
/// is when we explicitly want to surface a `warn!`.
pub fn version_compatible(calibrated: &str, detected: &str) -> bool {
    let cal_major = leading_token(calibrated);
    let det_major = detected
        .split_whitespace()
        .map(|tok| leading_token(tok.trim_start_matches('v')))
        .find(|s| !s.is_empty() && s.chars().next().is_some_and(|c| c.is_ascii_digit()));
    match det_major {
        Some(d) => d == cal_major,
        None => true, // unparseable — give the benefit of the doubt
    }
}

fn leading_token(s: &str) -> String {
    s.chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatible_same_major() {
        assert!(version_compatible("2.x", "claude-code 2.4.1"));
        assert!(version_compatible("0.x", "codex 0.99"));
    }

    #[test]
    fn compatible_when_detected_is_unparseable() {
        // Better to log "ok" than warn on every startup for weird outputs.
        assert!(version_compatible("2.x", "claude (built from source)"));
    }

    #[test]
    fn incompatible_major_bump() {
        assert!(!version_compatible("2.x", "claude-code 3.0.0"));
        assert!(!version_compatible("0.x", "codex 1.0"));
    }

    #[test]
    fn leading_token_strips_letters() {
        assert_eq!(leading_token("2.x"), "2");
        assert_eq!(leading_token("3.0.0"), "3");
        assert_eq!(leading_token("v2.4.1"), "");
    }
}
