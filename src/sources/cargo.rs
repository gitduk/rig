//! `[[cargo]]`: `cargo install`, version resolved via crates.io. Install
//! location is set via `--root` (a CLI flag, not an inherited env var) —
//! it never depends on what the caller's shell happens to have.

use std::fs;
use std::process::Command;

use anyhow::{Context, bail};
use time::OffsetDateTime;

use crate::config::CargoEntry;
use crate::lock::{Lock, ToolLock};
use crate::paths::Layout;
use crate::version::crates_io;

use super::{Phase, apply_env, collect_artifacts, finalize_partial, run_setup, tool_key};

pub fn install(
    entry: &CargoEntry,
    layout: &Layout,
    lock: &Lock,
    phase: Phase,
) -> anyhow::Result<ToolLock> {
    let tool = tool_key(&entry.name);
    println!("installing {} via cargo", entry.name);
    let version = crates_io::latest_version(&entry.name)
        .with_context(|| format!("failed to resolve latest version for {}", entry.name))?;

    let tool_dir = layout.tool_pkg_dir(tool);
    let final_dir = layout.pkg_dir(tool, &version);
    let partial_dir = tool_dir.join(format!("{version}.partial"));
    if partial_dir.exists() {
        fs::remove_dir_all(&partial_dir)
            .with_context(|| format!("failed to remove stale {}", partial_dir.display()))?;
    }

    println!("  cargo install --version {version} {}", entry.name);
    let mut command = Command::new("cargo");
    command
        .arg("install")
        .arg("--root")
        .arg(&partial_dir)
        .args(["--version", &version, &entry.name]);
    apply_env(&mut command, &entry.common.env)?;
    let status = command
        .status()
        .with_context(|| format!("failed to run `cargo install {}`", entry.name))?;
    if !status.success() {
        bail!("cargo install {} failed ({status})", entry.name);
    }

    if let Some(setup) = &entry.common.setup {
        run_setup(setup, &partial_dir, phase, &entry.common.env)
            .with_context(|| format!("setup hook failed for {tool}"))?;
    }

    // Collect (and conflict-check) before the atomic rename.
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
        version,
        source: format!("cargo:{}", entry.name),
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
        asset: None,
        size: None,
        pkg: Some(final_dir.display().to_string()),
        manager: None,
        root: None,
        eval_cacheable,
        eval_cached_output,
        eval_evidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, Common, CompletionsSpec};

    fn ripgrep_entry() -> CargoEntry {
        CargoEntry {
            name: "ripgrep".to_string(),
            common: Common {
                bin: Some(crate::config::BinSpec::Single("rg".to_string())),
                completions: CompletionsSpec::Enabled(false),
                ..config::test_common()
            },
        }
    }

    #[test]
    #[ignore = "compiles a real crate from source; run with `cargo test -- --ignored` (slow)"]
    fn installs_ripgrep_into_a_scratch_layout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).expect("mkdir home");
        let layout = Layout::new(&home, "~/.local");

        let entry = ripgrep_entry();
        let lock = Lock::default();
        let tool_lock = install(&entry, &layout, &lock, Phase::Install)
            .expect("ripgrep should install cleanly");

        assert_eq!(tool_lock.bins.len(), 1);
        assert!(std::path::Path::new(&tool_lock.bins[0]).is_symlink());
    }
}
