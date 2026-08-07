//! Update flow, reusing each source type's `sources::*::install`.
//! repo/git/cargo/node check a cheap version query against `rig.lock`
//! first; apt/curl have no such pre-check and just always reinstall.

use anyhow::{anyhow, bail};

use crate::config::{
    self, AptEntry, CargoEntry, Config, CurlEntry, GitEntry, NodeEntry, PluginEntry, RepoEntry,
    ResolvedEntry,
};
use crate::lock::{self, Lock};
use crate::paths::Layout;
use crate::sources;
use crate::version;

#[derive(Debug)]
pub enum Outcome {
    UpToDate {
        version: String,
    },
    Updated {
        from: String,
        to: String,
    },
    /// `[[curl]]`: no version to compare (reliability "depends on the
    /// script"), so every call reruns it rather than guessing staleness.
    Reran,
    /// `rig sync --force`: `eval` was re-probed in place, version untouched.
    EvalRefreshed {
        cacheable: Option<bool>,
    },
    /// `rig sync --force` on a tool with no `eval` configured.
    NoEval,
}

pub struct Report {
    pub tool: String,
    pub outcome: anyhow::Result<Outcome>,
}

/// `filter`: `None` = all configured tools (`rig update`); `Some(names)` =
/// only those lock keys (`rig update <tool...>`).
pub fn update_all(
    config: &Config,
    lock: &mut Lock,
    layout: &Layout,
    force: bool,
    refresh_eval: bool,
    filter: Option<&[String]>,
) -> Vec<Report> {
    let wanted = |key: &str| filter.is_none_or(|names| names.iter().any(|n| n == key));

    let mut reports = Vec::new();
    for entry in config::all_entries(config) {
        let tool = entry.key();
        if !wanted(&tool) {
            continue;
        }
        let outcome = if refresh_eval {
            refresh_eval_cache(&entry, &tool, lock)
        } else {
            match entry {
                ResolvedEntry::Repo(e) => update_repo(e, &tool, lock, layout, force),
                ResolvedEntry::Git(e) => update_git(e, &tool, lock, layout, force),
                ResolvedEntry::Cargo(e) => update_cargo(e, &tool, lock, layout, force),
                ResolvedEntry::Node(e) => update_node(e, &tool, lock, layout, force),
                ResolvedEntry::Apt(e) => update_apt(e, &tool, lock, layout),
                ResolvedEntry::Curl(e) => update_curl(e, &tool, lock, layout),
                ResolvedEntry::Plugin(e) => update_plugin(e, &tool, lock, layout, force),
            }
        };
        // Persist right after a lock-mutating outcome, not once at the end
        // of the batch — a killed process must not lose already-updated tools.
        let outcome = match outcome {
            Ok(o @ (Outcome::Updated { .. } | Outcome::Reran | Outcome::EvalRefreshed { .. })) => {
                lock::save_and_sync(config, lock, layout).map(|()| o)
            }
            other => other,
        };
        reports.push(Report { tool, outcome });
    }
    reports
}

/// Re-probes `eval` and patches the lock entry in place, unlike a real
/// update: version, bins, and completions are left untouched.
fn refresh_eval_cache(
    entry: &ResolvedEntry,
    key: &str,
    lock: &mut Lock,
) -> anyhow::Result<Outcome> {
    let tool = lock
        .tool
        .get_mut(key)
        .ok_or_else(|| anyhow!("{key} is not installed; run `rig install {key}` first"))?;
    let Some(eval) = entry.common().and_then(|c| c.eval.as_ref()) else {
        return Ok(Outcome::NoEval);
    };

    let (eval_cacheable, eval_cached_output, eval_evidence) =
        sources::resolve_eval_cache(Some(eval));
    tool.eval_cacheable = eval_cacheable;
    tool.eval_cached_output = eval_cached_output;
    tool.eval_evidence = eval_evidence;
    Ok(Outcome::EvalRefreshed {
        cacheable: eval_cacheable,
    })
}

fn already_installed(lock: &Lock, key: &str) -> anyhow::Result<String> {
    lock.tool
        .get(key)
        .map(|t| t.version.clone())
        .ok_or_else(|| anyhow!("{key} is not installed; run `rig install {key}` first"))
}

fn update_repo(
    entry: &RepoEntry,
    key: &str,
    lock: &mut Lock,
    layout: &Layout,
    force: bool,
) -> anyhow::Result<Outcome> {
    let current_version = already_installed(lock, key)?;

    let release = version::github::latest_release(&entry.name, entry.host)?;
    if !force && release.tag == current_version {
        return Ok(Outcome::UpToDate {
            version: current_version,
        });
    }

    let new_lock = sources::repo::install(entry, layout, lock, sources::Phase::Update)?;
    let to = new_lock.version.clone();
    lock.tool.insert(key.to_string(), new_lock);
    Ok(Outcome::Updated {
        from: current_version,
        to,
    })
}

