//! `[[repo]]` install flow — GitHub/Codeberg release binaries.

use std::fs;
use std::path::Path;

use anyhow::{Context, anyhow, bail};
use time::OffsetDateTime;

use crate::config::{self, RepoEntry};
use crate::lock::{Lock, ToolLock};
use crate::paths::Layout;
use crate::version::github::{self, Asset};

use super::{
    AssetPick, Phase, collect_artifacts, download, extract, finalize_partial, run_setup,
    select_asset, tool_key,
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
    println!(
        "installing {} via {} release",
        entry.name,
        entry.host.prefix()
    );

    let release = github::latest_release(&entry.name, entry.host)
        .with_context(|| format!("failed to resolve latest release for {}", entry.name))?;
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
            let list: String = matches
                .iter()
                .map(|a| format!("\n    - {}", a.name))
                .collect();
            bail!("bpick matches multiple assets, narrow it:{list}");
        }
    };

    println!("  picked asset: {}", asset.name);
    let cache_path = layout.cache_dir.join(&asset.name);
    download(&asset.url, &cache_path, asset.size)
        .with_context(|| format!("failed to download {}", asset.name))?;

    let tool_dir = layout.tool_pkg_dir(tool);
    let final_dir = layout.pkg_dir(tool, &release.tag);
    let partial_dir = tool_dir.join(format!("{}.partial", release.tag));
    if partial_dir.exists() {
        fs::remove_dir_all(&partial_dir)
            .with_context(|| format!("failed to remove stale {}", partial_dir.display()))?;
    }

    extract(&cache_path, &partial_dir, entry.extract)
        .with_context(|| format!("failed to extract {}", asset.name))?;

    if let Some(setup) = &entry.common.setup {
        run_setup(setup, &partial_dir, phase, &entry.common.env)
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
        super::resolve_eval_cache(entry.common.eval.as_ref());

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

/// `foo-15.2.0-linux.tar.gz` -> `foo-*-linux.tar.gz`, so the fix survives
/// future releases instead of pinning this exact version.
fn suggest_bpick(relevant: &[&Asset], version: &str) -> Option<String> {
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
