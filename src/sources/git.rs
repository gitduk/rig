//! `[[git]]` install flow — source build via a locally invoked `git`
//! binary (SIM-1: no `git2`, `git` is already on the system).

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail};
use time::OffsetDateTime;

use crate::config::{GitEntry, Host};
use crate::lock::{Lock, ToolLock};
use crate::paths::Layout;

use super::{Phase, apply_env, collect_artifacts, finalize_partial, run_setup, tool_key};

fn clone_url(host: Host, name: &str) -> String {
    match host {
        Host::Github => format!("https://github.com/{name}.git"),
        Host::Codeberg => format!("https://codeberg.org/{name}.git"),
    }
}

/// Lets the update flow check for a new commit without cloning.
pub fn remote_head_commit(host: Host, name: &str) -> anyhow::Result<String> {
    let url = clone_url(host, name);
    let output = Command::new("git")
        .args(["ls-remote", &url, "HEAD"])
        .output()
        .with_context(|| format!("failed to run `git ls-remote {url} HEAD`"))?;
    if !output.status.success() {
        bail!(
            "git ls-remote {url} HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
        .context("git ls-remote returned no output")
}

fn git_clone(url: &str, dest: &Path, env: &HashMap<String, String>) -> anyhow::Result<()> {
    let mut command = Command::new("git");
    command.args(["clone", "--quiet", url]).arg(dest);
    apply_env(&mut command, env)?;
    let status = command
        .status()
        .with_context(|| format!("failed to run `git clone {url}`"))?;
    if !status.success() {
        bail!("git clone {url} failed ({status})");
    }
    Ok(())
}

fn git_head_commit(repo_dir: &Path) -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_dir)
        .output()
        .context("failed to run `git rev-parse HEAD`")?;
    if !output.status.success() {
        bail!(
            "git rev-parse HEAD failed in {}: {}",
            repo_dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The hook only builds (e.g. `cargo build`), never installs — rig
/// collects `bin`/`completions` from the clone, same as `[[repo]]`.
pub fn install(
    entry: &GitEntry,
    layout: &Layout,
    lock: &Lock,
    phase: Phase,
) -> anyhow::Result<ToolLock> {
    let tool = tool_key(&entry.name);
    let url = clone_url(entry.host, &entry.name);
    let tool_dir = layout.tool_pkg_dir(tool);

    let partial_dir = tool_dir.join("clone.partial");
    if partial_dir.exists() {
        fs::remove_dir_all(&partial_dir)
            .with_context(|| format!("failed to remove stale {}", partial_dir.display()))?;
    }
    println!("installing {} via git (git clone {url})", entry.name);
    git_clone(&url, &partial_dir, &entry.common.env)?;

    if let Some(setup) = &entry.common.setup {
        run_setup(setup, &partial_dir, phase, &entry.common.env)
            .with_context(|| format!("setup hook failed for {tool}"))?;
    }

    let commit = git_head_commit(&partial_dir)?;
    let final_dir = layout.pkg_dir(tool, &commit);

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
        version: commit,
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
    use crate::config::{self, BinSpec, Common, CompletionsSpec};

    #[test]
    fn clone_url_dispatches_by_host() {
        assert_eq!(
            clone_url(Host::Github, "dandavison/delta"),
            "https://github.com/dandavison/delta.git"
        );
        assert_eq!(
            clone_url(Host::Codeberg, "explosion-mental/wallust"),
            "https://codeberg.org/explosion-mental/wallust.git"
        );
    }

    #[test]
    #[ignore = "clones a real (tiny) GitHub repo; run with `cargo test -- --ignored`"]
    fn clones_and_collects_from_a_real_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).expect("mkdir home");
        let layout = Layout::new(&home, "~/.local");

        let entry = GitEntry {
            name: "octocat/Hello-World".to_string(),
            host: Host::Github,
            common: Common {
                bin: Some(BinSpec::Single("README".to_string())),
                completions: CompletionsSpec::Enabled(false),
                ..config::test_common()
            },
        };

        let lock = Lock::default();
        let tool_lock =
            install(&entry, &layout, &lock, Phase::Install).expect("clone should install cleanly");

        assert_eq!(
            tool_lock.version.len(),
            40,
            "HEAD should be a full commit sha"
        );
        assert_eq!(tool_lock.bins.len(), 1);
        assert!(std::path::Path::new(&tool_lock.bins[0]).is_symlink());
    }
}
