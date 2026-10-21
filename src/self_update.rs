//! `rig update --self` — rig manages its own binary and bootstrap without
//! a config entry. Version knowledge lives here (mirrored by the target
//! mapping in the `rig.zsh` bootstrap); nothing in `~/.rig.toml` decides
//! how rig updates itself. It never touches the rate-limited GitHub API.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, anyhow, bail};

use crate::lock::{Lock, atomic_write};
use crate::paths::Layout;
use crate::sources;
use crate::update::Outcome;

const RELEASES_URL: &str = "https://github.com/gitduk/rig/releases";

/// The cargo-dist targets a release publishes, as `rig-<target>.tar.gz`
/// unpacking to `rig-<target>/rig`.
const SELF_TARGET: Option<&str> = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("x86_64-unknown-linux-gnu")
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    Some("aarch64-unknown-linux-gnu")
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    Some("x86_64-apple-darwin")
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("aarch64-apple-darwin")
} else {
    None
};

/// rig.zsh left the release archives when rig moved to cargo-dist; fetch
/// the canonical copy from the repo on a real self-update.
const BOOTSTRAP_URL: &str = "https://raw.githubusercontent.com/gitduk/rig/master/rig.zsh";

/// The build's own version, prefixed like a release tag (`v0.9.0`) so it
/// compares directly against `Release.tag`. Fallback only, for when the
/// installed binary can't be interrogated.
fn local_version() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

/// Version of the binary `update_self` will replace, so a locally-built
/// rig (e.g. `./target/release/rig update --self`) can't report itself
/// up to date while the installed rig lags behind.
fn installed_version(bin: &Path) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

/// Last whitespace token of a `rig --version` line, v-prefixed like a
/// release tag so `version_tuple` can compare it directly.
fn parse_version(output: &str) -> Option<String> {
    let version = output.split_whitespace().last()?;
    Some(if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    })
}

/// `v0.9.0` -> `(0, 9, 0)`. Malformed tags degrade to zero so a weird
/// release tag can never look newer than a local dev build.
fn version_tuple(tag: &str) -> (u64, u64, u64) {
    let parts: Vec<u64> = tag
        .strip_prefix('v')
        .unwrap_or(tag)
        .split('.')
        .filter_map(|p| p.parse().ok())
        .collect();
    match parts.as_slice() {
        [a, b, c] => (*a, *b, *c),
        _ => (0, 0, 0),
    }
}

/// `/releases/latest` redirects to `/releases/tag/<tag>` — the web
/// route, unlike the API, has no 60 requests/hour anonymous quota.
fn latest_tag() -> anyhow::Result<String> {
    let url = format!("{RELEASES_URL}/latest");
    let response = ureq::get(&url)
        .header("User-Agent", sources::USER_AGENT)
        .config()
        .max_redirects(0)
        .build()
        .call()
        .with_context(|| format!("failed to query {url}"))?;
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow!("{url} answered {} without a redirect", response.status()))?;
    tag_from_location(location)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{url} redirected to `{location}`, not a release tag"))
}

fn tag_from_location(location: &str) -> Option<&str> {
    let (_, tag) = location.rsplit_once("/releases/tag/")?;
    (!tag.is_empty() && !tag.contains('/')).then_some(tag)
}

/// cargo-dist publishes `<asset>.sha256` as `<hex> *<asset>`.
fn fetch_digest(url: &str) -> anyhow::Result<String> {
    let text = ureq::get(url)
        .header("User-Agent", sources::USER_AGENT)
        .call()
        .with_context(|| format!("failed to fetch {url}"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("failed to read response body from {url}"))?;
    parse_sha256_file(&text)
        .map(|hex| format!("sha256:{hex}"))
        .ok_or_else(|| anyhow!("{url} holds no sha256 digest"))
}

fn parse_sha256_file(text: &str) -> Option<&str> {
    let hex = text.split_whitespace().next()?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex)
}

/// Fetches the canonical `rig.zsh` from the repo's raw URL. The raw
/// endpoint publishes no digest; a body shorter than its content-length
/// (or an empty one) is refused rather than persisted.
fn fetch_bootstrap() -> anyhow::Result<String> {
    let mut response = ureq::get(BOOTSTRAP_URL)
        .header("User-Agent", sources::USER_AGENT)
        .call()
        .with_context(|| format!("failed to fetch {BOOTSTRAP_URL}"))?;
    // None for chunked and transparently decompressed bodies — when
    // present the comparison is byte-exact.
    let expected = response.body().content_length();
    let text = response
        .body_mut()
        .read_to_string()
        .with_context(|| format!("failed to read response body from {BOOTSTRAP_URL}"))?;
    let len = text.len() as u64;
    if let Some(expected) = expected
        && len != expected
    {
        bail!(
            "downloaded {len} bytes from {BOOTSTRAP_URL}, expected {expected} \
             (truncated download or file changed upstream)"
        );
    }
    if text.trim().is_empty() {
        bail!("{BOOTSTRAP_URL} returned an empty body");
    }
    Ok(text)
}

