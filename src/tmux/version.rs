//! Parse `tmux -V` output and compare against a required minimum.
//!
//! tmux prints one of:
//!   * `tmux 3.4`       — stable release
//!   * `tmux 3.5a`      — point release with letter suffix
//!   * `tmux next-3.6`  — pre-release branch
//!   * `tmux master`    — bleeding-edge build (treat as latest)
//!
//! We require ≥ 3.4 (a hard floor for the control-mode features we rely on).

use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    /// `0` for unsuffixed (`3.4`), `1` for `a`, `2` for `b`, etc.
    pub patch_letter: u32,
}

impl Version {
    pub const MIN_SUPPORTED: Self = Self {
        major: 3,
        minor: 4,
        patch_letter: 0,
    };

    pub fn satisfies_min(&self) -> bool {
        *self >= Self::MIN_SUPPORTED
    }
}

/// Parse a `tmux -V` output line. Returns `Ok(None)` for `tmux master`
/// (treated as "latest and acceptable" — we have no semver to compare).
pub fn parse(output: &str) -> Result<Option<Version>> {
    let trimmed = output.trim();
    let after_tmux = trimmed
        .strip_prefix("tmux ")
        .ok_or_else(|| anyhow::anyhow!("not a tmux -V line: {trimmed:?}"))?;

    if after_tmux == "master" {
        return Ok(None);
    }

    // `next-3.6` form: strip `next-`.
    let core = after_tmux.strip_prefix("next-").unwrap_or(after_tmux);

    // `3.5a` form: split on `.`, parse major; the minor portion may carry a
    // trailing letter (a/b/c/…) which we map to patch_letter 1, 2, 3, ….
    let (major_str, minor_str) = core
        .split_once('.')
        .ok_or_else(|| anyhow::anyhow!("malformed tmux version: {core:?}"))?;
    let major: u32 = major_str
        .parse()
        .map_err(|_| anyhow::anyhow!("bad major in tmux version: {major_str:?}"))?;

    let (minor_digits, suffix) = split_minor(minor_str);
    let minor: u32 = minor_digits
        .parse()
        .map_err(|_| anyhow::anyhow!("bad minor in tmux version: {minor_str:?}"))?;

    let patch_letter = if suffix.is_empty() {
        0
    } else {
        let c = suffix
            .chars()
            .next()
            .ok_or_else(|| anyhow::anyhow!("empty suffix"))?
            .to_ascii_lowercase();
        if !c.is_ascii_alphabetic() {
            bail!("unexpected version suffix: {suffix:?}");
        }
        (c as u32) - ('a' as u32) + 1
    };

    Ok(Some(Version {
        major,
        minor,
        patch_letter,
    }))
}

fn split_minor(s: &str) -> (&str, &str) {
    let mut digit_end = 0;
    for (i, c) in s.char_indices() {
        if c.is_ascii_digit() {
            digit_end = i + c.len_utf8();
        } else {
            break;
        }
    }
    s.split_at(digit_end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stable_release() {
        let v = parse("tmux 3.4").unwrap().unwrap();
        assert_eq!(v, Version { major: 3, minor: 4, patch_letter: 0 });
    }

    #[test]
    fn parses_letter_suffix() {
        let v = parse("tmux 3.5a").unwrap().unwrap();
        assert_eq!(v, Version { major: 3, minor: 5, patch_letter: 1 });
        let v = parse("tmux 3.5c").unwrap().unwrap();
        assert_eq!(v, Version { major: 3, minor: 5, patch_letter: 3 });
    }

    #[test]
    fn parses_next() {
        let v = parse("tmux next-3.6").unwrap().unwrap();
        assert_eq!(v, Version { major: 3, minor: 6, patch_letter: 0 });
    }

    #[test]
    fn parses_master_as_none() {
        assert!(parse("tmux master").unwrap().is_none());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("totally not tmux").is_err());
        assert!(parse("tmux").is_err());
        assert!(parse("tmux abc").is_err());
    }

    #[test]
    fn min_supported_3_4() {
        assert!(parse("tmux 3.4").unwrap().unwrap().satisfies_min());
        assert!(parse("tmux 3.5a").unwrap().unwrap().satisfies_min());
        assert!(parse("tmux 4.0").unwrap().unwrap().satisfies_min());
        assert!(parse("tmux next-3.7").unwrap().unwrap().satisfies_min());
    }

    #[test]
    fn rejects_3_3() {
        assert!(!parse("tmux 3.3").unwrap().unwrap().satisfies_min());
        assert!(!parse("tmux 3.3a").unwrap().unwrap().satisfies_min());
        assert!(!parse("tmux 2.9").unwrap().unwrap().satisfies_min());
    }

    #[test]
    fn ordering_handles_letter_suffix() {
        let a = parse("tmux 3.5").unwrap().unwrap();
        let b = parse("tmux 3.5a").unwrap().unwrap();
        let c = parse("tmux 3.5b").unwrap().unwrap();
        assert!(a < b);
        assert!(b < c);
        assert!(a < c);
    }
}
