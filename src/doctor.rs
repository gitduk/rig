//! Diagnostic checks. The shared `.crates.toml` split-brain and cross-root
//! duplication aren't checked here: they were bugs in the old
//! zinit-replacement sketch that per-tool `--root` isolation avoids by
//! construction, not something rig's own state can drift into.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{self, CompletionsSpec, Config, SetupSpec};
use crate::evalcache;
use crate::lock::Lock;
use crate::paths::Layout;

const SYSTEM_COMPLETION_DIRS: &[&str] = &[
    "/usr/share/zsh/functions/Completion",
    "/usr/share/zsh/vendor-completions",
    "/usr/share/zsh/site-functions",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Info,
}

#[derive(Debug)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
}

fn warn(message: impl Into<String>) -> Finding {
    Finding {
        severity: Severity::Warning,
        message: message.into(),
    }
}

fn info(message: impl Into<String>) -> Finding {
    Finding {
        severity: Severity::Info,
        message: message.into(),
    }
}

pub fn run(config: &Config, lock: &Lock, layout: &Layout) -> Vec<Finding> {
    let path_env = std::env::var("PATH").unwrap_or_default();

    let mut findings = Vec::new();
    findings.extend(check_config_newer_than_init_zsh(layout));
    findings.extend(check_prefix_bin_on_path(layout, &path_env));
    findings.extend(check_shadowed_installed_bin(lock, layout, &path_env));
    findings.extend(check_lock_vs_disk(lock));
    findings.extend(check_orphan_pkg_dirs(lock, layout));
    findings.extend(check_dangling_symlinks(layout));
    findings.extend(check_foreign_prefix_bin(config, lock, layout));
    findings.extend(check_missing_completions(config, lock));
    findings.extend(check_shadowed_completions(lock, SYSTEM_COMPLETION_DIRS));
    findings.extend(check_sudo_in_setup(config));
    findings.extend(check_unguarded_compdef(lock));
    findings.extend(check_dual_node_managers(config, layout));
    findings.extend(check_eval_cache_drift(config, lock));
    findings
}

/// The shell's stale-guard condition, checked manually: if `config.toml` is
/// newer than the generated `init.zsh`, a shell hasn't re-synced since the edit.
fn check_config_newer_than_init_zsh(layout: &Layout) -> Vec<Finding> {
    let (Ok(config_meta), Ok(init_meta)) = (
        fs::metadata(&layout.config_path),
        fs::metadata(&layout.init_zsh_path),
    ) else {
        return Vec::new();
    };
    let (Ok(config_mtime), Ok(init_mtime)) = (config_meta.modified(), init_meta.modified()) else {
        return Vec::new();
    };

    if config_mtime > init_mtime {
        vec![warn(
            "config.toml changed since the last `rig sync` — run `rig sync` to \
             pick up new tools",
        )]
    } else {
        Vec::new()
    }
}

fn check_prefix_bin_on_path(layout: &Layout, path_env: &str) -> Vec<Finding> {
    let on_path = std::env::split_paths(path_env).any(|p| p == layout.prefix_bin_dir);
    if on_path {
        Vec::new()
    } else {
        vec![warn(format!(
            "{} is not on $PATH — commands rig installs won't be found",
            layout.prefix_bin_dir.display()
        ))]
    }
}

/// Motivating case: `uv` resolved to `/usr/bin/uv`, shadowing the copy
/// actually installed. Only rig's own symlink dir is in scope.
fn check_shadowed_installed_bin(lock: &Lock, layout: &Layout, path_env: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (key, tool) in &lock.tool {
        for bin in &tool.bins {
            let bin_path = Path::new(bin);
            if !bin_path.starts_with(&layout.prefix_bin_dir) {
                continue;
            }
            let Some(name) = bin_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(resolved) = std::env::split_paths(path_env)
                .map(|dir| dir.join(name))
                .find(|candidate| candidate.exists())
            else {
                continue;
            };
            if resolved != bin_path {
                findings.push(warn(format!(
                    "{key}: `{name}` resolves to {} on $PATH, not rig's {}",
                    resolved.display(),
                    bin_path.display()
                )));
            }
        }
    }
    findings
}