/// Replaces `~/.local/bin/rig` with the freshly downloaded build, then
/// refreshes the bootstrap. The bootstrap is refreshed only on a real
/// update — never on a no-op `--self` — so a stale fetched `rig.zsh`
/// can't overwrite a newer local one.
pub fn update_self(force: bool, layout: &Layout, lock: &mut Lock) -> anyhow::Result<Outcome> {
    // Fail with a clear message instead of a 404 on a missing archive.
    let Some(target) = SELF_TARGET else {
        bail!("rig publishes no release build for this platform — upgrade rig manually");
    };

    let tag = latest_tag().context("failed to resolve the latest rig release")?;
    let live = layout.prefix_bin_dir.join("rig");
    let from = installed_version(&live).unwrap_or_else(local_version);
    // An installed build newer than the latest release (e.g. cargo-installed
    // master) must never be downgraded by `--self`.
    if !force && version_tuple(&from) >= version_tuple(&tag) {
        return Ok(Outcome::UpToDate { version: from });
    }

    let archive = format!("rig-{target}.tar.gz");
    let archive_url = format!("{RELEASES_URL}/download/{tag}/{archive}");
    let digest = fetch_digest(&format!("{archive_url}.sha256"))?;
    // Every network step finishes before anything goes live — a failure
    // leaves the current rig fully intact.
    eprintln!("rig: downloading rig.zsh");
    let bootstrap = fetch_bootstrap()?;
    // Staging next to the live binary keeps the final rename on one
    // filesystem; overwriting a running binary is safe on Unix.
    let tmp_tar = layout.prefix_bin_dir.join(".rig-self-update.tar.gz");
    // `.d` suffix keeps this clear of the file older rigs left at
    // `.rig-self-update` after a failed download.
    let tmp_dir = layout.prefix_bin_dir.join(".rig-self-update.d");
    eprintln!("rig: downloading {archive}");
    sources::download("rig", &archive_url, &tmp_tar, None, Some(&digest))
        .with_context(|| format!("failed to download {archive}"))?;
    sources::extract(&tmp_tar, &tmp_dir, None)
        .with_context(|| format!("failed to unpack {archive}"))?;
    let tmp_bin = tmp_dir.join("rig");
    if let Err(e) = sources::run_version_smoke(&tmp_bin, "rig") {
        fs::remove_file(&tmp_tar).ok();
        fs::remove_dir_all(&tmp_dir).ok();
        return Err(e);
    }

    // The new build passed its smoke test — land it, then the bootstrap.
    fs::rename(&tmp_bin, &live)
        .with_context(|| format!("failed to move the new build into {}", live.display()))?;
    atomic_write(&layout.state_dir.join("rig.zsh"), &bootstrap)?;
    fs::remove_file(&tmp_tar).ok();
    fs::remove_dir_all(&tmp_dir).ok();
    // rig no longer lives in `rig.lock` — a leftover entry from the old
    // `[[repo]] gitduk/rig` config era is retired here.
    lock.tool.remove("rig");

    Ok(Outcome::Updated { from, to: tag })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_version_is_v_prefixed() {
        assert_eq!(local_version(), format!("v{}", env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn installed_version_parses_rig_output() {
        assert_eq!(parse_version("rig 0.10.0\n"), Some("v0.10.0".to_string()));
        assert_eq!(parse_version("rig v0.9.0\n"), Some("v0.9.0".to_string()));
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn version_tuple_compares_numerically() {
        assert!(version_tuple("v0.10.0") > version_tuple("v0.9.0"));
        assert!(version_tuple("v0.9.0") == version_tuple("0.9.0"));
        assert_eq!(version_tuple("v0.9.0"), version_tuple("v0.9.0"));
        // A malformed tag can't masquerade as a newer version.
        assert!(version_tuple("garbage") < version_tuple("v0.1.0"));
    }

    #[test]
    fn tag_from_location_takes_the_last_segment() {
        let base = "https://github.com/gitduk/rig/releases";
        assert_eq!(
            tag_from_location(&format!("{base}/tag/v1.0.4")),
            Some("v1.0.4")
        );
        // No release yet: GitHub bounces to the release list.
        assert_eq!(tag_from_location(base), None);
        assert_eq!(tag_from_location(&format!("{base}/tag/")), None);
    }

    #[test]
    #[ignore = "hits the real GitHub release page; run with `cargo test -- --ignored`"]
    fn resolves_the_real_latest_tag() {
        let tag = latest_tag().expect("gitduk/rig should have a latest release");
        assert!(tag.starts_with('v'), "unexpected tag: {tag}");
    }

    #[test]
    fn parse_sha256_file_reads_the_dist_format() {
        let hex = "5826645c49fbbe388a0d40f0ff6eba36c5ae2cb84b707255f8b4874353cbebd8";
        assert_eq!(
            parse_sha256_file(&format!("{hex} *rig.tar.gz\n")),
            Some(hex)
        );
        assert_eq!(parse_sha256_file(hex), Some(hex));
        assert_eq!(parse_sha256_file("Not Found"), None);
        assert_eq!(parse_sha256_file(""), None);
    }
}