fn update_git(
    entry: &GitEntry,
    key: &str,
    lock: &mut Lock,
    layout: &Layout,
    force: bool,
) -> anyhow::Result<Outcome> {
    let current_version = already_installed(lock, key)?;

    let latest = sources::git::remote_head_commit(entry.host, &entry.name)?;
    if !force && latest == current_version {
        return Ok(Outcome::UpToDate {
            version: current_version,
        });
    }

    let new_lock = sources::git::install(entry, layout, lock, sources::Phase::Update)?;
    let to = new_lock.version.clone();
    lock.tool.insert(key.to_string(), new_lock);
    Ok(Outcome::Updated {
        from: current_version,
        to,
    })
}

/// This is rig's improvement over bare `cargo install --force`, which has
/// no version comparison of its own and always recompiles.
fn update_cargo(
    entry: &CargoEntry,
    key: &str,
    lock: &mut Lock,
    layout: &Layout,
    force: bool,
) -> anyhow::Result<Outcome> {
    let current_version = already_installed(lock, key)?;

    let latest = version::crates_io::latest_version(&entry.name)?;
    if !force && latest == current_version {
        return Ok(Outcome::UpToDate {
            version: current_version,
        });
    }

    let new_lock = sources::cargo::install(entry, layout, lock, sources::Phase::Update)?;
    let to = new_lock.version.clone();
    lock.tool.insert(key.to_string(), new_lock);
    Ok(Outcome::Updated {
        from: current_version,
        to,
    })
}

fn update_node(
    entry: &NodeEntry,
    key: &str,
    lock: &mut Lock,
    layout: &Layout,
    force: bool,
) -> anyhow::Result<Outcome> {
    let current = lock
        .tool
        .get(key)
        .ok_or_else(|| anyhow!("{key} is not installed; run `rig install {key}` first"))?;
    let current_version = current.version.clone();
    let manager = sources::node::ResolvedManager::parse(
        current
            .manager
            .as_deref()
            .ok_or_else(|| anyhow!("{key}'s rig.lock entry has no recorded manager"))?,
    )?;

    let latest = version::npm::latest(&entry.name)?.version;
    if !force && latest == current_version {
        return Ok(Outcome::UpToDate {
            version: current_version,
        });
    }

    let new_lock =
        sources::node::install_with_manager(entry, layout, lock, manager, sources::Phase::Update)?;
    let to = new_lock.version.clone();
    lock.tool.insert(key.to_string(), new_lock);
    Ok(Outcome::Updated {
        from: current_version,
        to,
    })
}

/// No cheap pre-check exists, so this always runs `apt-get install` (a
/// no-op if current) and diffs `dpkg-query` before/after for the outcome.
fn update_apt(
    entry: &AptEntry,
    key: &str,
    lock: &mut Lock,
    layout: &Layout,
) -> anyhow::Result<Outcome> {
    let current_version = already_installed(lock, key)?;

    let new_lock = sources::apt::install(entry, layout, sources::Phase::Update)?;
    let to = new_lock.version.clone();
    lock.tool.insert(key.to_string(), new_lock);

    if to == current_version {
        Ok(Outcome::UpToDate { version: to })
    } else {
        Ok(Outcome::Updated {
            from: current_version,
            to,
        })
    }
}

fn update_curl(
    entry: &CurlEntry,
    key: &str,
    lock: &mut Lock,
    layout: &Layout,
) -> anyhow::Result<Outcome> {
    if !lock.tool.contains_key(key) {
        bail!("{key} is not installed; run `rig install {key}` first");
    }
    let new_lock = sources::curl::install(entry, layout, sources::Phase::Update)?;
    lock.tool.insert(key.to_string(), new_lock);
    Ok(Outcome::Reran)
}