fn check_lock_vs_disk(lock: &Lock) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (key, tool) in &lock.tool {
        if let Some(pkg) = &tool.pkg
            && !Path::new(pkg).exists()
        {
            findings.push(warn(format!(
                "{key}: rig.lock points at {pkg}, which no longer exists on disk"
            )));
        }
        for bin in &tool.bins {
            if !Path::new(bin).exists() {
                findings.push(warn(format!("{key}: recorded bin {bin} is missing")));
            }
        }
    }
    findings
}

fn check_orphan_pkg_dirs(lock: &Lock, layout: &Layout) -> Vec<Finding> {
    let pkg_root = layout.state_dir.join("pkg");
    let Ok(tool_dirs) = fs::read_dir(&pkg_root) else {
        return Vec::new();
    };

    let known: HashSet<PathBuf> = lock
        .tool
        .values()
        .filter_map(|t| t.pkg.as_ref().map(PathBuf::from))
        .collect();

    let mut findings = Vec::new();
    for tool_dir in tool_dirs.flatten() {
        let tool_path = tool_dir.path();
        if !tool_path.is_dir() {
            continue;
        }
        let Ok(version_dirs) = fs::read_dir(&tool_path) else {
            continue;
        };
        for version_dir in version_dirs.flatten() {
            let version_path = version_dir.path();
            if version_path.is_dir() && !known.contains(&version_path) {
                findings.push(info(format!(
                    "orphaned pkg dir not referenced by any lock entry: {}",
                    version_path.display()
                )));
            }
        }
    }
    findings
}

fn check_dangling_symlinks(layout: &Layout) -> Vec<Finding> {
    let Ok(entries) = fs::read_dir(&layout.prefix_bin_dir) else {
        return Vec::new();
    };
    let mut findings = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_symlink() || path.exists() {
            continue;
        }
        let Ok(target) = fs::read_link(&path) else {
            continue;
        };
        let resolved = if target.is_absolute() {
            target
        } else {
            path.parent().unwrap_or(Path::new("/")).join(target)
        };
        if resolved.starts_with(&layout.state_dir) {
            findings.push(warn(format!(
                "{} is a dangling symlink into rig's pkg tree ({} no longer exists)",
                path.display(),
                resolved.display()
            )));
        }
    }
    findings
}

/// A not-yet-installed tool whose bin name already exists would be
/// rejected at install time — this surfaces that conflict before you try.
fn check_foreign_prefix_bin(config: &Config, lock: &Lock, layout: &Layout) -> Vec<Finding> {
    let mut findings = Vec::new();
    for entry in config::all_entries(config) {
        let Some(common) = entry.common() else {
            continue;
        };
        let key = entry.key();
        if lock.tool.contains_key(&key) {
            continue;
        }
        let default = config::tool_key(entry.name());
        for name in config::BinSpec::declared_names(common.bin.as_ref(), default) {
            let candidate = layout.prefix_bin_dir.join(&name);
            if candidate.exists() || candidate.is_symlink() {
                findings.push(warn(format!(
                    "{key} isn't installed via rig, but {} already exists \
                     — installing would conflict",
                    candidate.display()
                )));
            }
        }
    }
    findings
}

fn check_missing_completions(config: &Config, lock: &Lock) -> Vec<Finding> {
    let mut findings = Vec::new();
    for entry in config::all_entries(config) {
        let Some(common) = entry.common() else {
            continue;
        };
        let key = entry.key();
        let Some(tool) = lock.tool.get(&key) else {
            continue;
        };
        let wants_completions = !matches!(common.completions, CompletionsSpec::Enabled(false));
        if wants_completions && tool.completions.is_empty() {
            findings.push(warn(format!(
                "{key} is installed but has no registered completions"
            )));
        }
    }
    findings
}

/// Indexes each search dir once (a filename set) instead of re-walking it
/// per completion — `T` completions × `D` dirs would otherwise be `T*D` walks.
fn check_shadowed_completions(lock: &Lock, search_dirs: &[&str]) -> Vec<Finding> {
    let indexes: Vec<(&str, HashSet<String>)> = search_dirs
        .iter()
        .map(|dir| (*dir, index_filenames(Path::new(dir))))
        .collect();

    let mut findings = Vec::new();
    for (key, tool) in &lock.tool {
        for completion in &tool.completions {
            let Some(name) = Path::new(completion).file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            for (dir, names) in &indexes {
                if names.contains(name) {
                    findings.push(warn(format!(
                        "{key}'s completion `{name}` is shadowed by a system \
                         completion under {dir} — check fpath order"
                    )));
                }
            }
        }
    }
    findings
}

