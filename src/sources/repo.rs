//! `[[repo]]` install flow — GitHub/Codeberg release binaries.

use std::fs;
use std::path::Path;

use anyhow::{Context, anyhow};
use time::OffsetDateTime;

use crate::config::{self, RepoEntry};
use crate::lock::{Lock, ToolLock};
use crate::output::{progress, warning};
use crate::paths::Layout;
use crate::version::github::{self, Asset};

use super::{
    AssetPick, Phase, collect_artifacts, download, extract, finalize_partial, glob_match,
    pick_ambiguous, run_setup, select_asset, tool_key,
};

/// Runs the full install flow; regenerating init.zsh is the caller's job.
/// Returns the new lock entry — the caller inserts it and saves `rig.lock`.
pub fn install(
    entry: &RepoEntry,
    layout: &Layout,
    lock: &Lock,
    phase: Phase,
) -> anyhow::Result<ToolLock> {
    let tool = tool_key(&entry.name);
    let release = github::latest_release(&entry.name, entry.host)
        .with_context(|| format!("failed to resolve latest release for {}", entry.name))?;
    progress!(
        "{tool}: {} via {} release ({})",
        phase.describe_doing(&release.tag),
        entry.host.prefix(),
        entry.name
    );

    let asset = match select_asset(&release.assets, entry.bpick.as_ref()) {
        AssetPick::Found(asset) => asset,
        AssetPick::NoMatch {
            bpick_configured,
            relevant,
            all,
        } => {
            let headline = if bpick_configured {
                "no release asset matches bpick"
            } else {
                "no release asset auto-matches your platform — bpick not set"
            };
            let body = no_match_body(
                entry,
                layout,
                &release.tag,
                bpick_configured,
                &relevant,
                all,
            );
            return Err(anyhow!(body).context(headline));
        }
        AssetPick::Ambiguous(matches) => {
            let asset = pick_ambiguous(tool, &matches, entry.bpick.is_some())?;
            suggest_narrowed_bpick(tool, &release.assets, asset, entry, layout, &release.tag);
            asset
        }
    };

    progress!("{tool}: asset {}", asset.name);
    let cache_path = layout.cache_dir.join(&asset.name);
    download(
        tool,
        &asset.url,
        &cache_path,
        Some(asset.size),
        asset.digest.as_deref(),
    )
    .with_context(|| format!("failed to download {}", asset.name))?;

    let tool_dir = layout.tool_pkg_dir(tool);
    let final_dir = layout.pkg_dir(tool, &release.tag);
    let partial_dir = tool_dir.join(format!("{}.partial", release.tag));
    let extracted = extract(&cache_path, &partial_dir, entry.extract);
    // `download` never reuses a cached archive, so it's dead weight now.
    fs::remove_file(&cache_path).ok();
    extracted.with_context(|| format!("failed to extract {}", asset.name))?;

    if let Some(setup) = &entry.common.setup {
        run_setup(tool, setup, &partial_dir, phase, &entry.common.env)
            .with_context(|| format!("setup hook failed for {tool}"))?;
    }

    // Only for rig's own entry (by binary identity, not by repo owner, so
    // forks still match) — a broken release must never go live unverified.
    if tool == env!("CARGO_PKG_NAME") {
        super::smoke_test_version(&partial_dir, entry.common.bin.as_ref(), tool)
            .with_context(|| "new build failed its smoke test — keeping the current rig live")?;

        adopt_bootstrap_binary(&layout.prefix_bin_dir.join(tool))?;
    }

    // Collect (and thus conflict-check) *before* the atomic rename — a
    // failure here must never have already destroyed the old version.
    let artifacts = collect_artifacts(
        &partial_dir,
        &final_dir,
        &entry.common,
        tool,
        &layout.prefix_bin_dir,
        &layout.completions_dir,
        lock,
    )?;
    finalize_partial(&partial_dir, &final_dir)?;

    // Only after finalize: the eval command may resolve through the symlink
    // collect_bins just created, which points at final_dir, not partial_dir.
    let (eval_cacheable, eval_cached_output, eval_evidence) =
        super::resolve_eval_cache(tool, entry.common.eval.as_ref());

    Ok(ToolLock {
        version: release.tag,
        source: format!("{}:{}", entry.host.prefix(), entry.name),
        installed_at: OffsetDateTime::now_utc(),
        bins: artifacts
            .bins
            .iter()
            .map(|(_, target)| target.display().to_string())
            .collect(),
        completions: artifacts
            .completions
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        asset: Some(asset.name.clone()),
        size: Some(asset.size),
        pkg: Some(final_dir.display().to_string()),
        manager: None,
        root: None,
        eval_cacheable,
        eval_cached_output,
        eval_evidence,
    })
}

