//! `rig update --self` — rig manages its own binary and bootstrap without
//! a config entry. Version knowledge lives here (mirrored by the hardcoded
//! URLs in the `rig.zsh` bootstrap and `release.yml`); nothing in
//! `~/.rig.toml` decides how rig updates itself.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, anyhow, bail};

use crate::config::Host;
use crate::lock::{Lock, atomic_write};
use crate::paths::Layout;
use crate::sources;
use crate::update::Outcome;
use crate::version::github::{self, Asset};

/// Binary asset name, mirrored by the bootstrap snippet in `rig.zsh` and
/// `release.yml`'s `cp target/release/rig dist/rig-x86_64-unknown-linux-gnu`.
const SELF_ASSET: &str = "rig-x86_64-unknown-linux-gnu";
/// Bootstrap asset name, refreshed on a real self-update.
const BOOTSTRAP_ASSET: &str = "rig.zsh";

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

/// Replaces `~/.local/bin/rig` with the freshly downloaded build, then
/// refreshes the bootstrap. The bootstrap is refreshed only on a real
/// update — never on a no-op `--self` — so a release asset that lags a
/// newer local checkout can't overwrite a newer local `rig.zsh`.
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

    let bin_asset = pick_asset(&release, SELF_ASSET)?;
    let boot_asset = pick_asset(&release, BOOTSTRAP_ASSET)?;
    // Both downloads (the only network step) happen before anything goes
    // live, so a network failure leaves the current rig fully intact.
    // Same directory as the live binary keeps the final rename on one
    // filesystem; overwriting a running binary is safe on Unix.
    let tmp_bin = layout.prefix_bin_dir.join(".rig-self-update");
    println!("  downloading {SELF_ASSET}");
    sources::download(
        &bin_asset.url,
        &tmp_bin,
        bin_asset.size,
        bin_asset.digest.as_deref(),
    )
    .with_context(|| format!("failed to download {SELF_ASSET}"))?;
    fs::set_permissions(&tmp_bin, fs::Permissions::from_mode(0o755))
        .context("failed to make the downloaded build executable")?;
    if let Err(e) = sources::run_version_smoke(&tmp_bin, SELF_ASSET) {
        fs::remove_file(&tmp_bin).ok();
        return Err(e);
    }

    let tmp_boot = layout.state_dir.join(format!(".{BOOTSTRAP_ASSET}.new"));
    sources::download(
        &boot_asset.url,
        &tmp_boot,
        boot_asset.size,
        boot_asset.digest.as_deref(),
    )
    .with_context(|| format!("failed to download {BOOTSTRAP_ASSET}"))?;

    // Both artifacts are verified on disk — land them.
    fs::rename(&tmp_bin, &live)
        .with_context(|| format!("failed to move the new build into {}", live.display()))?;
    let fresh = fs::read_to_string(&tmp_boot)
        .with_context(|| format!("failed to read {}", tmp_boot.display()))?;
    atomic_write(&layout.state_dir.join(BOOTSTRAP_ASSET), &fresh)?;
    fs::remove_file(&tmp_boot).ok();
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
        let release = release_with(&["rig.zsh", SELF_ASSET]);
        assert_eq!(
            pick_asset(&release, SELF_ASSET)
                .expect("asset should be found")
                .name,
            SELF_ASSET
        );
    }

    #[test]
    fn pick_asset_lists_available_assets_on_miss() {
        let release = release_with(&["rig.zsh"]);
        let err = pick_asset(&release, SELF_ASSET).unwrap_err().to_string();
        assert!(err.contains(SELF_ASSET));
        assert!(err.contains("rig.zsh"));
    }
}