fn index_filenames(dir: &Path) -> HashSet<String> {
    let mut names = HashSet::new();
    collect_filenames(dir, &mut names);
    names
}

fn collect_filenames(dir: &Path, names: &mut HashSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_filenames(&path, names);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            names.insert(name.to_string());
        }
    }
}

fn check_sudo_in_setup(config: &Config) -> Vec<Finding> {
    let mut findings = Vec::new();
    for entry in config::all_entries(config) {
        let Some(setup) = entry.common().and_then(|c| c.setup.as_ref()) else {
            continue;
        };
        if setup_uses_sudo(setup) {
            findings.push(info(format!(
                "{}'s setup hook runs `sudo` — its output is outside rig's \
                 control, uninstall will be incomplete",
                entry.key()
            )));
        }
    }
    findings
}

fn setup_uses_sudo(setup: &SetupSpec) -> bool {
    let commands: Vec<&str> = match setup {
        SetupSpec::Single(c) => vec![c.as_str()],
        SetupSpec::Multiple(cs) => cs.iter().map(String::as_str).collect(),
        SetupSpec::Split { install, update } => vec![install.as_str(), update.as_str()],
    };
    commands
        .iter()
        .any(|c| c.split_whitespace().any(|w| w == "sudo"))
}

fn check_unguarded_compdef(lock: &Lock) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (key, tool) in &lock.tool {
        let Some(output) = &tool.eval_cached_output else {
            continue;
        };
        if output.contains("compdef") && !output.contains("functions[compdef]") {
            findings.push(warn(format!(
                "{key}'s cached eval output calls `compdef` without a guard \
                 — must stay after compinit"
            )));
        }
    }
    findings
}

fn check_dual_node_managers(config: &Config, layout: &Layout) -> Vec<Finding> {
    if config.node.is_empty() {
        return Vec::new();
    }

    // `npm root -g` doesn't depend on the entry — one call covers all of them.
    let npm_global_root = Command::new("npm")
        .args(["root", "-g"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string()));

    let mut findings = Vec::new();
    for entry in &config.node {
        let bun_present = layout
            .prefix_bin_dir
            .parent()
            .map(|p| p.join("install/global/node_modules").join(&entry.name))
            .is_some_and(|p| p.exists());
        let npm_present = npm_global_root
            .as_ref()
            .map(|root| root.join(&entry.name))
            .is_some_and(|p| p.exists());

        if bun_present && npm_present {
            findings.push(warn(format!(
                "{} is installed under both bun's and npm's global trees \
                 — only one will win on PATH",
                entry.name
            )));
        }
    }
    findings
}