/// No cheap pre-check like `update_repo`'s — `git pull` itself must
/// contact the remote, so a prior `git ls-remote` would just double that.
fn update_plugin(
    entry: &PluginEntry,
    key: &str,
    lock: &mut Lock,
    layout: &Layout,
    force: bool,
) -> anyhow::Result<Outcome> {
    let current_version = match lock.tool.get(key) {
        Some(tool) => tool.version.clone(),
        // A clone that predates plugin lock tracking — `sync` backfills it.
        None if layout.plugins_dir.join(key).exists() => {
            bail!("{key} is cloned but not yet tracked in rig.lock; run `rig sync` first")
        }
        None => bail!("{key} is not installed; run `rig install {key}` first"),
    };

    let new_lock = sources::plugin::update(entry, layout)?;
    let to = new_lock.version.clone();
    if !force && to == current_version {
        return Ok(Outcome::UpToDate {
            version: current_version,
        });
    }
    lock.tool.insert(key.to_string(), new_lock);
    Ok(Outcome::Updated {
        from: current_version,
        to,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, BpickSpec, Common, Host};
    use std::fs;

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

    fn autosuggestions_entry() -> PluginEntry {
        PluginEntry {
            name: "zsh-users/zsh-autosuggestions".to_string(),
            description: None,
            source: Some("zsh-autosuggestions.zsh".to_string()),
            defer: 1,
            atload: None,
        }
    }

    #[test]
    fn update_plugin_errors_when_never_cloned() {
        let mut lock = Lock::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(tmp.path(), "~/.local");
        let entry = autosuggestions_entry();

        let outcome = update_plugin(&entry, "zsh-autosuggestions", &mut lock, &layout, false);
        let message = outcome.unwrap_err().to_string();
        assert!(message.contains("not installed"));
        assert!(message.contains("rig install"));
    }

    #[test]
    fn update_plugin_points_at_sync_for_an_untracked_clone() {
        // Simulates a clone that predates plugin lock tracking: the dest
        // dir exists on disk, but `rig.lock` has no entry for it yet.
        let mut lock = Lock::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(tmp.path(), "~/.local");
        fs::create_dir_all(layout.plugins_dir.join("zsh-autosuggestions"))
            .expect("mkdir plugin clone");
        let entry = autosuggestions_entry();

        let outcome = update_plugin(&entry, "zsh-autosuggestions", &mut lock, &layout, false);
        let message = outcome.unwrap_err().to_string();
        assert!(message.contains("rig sync"));
    }

    #[test]
    fn errors_when_tool_is_not_yet_installed() {
        let mut lock = Lock::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(tmp.path(), "~/.local");
        let entry = delta_entry();

        let outcome = update_repo(&entry, "delta", &mut lock, &layout, false);
        assert!(outcome.is_err());
        assert!(outcome.unwrap_err().to_string().contains("not installed"));
    }

    #[test]
    fn refresh_eval_cache_repoints_stale_cached_output() {
        let mut lock = Lock::default();
        lock.tool.insert(
            "delta".to_string(),
            crate::lock::ToolLock {
                eval_cacheable: Some(true),
                eval_cached_output: Some("stale-output".to_string()),
                ..crate::lock::test_tool_lock()
            },
        );
        let mut entry = delta_entry();
        entry.common.eval = Some(config::EvalSpec::Cmd("echo fresh-output".to_string()));
        let resolved = ResolvedEntry::Repo(&entry);

        let outcome =
            refresh_eval_cache(&resolved, "delta", &mut lock).expect("refresh should run");
        assert!(matches!(
            outcome,
            Outcome::EvalRefreshed {
                cacheable: Some(true)
            }
        ));
        assert_eq!(
            lock.tool["delta"].eval_cached_output.as_deref(),
            Some("fresh-output\n")
        );
    }

    #[test]
    fn refresh_eval_cache_is_a_noop_without_eval_configured() {
        let mut lock = Lock::default();
        lock.tool
            .insert("delta".to_string(), crate::lock::test_tool_lock());
        let entry = delta_entry();
        let resolved = ResolvedEntry::Repo(&entry);

        let outcome =
            refresh_eval_cache(&resolved, "delta", &mut lock).expect("no-eval isn't an error");
        assert!(matches!(outcome, Outcome::NoEval));
    }

    #[test]
    fn refresh_eval_cache_errors_when_not_yet_installed() {
        let mut lock = Lock::default();
        let entry = delta_entry();
        let resolved = ResolvedEntry::Repo(&entry);

        let outcome = refresh_eval_cache(&resolved, "delta", &mut lock);
        assert!(outcome.is_err());
        assert!(outcome.unwrap_err().to_string().contains("not installed"));
    }

    #[test]
    #[ignore = "hits the real GitHub API and downloads a real release; run with `cargo test -- --ignored`"]
    fn skips_when_current_then_reinstalls_when_forced() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).expect("mkdir home");
        let layout = Layout::new(&home, "~/.local");
        let entry = delta_entry();

        let mut lock = Lock::default();
        let installed = sources::repo::install(&entry, &layout, &lock, sources::Phase::Install)
            .expect("initial install");
        lock.tool.insert("delta".to_string(), installed);

        let outcome =
            update_repo(&entry, "delta", &mut lock, &layout, false).expect("update should run");
        assert!(matches!(outcome, Outcome::UpToDate { .. }), "{outcome:?}");

        let outcome = update_repo(&entry, "delta", &mut lock, &layout, true)
            .expect("forced update should run");
        assert!(matches!(outcome, Outcome::Updated { .. }), "{outcome:?}");
    }
}
