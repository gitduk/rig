//! `rig remove` only deletes symlinks that still point into rig's own
//! `state_dir`. If the lockfile drifted (or a name got reused by something
//! else since), the file is left alone instead of guessed-at.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail};

use crate::lock::ToolLock;
use crate::paths::Layout;
use crate::sources::node::ResolvedManager;

pub fn remove(tool: &ToolLock, layout: &Layout) -> anyhow::Result<()> {
    if let Some(manager) = &tool.manager {
        uninstall_node_package(tool, manager)?;
    } else if tool.source.starts_with("apt:") {
        uninstall_apt_package(tool)?;
    }
    for bin in &tool.bins {
        remove_owned_symlink(Path::new(bin), layout)?;
    }
    for completion in &tool.completions {
        remove_owned_symlink(Path::new(completion), layout)?;
    }
    if let Some(pkg) = &tool.pkg {
        let pkg_path = Path::new(pkg);
        if pkg_path.starts_with(&layout.state_dir) && pkg_path.exists() {
            fs::remove_dir_all(pkg_path)
                .with_context(|| format!("failed to remove {}", pkg_path.display()))?;
        }
    }
    Ok(())
}

/// A node package's bins/root live outside `state_dir` (bun/npm own that
/// tree), so only the manager can remove them.
fn uninstall_node_package(tool: &ToolLock, manager: &str) -> anyhow::Result<()> {
    let name = tool
        .source
        .strip_prefix("node:")
        .with_context(|| format!("malformed node source in rig.lock: {}", tool.source))?;
    let (bin, verb) = match ResolvedManager::parse(manager)? {
        ResolvedManager::Bun => ("bun", "remove"),
        ResolvedManager::Npm => ("npm", "uninstall"),
    };
    let status = Command::new(bin)
        .args([verb, "-g", name])
        .status()
        .with_context(|| format!("failed to run `{bin} {verb} -g {name}`"))?;
    if !status.success() {
        bail!("`{bin} {verb} -g {name}` failed ({status})");
    }
    Ok(())
}

/// An apt package's files are placed by dpkg outside `state_dir` (see
/// `sources::apt`'s doc comment), so only `apt-get remove` can undo it.
fn uninstall_apt_package(tool: &ToolLock) -> anyhow::Result<()> {
    let name = tool
        .source
        .strip_prefix("apt:")
        .with_context(|| format!("malformed apt source in rig.lock: {}", tool.source))?;
    eprintln!("removing {name} via `sudo apt-get remove -y`");
    let status = Command::new("sudo")
        .args(["apt-get", "remove", "-y", name])
        .status()
        .with_context(|| format!("failed to run `sudo apt-get remove -y {name}`"))?;
    if !status.success() {
        bail!("apt-get remove -y {name} failed ({status})");
    }
    Ok(())
}

fn remove_owned_symlink(path: &Path, layout: &Layout) -> anyhow::Result<()> {
    if !path.is_symlink() {
        return Ok(());
    }

    let target = fs::read_link(path)
        .with_context(|| format!("failed to read symlink {}", path.display()))?;
    let resolved = if target.is_absolute() {
        target
    } else {
        path.parent().unwrap_or(Path::new("/")).join(target)
    };

    if !resolved.starts_with(&layout.state_dir) {
        return Ok(());
    }

    fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use time::OffsetDateTime;

    fn tool_lock(bins: Vec<String>, pkg: Option<String>) -> ToolLock {
        ToolLock {
            version: "1.0.0".to_string(),
            source: "github:test/test".to_string(),
            installed_at: OffsetDateTime::UNIX_EPOCH,
            bins,
            completions: Vec::new(),
            asset: None,
            size: None,
            pkg,
            manager: None,
            root: None,
            eval_cacheable: None,
            eval_cached_output: None,
            eval_evidence: Vec::new(),
        }
    }

    #[test]
    fn uninstall_apt_package_rejects_a_malformed_source() {
        let mut tool = tool_lock(Vec::new(), None);
        tool.source = "cargo:ripgrep".to_string();
        let err = uninstall_apt_package(&tool).expect_err("source has no apt: prefix");
        assert!(err.to_string().contains("cargo:ripgrep"));
    }

    #[test]
    fn uninstall_node_package_rejects_a_malformed_source() {
        let mut tool = tool_lock(Vec::new(), None);
        tool.source = "cargo:ripgrep".to_string();
        let err = uninstall_node_package(&tool, "bun").expect_err("source has no node: prefix");
        assert!(err.to_string().contains("cargo:ripgrep"));
    }

    #[test]
    fn uninstall_node_package_rejects_an_unknown_manager() {
        let mut tool = tool_lock(Vec::new(), None);
        tool.source = "node:cowsay".to_string();
        let err = uninstall_node_package(&tool, "yarn").expect_err("yarn isn't a known manager");
        assert!(err.to_string().contains("yarn"));
    }

    #[test]
    fn removes_symlink_pointing_into_state_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).expect("mkdir home");
        let layout = Layout::new(&home, "~/.local");

        let pkg_dir = layout.pkg_dir("delta", "0.18.2");
        fs::create_dir_all(&pkg_dir).expect("mkdir pkg dir");
        let real_bin = pkg_dir.join("delta");
        fs::write(&real_bin, b"binary").expect("write fake binary");

        fs::create_dir_all(&layout.prefix_bin_dir).expect("mkdir prefix bin dir");
        let bin_link = layout.prefix_bin_dir.join("delta");
        symlink(&real_bin, &bin_link).expect("create symlink");

        let tool = tool_lock(
            vec![bin_link.display().to_string()],
            Some(pkg_dir.display().to_string()),
        );
        remove(&tool, &layout).expect("remove should succeed");

        assert!(!bin_link.exists());
        assert!(!bin_link.is_symlink());
        assert!(!pkg_dir.exists());
    }

    #[test]
    fn leaves_a_symlink_that_points_outside_state_dir_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).expect("mkdir home");
        let layout = Layout::new(&home, "~/.local");

        // Simulates lockfile drift: this bin now points somewhere rig
        // doesn't own (e.g. reinstalled by a different manager).
        let outside = tmp.path().join("not-rigs.bin");
        fs::write(&outside, b"binary").expect("write fake binary");
        fs::create_dir_all(&layout.prefix_bin_dir).expect("mkdir prefix bin dir");
        let bin_link = layout.prefix_bin_dir.join("delta");
        symlink(&outside, &bin_link).expect("create symlink");

        let tool = tool_lock(vec![bin_link.display().to_string()], None);
        remove(&tool, &layout).expect("remove should succeed");

        assert!(
            bin_link.is_symlink(),
            "untracked-target symlink must survive"
        );
    }
}