/// Adopt a raw binary left by the bootstrap snippet; a real symlink here
/// means rig already owns it — leave that to `check_conflict`.
fn adopt_bootstrap_binary(live: &Path) -> anyhow::Result<()> {
    if live.exists() && !live.is_symlink() {
        fs::remove_file(live)
            .with_context(|| format!("failed to remove the pre-managed {}", live.display()))?;
    }
    Ok(())
}

/// Builds the config-diff body for a failed asset pick — the one place
/// with both the release version and config path needed to suggest a fix.
fn no_match_body(
    entry: &RepoEntry,
    layout: &Layout,
    version: &str,
    bpick_configured: bool,
    relevant: &[&Asset],
    all: &[Asset],
) -> String {
    if let Some(value) = suggest_bpick(relevant, version)
        && let Some(diff) =
            config::render_entry_diff(&layout.config_path, &entry.name, "bpick", Some(&value))
    {
        return format!("{}\n{diff}", layout.config_path.display());
    }

    let candidates: Vec<&Asset> = if relevant.is_empty() {
        all.iter().collect()
    } else {
        relevant.to_vec()
    };
    let list: String = candidates
        .iter()
        .map(|a| format!("\n    - {}", a.name))
        .collect();
    let suffix = if bpick_configured {
        ""
    } else {
        " — set `bpick` explicitly"
    };
    format!("no asset matches your platform{suffix}\navailable assets:{list}")
}

/// Shows the `bpick` that would make this pick automatic next release;
/// says so instead when no glob can express the difference (a version
/// prefix is not a negative match).
fn suggest_narrowed_bpick(
    tool: &str,
    assets: &[Asset],
    asset: &Asset,
    entry: &RepoEntry,
    layout: &Layout,
    tag: &str,
) {
    let Some(suggested) = suggest_bpick(std::slice::from_ref(&asset), tag) else {
        return;
    };
    if !narrows_to(&suggested, assets, asset) {
        warning!(
            "{tool}: no `bpick` can single out {} in this release — you'll be asked again next release",
            asset.name
        );
        return;
    }

    let current = entry
        .bpick
        .as_ref()
        .map(config::BpickSpec::patterns)
        .unwrap_or_default();
    if current.contains(&suggested.as_str()) {
        return;
    }
    if let Some(diff) =
        config::render_entry_diff(&layout.config_path, &entry.name, "bpick", Some(&suggested))
    {
        warning!(
            "{tool}: picked {} — set `bpick` to skip this prompt next time\n{diff}",
            asset.name
        );
    }
}

/// True when `pattern` selects `asset` and nothing else in the release.
fn narrows_to(pattern: &str, assets: &[Asset], asset: &Asset) -> bool {
    let mut matched = assets.iter().filter(|a| glob_match(pattern, &a.name));
    matches!((matched.next(), matched.next()), (Some(only), None) if only.name == asset.name)
}

