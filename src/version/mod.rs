//! Version handling for install/update flows: the three lookup APIs
//! (read-only HTTP GETs — no filesystem or process side effects), plus the
//! sentinel and display helper they all feed.

pub mod crates_io;
pub mod github;
pub mod npm;

use anyhow::Context;
use serde::de::DeserializeOwned;

use crate::sources::USER_AGENT;

/// A `[[curl]]` install script exposes no version, so `sources::curl`
/// records this and the CLI prints nothing in its place.
pub const UNVERSIONED: &str = "unversioned";

/// Display only: strips a tag's `v` prefix and shortens a full commit sha.
/// The stored string stays verbatim — it names `pkg/<tool>/<version>/` and
/// is compared byte-for-byte against the upstream tag.
pub fn display_version(version: &str) -> &str {
    // Upstream tags disagree on the `v` (ccs tags `v0.44.1`, delta `0.18.2`);
    // requiring a digit after it leaves `version-2`-style tags intact.
    let trimmed = match version.strip_prefix('v') {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest,
        _ => version,
    };
    // git/plugin record a full HEAD sha; hex never starts with `v`.
    if trimmed.len() == 40 && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        &trimmed[..7]
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_version_unifies_tag_prefixes() {
        // The whole point: two repos, two tagging habits, one output shape.
        assert_eq!(display_version("v0.44.1"), "0.44.1");
        assert_eq!(display_version("0.18.2"), "0.18.2");
        assert_eq!(display_version("v2"), "2");
    }

    #[test]
    fn display_version_leaves_non_semver_alone() {
        assert_eq!(display_version("0.24.0-1build1"), "0.24.0-1build1");
        assert_eq!(display_version(UNVERSIONED), UNVERSIONED);
        // `v` not followed by a digit is part of the name, not a prefix.
        assert_eq!(display_version("version-2"), "version-2");
        assert_eq!(display_version("valhalla"), "valhalla");
    }

    #[test]
    fn display_version_shortens_only_a_full_sha() {
        let sha = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
        assert_eq!(sha.len(), 40);
        assert_eq!(display_version(sha), "a1b2c3d");
        // 39 hex chars is not a sha, and `deadbeef` words aren't either.
        assert_eq!(display_version(&sha[..39]), &sha[..39]);
        assert_eq!(display_version("deadbeef"), "deadbeef");
    }

    #[test]
    fn display_version_survives_degenerate_input() {
        assert_eq!(display_version(""), "");
        assert_eq!(display_version("v"), "v");
    }
}

/// GET + parse, shared by all three version-lookup APIs — they differ only
/// in the URL and the response type.
pub(super) fn fetch_json<T: DeserializeOwned>(url: &str, what: &str) -> anyhow::Result<T> {
    let text = ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .with_context(|| format!("failed to query {url}"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("failed to read response body from {url}"))?;
    serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {what} response from {url}"))
}
