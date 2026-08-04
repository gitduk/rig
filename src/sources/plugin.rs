//! `[[plugin]]` — Job B. Rig only clones/updates; zsh sources the
//! result directly, so there's no build step and no bin/completions
//! collection here (unlike every other source type in this module).
//! No `rig.lock` entry either — "installed?" is just "does the dir exist".

use std::process::Command;

use anyhow::{Context, bail};

use crate::config::PluginEntry;
use crate::paths::Layout;

use super::tool_key;

pub fn install(entry: &PluginEntry, layout: &Layout) -> anyhow::Result<()> {
    let dest = layout.plugins_dir.join(tool_key(&entry.name));
    if dest.exists() {
        bail!("{} already cloned; use update() to pull", dest.display());
    }

    let url = format!("https://github.com/{}.git", entry.name);
    let status = Command::new("git")
        .args(["clone", "--quiet", &url])
        .arg(&dest)
        .status()
        .with_context(|| format!("failed to run `git clone {url}`"))?;
    if !status.success() {
        bail!("git clone {url} failed ({status})");
    }
    Ok(())
}

pub fn update(entry: &PluginEntry, layout: &Layout) -> anyhow::Result<()> {
    let dest = layout.plugins_dir.join(tool_key(&entry.name));
    let status = Command::new("git")
        .args(["pull", "--quiet"])
        .current_dir(&dest)
        .status()
        .with_context(|| format!("failed to run `git pull` in {}", dest.display()))?;
    if !status.success() {
        bail!("git pull in {} failed ({status})", dest.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn autosuggestions_entry() -> PluginEntry {
        PluginEntry {
            name: "zsh-users/zsh-autosuggestions".to_string(),
            source: Some("zsh-autosuggestions.zsh".to_string()),
            defer: 1,
            atload: None,
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