/// `foo-15.2.0-linux.tar.gz` -> `foo-*-linux.tar.gz`, so the fix survives
/// future releases instead of pinning this exact version.
fn suggest_bpick(relevant: &[&Asset], version: &str) -> Option<String> {
    // Tags are often `v`-prefixed while asset names carry the bare version.
    let version = version.strip_prefix('v').unwrap_or(version);
    let extractable = |name: &str| {
        name.ends_with(".tar.gz")
            || name.ends_with(".tgz")
            || name.ends_with(".zip")
            || name.ends_with(".deb")
    };
    let candidate = relevant.iter().find(|a| extractable(&a.name))?;
    candidate
        .name
        .contains(version)
        .then(|| candidate.name.replacen(version, "*", 1))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::{BpickSpec, Common, Host};

    #[test]
    fn tool_key_takes_the_last_path_segment() {
        assert_eq!(tool_key("dandavison/delta"), "delta");
        assert_eq!(tool_key("delta"), "delta");
    }

    #[test]
    fn adopt_bootstrap_binary_removes_a_plain_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let live = tmp.path().join("rig");
        fs::write(&live, b"bootstrap binary").expect("write fake bootstrap binary");

        adopt_bootstrap_binary(&live).expect("plain file should be adopted");

        assert!(!live.exists());
    }

    #[test]
    fn adopt_bootstrap_binary_leaves_an_existing_symlink_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("real-rig");
        fs::write(&target, b"managed binary").expect("write managed binary");
        let live = tmp.path().join("rig");
        std::os::unix::fs::symlink(&target, &live).expect("symlink managed binary");

        adopt_bootstrap_binary(&live).expect("an existing symlink must be a no-op");

        assert!(live.is_symlink());
    }

    #[test]
    fn adopt_bootstrap_binary_is_a_noop_when_nothing_is_there() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let live = tmp.path().join("rig");

        adopt_bootstrap_binary(&live).expect("a missing path must be a no-op");
    }

    fn delta_entry() -> RepoEntry {
        RepoEntry {
            name: "dandavison/delta".to_string(),
            host: Host::Github,
            bpick: Some(BpickSpec::Single(
                "*-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            )),
            extract: None,
            common: Common {
                ..config::test_common()
            },
        }
    }

    fn test_asset(name: &str) -> Asset {
        Asset {
            name: name.to_string(),
            url: String::new(),
            size: 0,
            digest: None,
        }
    }

    #[test]
    fn suggest_bpick_strips_a_v_prefixed_tag() {
        let assets = [test_asset("DBX_0.6.68_amd64.deb")];
        let refs: Vec<&Asset> = assets.iter().collect();

        assert_eq!(
            suggest_bpick(&refs, "v0.6.68").as_deref(),
            Some("DBX_*_amd64.deb")
        );
        assert_eq!(suggest_bpick(&refs, "v0.6.69"), None);
    }

    #[test]
    fn no_match_body_suggests_a_pattern_for_a_v_prefixed_tag() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(tmp.path(), "~/.local");
        fs::write(&layout.config_path, "[[repo]]\nname = \"t8y2/dbx\"\n").expect("write config");
        let entry = RepoEntry {
            name: "t8y2/dbx".to_string(),
            host: Host::Github,
            bpick: None,
            extract: None,
            common: Common {
                ..config::test_common()
            },
        };
        let assets = [test_asset("DBX_0.6.68_amd64.deb")];
        let relevant: Vec<&Asset> = assets.iter().collect();

        let body = no_match_body(&entry, &layout, "v0.6.68", false, &relevant, &assets);

        assert!(
            body.contains("+ bpick = \"DBX_*_amd64.deb\""),
            "unexpected body:\n{body}"
        );
    }

    #[test]
    fn narrows_to_rejects_a_pattern_that_still_matches_a_sibling() {
        let assets = [
            test_asset("DBX_0.6.68_amd64.deb"),
            test_asset("DBX_0.6.68-beta.20261107.4_amd64.deb"),
        ];
        let stable = &assets[0];

        assert!(!narrows_to("DBX_*_amd64.deb", &assets, stable));
        assert!(narrows_to("DBX_0.6.68_amd64.deb", &assets, stable));
    }

    #[test]
    #[ignore = "hits the real GitHub API and downloads a real release; run with `cargo test -- --ignored`"]
    fn installs_delta_into_a_scratch_layout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).expect("mkdir home");
        let layout = Layout::new(&home, "~/.local");
        let entry = delta_entry();

        let lock = Lock::default();
        let tool_lock =
            install(&entry, &layout, &lock, Phase::Install).expect("delta should install cleanly");

        assert_eq!(tool_lock.bins.len(), 1);
        let bin_path = PathBuf::from(&tool_lock.bins[0]);
        assert!(bin_path.is_symlink());
        assert!(bin_path.exists(), "symlink should resolve to a real file");

        // Reinstalling the same tool must not trip the untracked-conflict check.
        let mut lock2 = Lock::default();
        lock2.tool.insert("delta".to_string(), tool_lock);
        install(&entry, &layout, &lock2, Phase::Install)
            .expect("reinstall over our own tracked bins should succeed");
    }
}
