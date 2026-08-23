//! Asking GitHub whether a newer release exists.
//!
//! Checking only: nothing here downloads or replaces anything. An updater that
//! fetches a binary and runs it is a channel for executing code on the user's
//! machine, and earning that needs signed artefacts and a verification step
//! this application does not have yet. Pointing at the release page leaves the
//! decision, and the download, with the person.

use std::sync::Arc;

use anyhow::Result;
use gpui::http_client::{github::latest_github_release, HttpClient};

/// Where releases are published.
pub const REPO: &str = "duykhanhxx03/s3-browser";

/// A release newer than the running build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    /// Normalised, without the tag's leading `v`.
    pub version: String,
    /// The release page, not an asset: the user picks the build for their own
    /// platform, which this application cannot do for them from here.
    pub url: String,
}

/// Splits a tag into numbers that can be compared.
///
/// Returns `None` for anything it cannot read as a version. That matters more
/// than it looks: an unreadable tag must not be treated as newer, or a
/// repository that once publishes `nightly` nags every user forever.
fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let tag = tag.trim();
    let tag = tag.strip_prefix('v').or_else(|| tag.strip_prefix('V')).unwrap_or(tag);
    // A pre-release or build suffix is dropped rather than rejected: `0.2.0-rc.1`
    // is still recognisably 0.2.0, and comparing it as equal keeps a release
    // candidate from being offered as newer than the release it precedes.
    let tag = tag.split(['-', '+']).next()?;

    let mut parts = tag.split('.');
    let major = parts.next()?.parse().ok()?;
    // Missing components are zero, so `1.0` and `1.0.0` compare equal rather
    // than one of them failing to parse.
    let minor = match parts.next() {
        Some(part) => part.parse().ok()?,
        None => 0,
    };
    let patch = match parts.next() {
        Some(part) => part.parse().ok()?,
        None => 0,
    };
    // Trailing junk means this is not a version, whatever the first fields say.
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Whether `candidate` names a version above `current`.
///
/// Compared as numbers, never as text: `0.10.0` sorts *below* `0.9.0` as a
/// string, so a string comparison starts silently withholding updates at the
/// tenth minor release and looks correct until then.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

/// Asks GitHub for the newest release, and reports one only if it is newer.
///
/// `require_assets` is on: a release with no attached files gives a user
/// nothing to install, so offering it sends them to an empty page.
/// Pre-releases are excluded for the same reason a release candidate parses as
/// its own release above — people running the published build did not ask to
/// test the next one.
pub async fn check(http: Arc<dyn HttpClient>, current: &str) -> Result<Option<Update>> {
    let release = latest_github_release(REPO, true, false, http).await?;
    if !is_newer(&release.tag_name, current) {
        return Ok(None);
    }
    let version = release
        .tag_name
        .trim_start_matches(['v', 'V'])
        .to_string();
    Ok(Some(Update {
        version,
        url: format!(
            "https://github.com/{REPO}/releases/tag/{}",
            release.tag_name
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_higher_version_is_newer() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
    }

    #[test]
    fn the_leading_v_on_a_tag_is_optional() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
    }

    /// The reason versions are parsed rather than compared as strings: this is
    /// the first release where the two disagree, and a string comparison would
    /// stop reporting updates from here on without ever failing loudly.
    #[test]
    fn ten_is_above_nine_rather_than_below_it() {
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
        assert!(is_newer("0.2.10", "0.2.9"));
    }

    #[test]
    fn the_same_version_is_not_an_update() {
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn a_missing_component_counts_as_zero() {
        assert!(!is_newer("1.0", "1.0.0"));
        assert!(!is_newer("1", "1.0.0"));
        assert!(is_newer("1.1", "1.0.9"));
    }

    /// A tag nobody can read is not an update. Treating it as one would nag
    /// every user on every launch, with no version to move to.
    #[test]
    fn an_unreadable_tag_is_never_newer() {
        assert!(!is_newer("nightly", "0.1.0"));
        assert!(!is_newer("", "0.1.0"));
        assert!(!is_newer("release-2026", "0.1.0"));
        assert!(!is_newer("1.2.3.4", "0.1.0"));
        assert!(!is_newer("0.1.0", "not-a-version"));
    }

    /// A release candidate compares as the release it precedes, so someone on
    /// the published build is not offered the test build of the same number.
    #[test]
    fn a_pre_release_suffix_does_not_make_it_newer() {
        assert!(!is_newer("0.1.0-rc.1", "0.1.0"));
        assert!(is_newer("0.2.0-rc.1", "0.1.0"));
        assert!(!is_newer("0.1.0+build.7", "0.1.0"));
    }
}
