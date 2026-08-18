//! `[[plugin]]` — rig only clones/updates; zsh sources the result
//! directly, so there's no build step and no bin/completions collection
//! here (unlike every other source type in this module). Still produces
//! a `ToolLock` like the other six, tracked in `rig.lock` the same way.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail};
use time::OffsetDateTime;

use crate::config::PluginEntry;
use crate::lock::ToolLock;
use crate::paths::Layout;

use super::{git_head_commit, tool_key};

pub fn install(entry: &PluginEntry, layout: &Layout) -> anyhow::Result<ToolLock> {
    let dest = layout.plugins_dir.join(tool_key(&entry.name));
    if dest.exists() {
        bail!("{} already cloned; use update() to pull", dest.display());
    }

    eprintln!(
        "{}: installing via git clone ({})",
        tool_key(&entry.name),
        entry.name
    );
    let url = format!("https://github.com/{}.git", entry.name);
    let status = Command::new("git")
        .args(["clone", "--quiet", &url])
        .arg(&dest)
        .status()
        .with_context(|| format!("failed to run `git clone {url}`"))?;
    if !status.success() {
        bail!("git clone {url} failed ({status})");
    }
    build_lock(entry, &dest)
}

pub fn update(entry: &PluginEntry, layout: &Layout) -> anyhow::Result<ToolLock> {
    let dest = layout.plugins_dir.join(tool_key(&entry.name));
    eprintln!(
        "{}: updating via git pull ({})",
        tool_key(&entry.name),
        entry.name
    );
    let status = Command::new("git")
        .args(["pull", "--quiet"])
        .current_dir(&dest)
        .status()
        .with_context(|| format!("failed to run `git pull` in {}", dest.display()))?;
    if !status.success() {
        bail!("git pull in {} failed ({status})", dest.display());
    }
    build_lock(entry, &dest)
}

/// Reads an already-cloned plugin's current commit into a `ToolLock`
/// without touching it — backfills `rig.lock` for clones that predate
/// plugin tracking, so `sync` doesn't reclone what's already on disk.
pub fn read_installed(entry: &PluginEntry, layout: &Layout) -> anyhow::Result<ToolLock> {
    let dest = layout.plugins_dir.join(tool_key(&entry.name));
    build_lock(entry, &dest)
}

fn build_lock(entry: &PluginEntry, dest: &Path) -> anyhow::Result<ToolLock> {
    Ok(ToolLock {
        version: git_head_commit(dest)?,
        source: format!("plugin:{}", entry.name),
        installed_at: OffsetDateTime::now_utc(),
        bins: Vec::new(),
        completions: Vec::new(),
        asset: None,
        size: None,
        pkg: Some(dest.display().to_string()),
        manager: None,
        root: None,
        eval_cacheable: None,
        eval_cached_output: None,
        eval_evidence: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn autosuggestions_entry() -> PluginEntry {
        PluginEntry {
            name: "zsh-users/zsh-autosuggestions".to_string(),
            description: None,
            source: Some("zsh-autosuggestions.zsh".to_string()),
            run: None,
        }
    }

    #[test]
    #[ignore = "clones a real (tiny) GitHub repo; run with `cargo test -- --ignored`"]
    fn clones_then_updates_a_real_plugin() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).expect("mkdir home");
        let layout = Layout::new(&home, "~/.local");
        let entry = autosuggestions_entry();

        install(&entry, &layout).expect("clone should succeed");
        let dest = layout.plugins_dir.join("zsh-autosuggestions");
        assert!(dest.join("zsh-autosuggestions.zsh").exists());

        // Idempotent-guard: a second install() must refuse, not clobber.
        assert!(install(&entry, &layout).is_err());

        update(&entry, &layout).expect("pull should succeed on an existing clone");
    }
}