fn check_eval_cache_drift(config: &Config, lock: &Lock) -> Vec<Finding> {
    let mut findings = Vec::new();
    for entry in config::all_entries(config) {
        let key = entry.key();
        let Some(eval) = entry.common().and_then(|c| c.eval.as_ref()) else {
            continue;
        };
        if eval.cache_override().is_some() {
            continue; // explicit override, not derived from a probe
        }
        let Some(tool) = lock.tool.get(&key) else {
            continue;
        };
        let Some(recorded) = tool.eval_cacheable else {
            continue;
        };

        if let Ok(probe) = evalcache::probe(eval.command())
            && probe.cacheable != recorded
        {
            findings.push(warn(format!(
                "{key}'s eval cacheability changed ({recorded} -> {}) \
                 — run `rig sync --force` to re-probe",
                probe.cacheable
            )));
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock;
    use std::os::unix::fs::symlink;

    #[test]
    fn flags_prefix_bin_dir_missing_from_path() {
        let layout = Layout::new(Path::new("/home/kaige"), "~/.local");
        let findings = check_prefix_bin_on_path(&layout, "/usr/bin:/bin");
        assert_eq!(findings.len(), 1);

        let path_with_it = format!("/usr/bin:{}", layout.prefix_bin_dir.display());
        assert!(check_prefix_bin_on_path(&layout, &path_with_it).is_empty());
    }

    #[test]
    fn flags_an_earlier_path_entry_shadowing_a_rigs_own_installed_bin() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).expect("mkdir home");
        let layout = Layout::new(&home, "~/.local");
        fs::create_dir_all(&layout.prefix_bin_dir).expect("mkdir prefix bin dir");
        fs::write(layout.prefix_bin_dir.join("uv"), b"rig's own").expect("write rig's bin");

        let system_dir = tmp.path().join("usr-bin");
        fs::create_dir_all(&system_dir).expect("mkdir system dir");
        fs::write(system_dir.join("uv"), b"system copy").expect("write shadowing bin");

        let mut lock = Lock::default();
        let mut tool = lock::test_tool_lock();
        tool.bins = vec![layout.prefix_bin_dir.join("uv").display().to_string()];
        lock.tool.insert("uv".to_string(), tool);

        let path_env = format!(
            "{}:{}",
            system_dir.display(),
            layout.prefix_bin_dir.display()
        );
        let findings = check_shadowed_installed_bin(&lock, &layout, &path_env);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("uv"));

        let path_env_in_order = format!(
            "{}:{}",
            layout.prefix_bin_dir.display(),
            system_dir.display()
        );
        assert!(check_shadowed_installed_bin(&lock, &layout, &path_env_in_order).is_empty());
    }

    #[test]
    fn flags_lock_entry_whose_pkg_dir_vanished() {
        let mut lock = Lock::default();
        let mut tool = lock::test_tool_lock();
        tool.pkg = Some("/nonexistent/pkg/dir".to_string());
        lock.tool.insert("ghost".to_string(), tool);

        let findings = check_lock_vs_disk(&lock);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("ghost"));
    }

    #[test]
    fn flags_orphan_pkg_dirs_not_in_lock() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).expect("mkdir home");
        let layout = Layout::new(&home, "~/.local");

        let tracked = layout.pkg_dir("delta", "0.18.2");
        fs::create_dir_all(&tracked).expect("mkdir tracked");
        let orphan = layout.pkg_dir("wallust", "0.1.0");
        fs::create_dir_all(&orphan).expect("mkdir orphan");

        let mut lock = Lock::default();
        let mut tool = lock::test_tool_lock();
        tool.pkg = Some(tracked.display().to_string());
        lock.tool.insert("delta".to_string(), tool);

        let findings = check_orphan_pkg_dirs(&lock, &layout);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("wallust"));
    }

    #[test]
    fn flags_dangling_symlink_into_state_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).expect("mkdir home");
        let layout = Layout::new(&home, "~/.local");
        fs::create_dir_all(&layout.prefix_bin_dir).expect("mkdir prefix bin dir");

        let gone = layout.pkg_dir("delta", "0.18.2").join("delta");
        symlink(&gone, layout.prefix_bin_dir.join("delta")).expect("create dangling symlink");

        let findings = check_dangling_symlinks(&layout);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn flags_foreign_file_blocking_a_not_yet_installed_tool() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).expect("mkdir home");
        let layout = Layout::new(&home, "~/.local");
        fs::create_dir_all(&layout.prefix_bin_dir).expect("mkdir prefix bin dir");
        fs::write(layout.prefix_bin_dir.join("delta"), b"not rig's").expect("write foreign file");

        let mut config = Config::default();
        config.repo.push(crate::config::RepoEntry {
            name: "dandavison/delta".to_string(),
            host: crate::config::Host::Github,
            bpick: None,
            extract: None,
            common: crate::config::Common {
                ..config::test_common()
            },
        });

        let findings = check_foreign_prefix_bin(&config, &Lock::default(), &layout);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn flags_sudo_in_any_setup_shape() {
        assert!(setup_uses_sudo(&SetupSpec::Single(
            "sudo cp x y".to_string()
        )));
        assert!(setup_uses_sudo(&SetupSpec::Multiple(vec![
            "echo hi".to_string(),
            "sudo ln -sf a b".to_string(),
        ])));
        assert!(!setup_uses_sudo(&SetupSpec::Single(
            "cargo build --release".to_string()
        )));
    }

    #[test]
    fn flags_compdef_without_a_guard() {
        let mut lock = Lock::default();
        let mut unguarded = lock::test_tool_lock();
        unguarded.eval_cached_output = Some("compdef _foo foo".to_string());
        lock.tool.insert("foo".to_string(), unguarded);

        let mut guarded = lock::test_tool_lock();
        guarded.eval_cached_output =
            Some("[[ ${+functions[compdef]} -ne 0 ]] && compdef _bar bar".to_string());
        lock.tool.insert("bar".to_string(), guarded);

        let findings = check_unguarded_compdef(&lock);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("foo"));
    }
}
