//! State-directory layout. Path-joining is pure and takes `home` as a
//! parameter rather than reading `$HOME` internally, so it's testable
//! without mutating process environment.

use std::path::{Path, PathBuf};

use anyhow::Context;

pub fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("$HOME is not set")
}

pub fn expand_tilde_in(home: &Path, path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    if path == "~" {
        return home.to_path_buf();
    }
    PathBuf::from(path)
}

/// `prefix_bin_dir` is the one shared, non-`state_dir` path.
pub struct Layout {
    pub config_path: PathBuf,
    pub state_dir: PathBuf,
    pub lock_path: PathBuf,
    pub init_zsh_path: PathBuf,
    pub completions_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub prefix_bin_dir: PathBuf,
    /// `[[plugin]]` clones. Unlike `pkg/`, no version-swap indirection:
    /// zsh just `source`s whatever's here, so `git pull` updates in place.
    pub plugins_dir: PathBuf,
}

impl Layout {
    pub fn new(home: &Path, prefix: &str) -> Self {
        let state_dir = home.join(".local/share/rig");
        let prefix_dir = expand_tilde_in(home, prefix);
        Self {
            config_path: home.join(".config/rig/config.toml"),
            lock_path: state_dir.join("rig.lock"),
            init_zsh_path: state_dir.join("init.zsh"),
            completions_dir: state_dir.join("completions"),
            cache_dir: state_dir.join("cache"),
            prefix_bin_dir: prefix_dir.join("bin"),
            plugins_dir: state_dir.join("plugins"),
            state_dir,
        }
    }

    /// Parent of `pkg_dir(tool, _)` — needed before a version is known yet
    /// (e.g. `[[git]]` clones before it can `git rev-parse HEAD`).
    pub fn tool_pkg_dir(&self, tool: &str) -> PathBuf {
        self.state_dir.join("pkg").join(tool)
    }

    /// `pkg/<tool>/<version>/` — the one indirection layer, for atomic swaps.
    pub fn pkg_dir(&self, tool: &str, version: &str) -> PathBuf {
        self.tool_pkg_dir(tool).join(version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_tilde_prefixed_paths() {
        let home = Path::new("/home/kaige");
        assert_eq!(
            expand_tilde_in(home, "~/.local"),
            PathBuf::from("/home/kaige/.local")
        );
        assert_eq!(expand_tilde_in(home, "~"), PathBuf::from("/home/kaige"));
        assert_eq!(
            expand_tilde_in(home, "/opt/tools"),
            PathBuf::from("/opt/tools")
        );
    }

    #[test]
    fn resolves_layout_under_default_prefix() {
        let home = Path::new("/home/kaige");
        let layout = Layout::new(home, "~/.local");

        assert_eq!(
            layout.config_path,
            PathBuf::from("/home/kaige/.config/rig/config.toml")
        );
        assert_eq!(
            layout.lock_path,
            PathBuf::from("/home/kaige/.local/share/rig/rig.lock")
        );
        assert_eq!(
            layout.plugins_dir,
            PathBuf::from("/home/kaige/.local/share/rig/plugins")
        );
        assert_eq!(
            layout.prefix_bin_dir,
            PathBuf::from("/home/kaige/.local/bin")
        );
        assert_eq!(
            layout.pkg_dir("delta", "0.18.2"),
            PathBuf::from("/home/kaige/.local/share/rig/pkg/delta/0.18.2"),
        );
    }

    #[test]
    fn resolves_layout_under_custom_prefix() {
        let home = Path::new("/home/kaige");
        let layout = Layout::new(home, "/opt/tools");
        assert_eq!(layout.prefix_bin_dir, PathBuf::from("/opt/tools/bin"));
    }
}
