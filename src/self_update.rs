//! `rig update --self` — rig manages its own binary and bootstrap without
//! a config entry. Version knowledge lives here (mirrored by the hardcoded
//! archive name in the `rig.zsh` bootstrap); nothing in `~/.rig.toml`
//! decides how rig updates itself.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, anyhow, bail};

use crate::config::Host;
use crate::lock::{Lock, atomic_write};
use crate::paths::Layout;
use crate::sources;
use crate::update::Outcome;
use crate::version::github::{self, Asset};

/// cargo-dist ships the linux-x86_64 build as `rig-<target>.tar.gz`
/// (unpacking to `rig-<target>/rig`), not as a bare binary anymore.
const SELF_ARCHIVE: &str = "rig-x86_64-unknown-linux-gnu.tar.gz";

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

fn pick_asset<'a>(release: &'a github::Release, name: &str) -> anyhow::Result<&'a Asset> {
    release
        .assets
        .iter()
        .find(|a| a.name == name)
        .ok_or_else(|| {
            let list: String = release
                .assets
                .iter()
                .map(|a| format!("\n    - {}", a.name))
                .collect();
            anyhow!(
                "release {} has no `{name}` asset; available:{list}",
                release.tag
            )
        })
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
    // The release publishes a single linux-x86_64 build; fail with a clear
    // message instead of an exec-format error after the download.
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        bail!(
            "`rig update --self` publishes only a linux-x86_64 build — upgrade rig manually on this platform"
        );
    }

    let release = github::latest_release("gitduk/rig", Host::Github)
        .context("failed to resolve the latest rig release")?;
    let live = layout.prefix_bin_dir.join("rig");
    let from = installed_version(&live).unwrap_or_else(local_version);
    // An installed build newer than the latest release (e.g. cargo-installed
    // master) must never be downgraded by `--self`.
    if !force && version_tuple(&from) >= version_tuple(&release.tag) {
        return Ok(Outcome::UpToDate { version: from });
    }

    let archive = pick_asset(&release, SELF_ARCHIVE)?;
    // The bootstrap fetch and the archive download are the only network
    // steps, and both finish before anything goes live — a failure leaves
    // the current rig fully intact.
    eprintln!("rig: downloading rig.zsh");
    let bootstrap = fetch_bootstrap()?;
    // Staging next to the live binary keeps the final rename on one
    // filesystem; overwriting a running binary is safe on Unix.
    let tmp_tar = layout.prefix_bin_dir.join(".rig-self-update.tar.gz");
    // `.d` suffix keeps this clear of the file older rigs left at
    // `.rig-self-update` after a failed download.
    let tmp_dir = layout.prefix_bin_dir.join(".rig-self-update.d");
    eprintln!("rig: downloading {SELF_ARCHIVE}");
    sources::download(
        "rig",
        &archive.url,
        &tmp_tar,
        archive.size,
        archive.digest.as_deref(),
    )
    .with_context(|| format!("failed to download {SELF_ARCHIVE}"))?;
    sources::extract(&tmp_tar, &tmp_dir, None)
        .with_context(|| format!("failed to unpack {SELF_ARCHIVE}"))?;
    // dist tarballs nest the binary one level down: `rig-<target>/rig`.
    let tmp_bin = tmp_dir
        .join(SELF_ARCHIVE.trim_end_matches(".tar.gz"))
        .join("rig");
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

    Ok(Outcome::Updated {
        from,
        to: release.tag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_with(assets: &[&str]) -> github::Release {
        github::Release {
            tag: "v9.9.9".to_string(),
            assets: assets
                .iter()
                .map(|n| Asset {
                    name: n.to_string(),
                    url: format!("https://example.com/{n}"),
                    size: 1,
                    digest: None,
                })
                .collect(),
        }
    }

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
    fn pick_asset_finds_the_named_asset() {
        let release = release_with(&["rig-aarch64-unknown-linux-gnu.tar.gz", SELF_ARCHIVE]);
        assert_eq!(
            pick_asset(&release, SELF_ARCHIVE)
                .expect("asset should be found")
                .name,
            SELF_ARCHIVE
        );
    }

    #[test]
    fn pick_asset_lists_available_assets_on_miss() {
        let release = release_with(&["rig-aarch64-unknown-linux-gnu.tar.gz"]);
        let err = pick_asset(&release, SELF_ARCHIVE).unwrap_err().to_string();
        assert!(err.contains(SELF_ARCHIVE));
        assert!(err.contains("rig-aarch64-unknown-linux-gnu.tar.gz"));
    }
}
