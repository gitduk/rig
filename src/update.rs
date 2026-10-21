//! Update flow, reusing each source type's `sources::*::install`.
//! repo/git/cargo/node check a cheap version query against `rig.lock`
//! first and skip when current; apt compares against its local candidate
//! the same way; curl always reruns, so an unfiltered
//! `rig update` skips it — only `rig update <tool>` reaches it.

use anyhow::{anyhow, bail};

use crate::config::{
    self, AptEntry, CargoEntry, Config, CurlEntry, GitEntry, NodeEntry, PluginEntry, RepoEntry,
    ResolvedEntry,
};
use crate::lock::{self, Lock, ToolLock};
use crate::paths::{CONFIG_PATH_HINT, Layout};
use crate::pool;
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
    /// Blanket `rig update` keeps curl out; this only fires on an
    /// explicit `rig update <tool>` or `rig install`'s folded check.
    Reran,
    /// `rig sync --force`: `eval` was re-probed in place, version untouched.
    EvalRefreshed {
        cacheable: Option<bool>,
    },
    /// `rig sync --force` on a tool with no `eval` configured.
    NoEval,
    /// Blanket run only: configured but absent from `rig.lock` — maybe
    /// installed by other means, so it's skipped rather than failed.
    NotInstalled,
}

pub struct Report {
    pub tool: String,
    pub outcome: anyhow::Result<Outcome>,
}

/// A mutation a worker wants applied to `rig.lock`. Workers only read a
/// snapshot taken at batch start; the main thread applies these in
/// completion order, so lock writes stay serialized while the version
/// queries and reinstall work runs concurrently.
#[derive(Debug)]
enum LockChange {
    /// Replace the tool's whole lock entry (every real update path).
    Replace(Box<ToolLock>),
    /// `sync --force`: patch only the eval fields, version/bins untouched.
    RefreshEval {
        cacheable: Option<bool>,
        output: Option<String>,
        evidence: Vec<String>,
    },
}

fn apply_change(lock: &mut Lock, key: &str, change: LockChange) {
    match change {
        LockChange::Replace(tool_lock) => {
            lock.tool.insert(key.to_string(), *tool_lock);
        }
        LockChange::RefreshEval {
            cacheable,
            output,
            evidence,
        } => {
            if let Some(tool) = lock.tool.get_mut(key) {
                tool.eval_cacheable = cacheable;
                tool.eval_cached_output = output;
                tool.eval_evidence = evidence;
            }
        }
    }
}

/// `filter`: `None` = all configured tools (`rig update`); `Some(names)` =
/// only those lock keys (`rig update <tool...>`). `verbose` forces
/// streaming output even when several tools run in parallel. `on_report`
/// sees each worker result as it lands, before it joins the returned list.
#[allow(clippy::too_many_arguments)]
pub fn update_all(
    config: &Config,
    lock: &mut Lock,
    layout: &Layout,
    force: bool,
    refresh_eval: bool,
    filter: Option<&[String]>,
    verbose: bool,
    mut on_report: impl FnMut(&Report),
) -> Vec<Report> {
    // Resolve names like `rig install` does (key, full name or bin name);
    // a name matching no entry must fail, not silently drop out.
    let mut reports = Vec::new();
    let keys: Option<Vec<String>> = filter.map(|names| {
        names
            .iter()
            .filter_map(|n| match config::resolve_tool(config, n) {
                Some(e) => Some(e.key()),
                None => {
                    reports.push(Report {
                        tool: n.clone(),
                        outcome: Err(anyhow!("{n} is not in {CONFIG_PATH_HINT}")),
                    });
                    None
                }
            })
            .collect()
    });
    let wanted = |key: &str| keys.as_ref().is_none_or(|ks| ks.iter().any(|k| k == key));
    // Curl has no cheap staleness check, so an unfiltered `rig update`
    // would rerun every curl script each time; it only updates when named
    // explicitly (`rig update <tool>`) or via `rig install`'s folded
    // check. apt stays in the blanket run — its candidate check is cheap.
    let blanket = filter.is_none();
    // A blanket run can't tell "never installed" from "installed outside
    // rig", so neither is a failure; named tools still error. An untracked
    // plugin clone still goes through, for its `rig sync` hint.
    let tracked = |e: &ResolvedEntry<'_>| {
        let key = e.key();
        lock.tool.contains_key(&key)
            || (matches!(e, ResolvedEntry::Plugin(_)) && layout.plugins_dir.join(&key).exists())
    };
    // Collect (key, full name): the key feeds the report, but `resolve_tool`
    // also matches declared bin names, so a key can hit a different entry
    // whose `bin` shares it — only the full name is unambiguous.
    // apt's sudo prompt needs the terminal, so apt targets run first, one
    // at a time and streaming; everything else goes to the parallel pool.
    let (serial, parallel): (Vec<_>, Vec<_>) = config::all_entries(config)
        .into_iter()
        .filter(|e| wanted(&e.key()) && !(blanket && matches!(e, ResolvedEntry::Curl(_))))
        .filter(|e| {
            let skip = blanket && !tracked(e);
            if skip {
                reports.push(Report {
                    tool: e.key(),
                    outcome: Ok(Outcome::NotInstalled),
                });
            }
            !skip
        })
        .partition(|e| e.needs_terminal());
    let as_target = |e: ResolvedEntry<'_>| (e.key(), e.name().to_string());
    let serial: Vec<(String, String)> = serial.into_iter().map(as_target).collect();
    let parallel: Vec<(String, String)> = parallel.into_iter().map(as_target).collect();
    let workers = config.settings.parallel as usize;

    // Workers only read shared state, so the version check and reinstall
    // run against a snapshot taken at batch start. Each tool touches only
    // its own lock key, so stale reads can't cross-contaminate; the main
    // thread merges every result as it arrives.
    let lock_snapshot = lock.clone();

    pool::run_batched(
        serial,
        parallel,
        workers,
        verbose,
        |(_, name)| update_one(config, &lock_snapshot, layout, name, force, refresh_eval),
        |(tool, _), result| {
            // `Some(change)` is exactly "must persist": every producer pairs
            // a mutating outcome (Updated/Reran/EvalRefreshed) with a change.
            let report = match result {
                Ok((outcome, Some(change))) => {
                    apply_change(lock, &tool, change);
                    // Persist right after a lock-mutating outcome, not once
                    // at batch end — a killed process must not lose
                    // already-updated tools.
                    match lock::save_and_sync(config, lock, layout) {
                        Ok(()) => {
                            if let Some(saved) = lock.tool.get(&tool) {
                                sources::prune_superseded(&tool, saved, layout);
                            }
                            Report {
                                tool,
                                outcome: Ok(outcome),
                            }
                        }
                        Err(e) => Report {
                            tool,
                            outcome: Err(e),
                        },
                    }
                }
                Ok((outcome, None)) => Report {
                    tool,
                    outcome: Ok(outcome),
                },
                Err(e) => Report {
                    tool,
                    outcome: Err(e),
                },
            };
            on_report(&report);
            reports.push(report);
        },
    );
    reports
}

/// One tool's update inside a worker: re-resolve the tool's full name to
/// its entry (workers can't hold `ResolvedEntry` borrows across threads,
/// only the snapshot), then dispatch exactly like the old serial loop did.
/// The lock key is derived from the resolved entry — the config name is
/// only the unambiguous resolution handle.
fn update_one(
    config: &Config,
    lock: &Lock,
    layout: &Layout,
    name: &str,
    force: bool,
    refresh_eval: bool,
) -> anyhow::Result<(Outcome, Option<LockChange>)> {
    let entry = config::resolve_tool(config, name)
        .ok_or_else(|| anyhow!("{name} is not in {CONFIG_PATH_HINT}"))?;
    let key = entry.key();
    if refresh_eval {
        refresh_eval_cache(&entry, &key, lock)
    } else {
        match entry {
            ResolvedEntry::Repo(e) => update_repo(e, &key, lock, layout, force),
            ResolvedEntry::Git(e) => update_git(e, &key, lock, layout, force),
            ResolvedEntry::Cargo(e) => update_cargo(e, &key, lock, layout, force),
            ResolvedEntry::Node(e) => update_node(e, &key, lock, layout, force),
            ResolvedEntry::Apt(e) => update_apt(e, &key, lock, layout),
            ResolvedEntry::Curl(e) => update_curl(e, &key, lock, layout),
            ResolvedEntry::Plugin(e) => update_plugin(e, &key, lock, layout, force),
        }
    }
}

/// Folds an update check into `rig install` for an already-installed tool:
/// `Some(new)` if the source reinstalled (newer version / curl), else `None`.
pub fn check_and_update(
    config: &Config,
    lock: &Lock,
    layout: &Layout,
    name: &str,
) -> anyhow::Result<Option<ToolLock>> {
    match update_one(config, lock, layout, name, false, false)? {
        (_, Some(LockChange::Replace(new))) => Ok(Some(*new)),
        _ => Ok(None),
    }
}

/// Re-probes `eval` and returns the patch, unlike a real update: version,
/// bins, and completions are left untouched. `apply_change` does the write.
fn refresh_eval_cache(
    entry: &ResolvedEntry,
    key: &str,
    lock: &Lock,
) -> anyhow::Result<(Outcome, Option<LockChange>)> {
    if !lock.tool.contains_key(key) {
        return Err(not_installed(key));
    }
    let Some(eval) = entry.common().and_then(|c| c.eval.as_ref()) else {
        return Ok((Outcome::NoEval, None));
    };

    let (eval_cacheable, eval_cached_output, eval_evidence) =
        sources::resolve_eval_cache(key, Some(eval));
    Ok((
        Outcome::EvalRefreshed {
            cacheable: eval_cacheable,
        },
        Some(LockChange::RefreshEval {
            cacheable: eval_cacheable,
            output: eval_cached_output,
            evidence: eval_evidence,
        }),
    ))
}

/// Absent from `rig.lock` — the binary may still exist, installed by other means.
fn not_installed(key: &str) -> anyhow::Error {
    anyhow!("{key} is not installed by rig; run `rig install {key}` to manage it")
}

fn already_installed(lock: &Lock, key: &str) -> anyhow::Result<String> {
    lock.tool
        .get(key)
        .map(|t| t.version.clone())
        .ok_or_else(|| not_installed(key))
}

fn update_repo(
    entry: &RepoEntry,
    key: &str,
    lock: &Lock,
    layout: &Layout,
    force: bool,
) -> anyhow::Result<(Outcome, Option<LockChange>)> {
    let current_version = already_installed(lock, key)?;

    let release = version::github::latest_release(&entry.name, entry.host)?;
    if !force && release.tag == current_version {
        return Ok((
            Outcome::UpToDate {
                version: current_version,
            },
            None,
        ));
    }

    let new_lock = sources::repo::install(
        entry,
        layout,
        lock,
        sources::Phase::Update {
            from: &current_version,
        },
    )?;
    let to = new_lock.version.clone();
    Ok((
        Outcome::Updated {
            from: current_version,
            to,
        },
        Some(LockChange::Replace(Box::new(new_lock))),
    ))
}

fn update_git(
    entry: &GitEntry,
    key: &str,
    lock: &Lock,
    layout: &Layout,
    force: bool,
) -> anyhow::Result<(Outcome, Option<LockChange>)> {
    let current_version = already_installed(lock, key)?;

    let latest = sources::git::remote_head_commit(entry.host, &entry.name)?;
    if !force && latest == current_version {
        return Ok((
            Outcome::UpToDate {
                version: current_version,
            },
            None,
        ));
    }

    let new_lock = sources::git::install(
        entry,
        layout,
        lock,
        sources::Phase::Update {
            from: &current_version,
        },
    )?;
    let to = new_lock.version.clone();
    Ok((
        Outcome::Updated {
            from: current_version,
            to,
        },
        Some(LockChange::Replace(Box::new(new_lock))),
    ))
}

/// This is rig's improvement over bare `cargo install --force`, which has
/// no version comparison of its own and always recompiles.
fn update_cargo(
    entry: &CargoEntry,
    key: &str,
    lock: &Lock,
    layout: &Layout,
    force: bool,
) -> anyhow::Result<(Outcome, Option<LockChange>)> {
    let current_version = already_installed(lock, key)?;

    let latest = version::crates_io::latest_version(&entry.name)?;
    if !force && latest == current_version {
        return Ok((
            Outcome::UpToDate {
                version: current_version,
            },
            None,
        ));
    }

    let new_lock = sources::cargo::install(
        entry,
        layout,
        lock,
        sources::Phase::Update {
            from: &current_version,
        },
    )?;
    let to = new_lock.version.clone();
    Ok((
        Outcome::Updated {
            from: current_version,
            to,
        },
        Some(LockChange::Replace(Box::new(new_lock))),
    ))
}

fn update_node(
    entry: &NodeEntry,
    key: &str,
    lock: &Lock,
    layout: &Layout,
    force: bool,
) -> anyhow::Result<(Outcome, Option<LockChange>)> {
    let current = lock.tool.get(key).ok_or_else(|| not_installed(key))?;
    let current_version = current.version.clone();
    let manager = sources::node::ResolvedManager::parse(
        current
            .manager
            .as_deref()
            .ok_or_else(|| anyhow!("{key}'s rig.lock entry has no recorded manager"))?,
    )?;

    let latest = version::npm::latest(&entry.name)?.version;
    if !force && latest == current_version {
        return Ok((
            Outcome::UpToDate {
                version: current_version,
            },
            None,
        ));
    }

    let new_lock = sources::node::install_with_manager(
        entry,
        layout,
        lock,
        manager,
        sources::Phase::Update {
            from: &current_version,
        },
    )?;
    let to = new_lock.version.clone();
    Ok((
        Outcome::Updated {
            from: current_version,
            to,
        },
        Some(LockChange::Replace(Box::new(new_lock))),
    ))
}

/// apt's local candidate is the pre-check, so a current package never
/// touches sudo. No readable candidate falls back to rerunning the install.
fn update_apt(
    entry: &AptEntry,
    key: &str,
    lock: &Lock,
    layout: &Layout,
) -> anyhow::Result<(Outcome, Option<LockChange>)> {
    let current_version = already_installed(lock, key)?;
    if sources::apt::candidate_version(&entry.name).as_deref() == Some(current_version.as_str()) {
        return Ok((
            Outcome::UpToDate {
                version: current_version,
            },
            None,
        ));
    }

    let new_lock = sources::apt::install(
        entry,
        layout,
        sources::Phase::Update {
            from: &current_version,
        },
    )?;
    let to = new_lock.version.clone();
    if to == current_version {
        return Ok((Outcome::UpToDate { version: to }, None));
    }
    Ok((
        Outcome::Updated {
            from: current_version,
            to,
        },
        Some(LockChange::Replace(Box::new(new_lock))),
    ))
}

/// No cheap pre-check exists, so it always reruns the script; `update_all`
/// keeps curl out of blanket runs, so this fires only on an explicit
/// `rig update <tool>` (or the update check folded into `rig install`).
fn update_curl(
    entry: &CurlEntry,
    key: &str,
    lock: &Lock,
    layout: &Layout,
) -> anyhow::Result<(Outcome, Option<LockChange>)> {
    let current_version = already_installed(lock, key)?;
    let new_lock = sources::curl::install(
        entry,
        layout,
        sources::Phase::Update {
            from: &current_version,
        },
    )?;
    Ok((
        Outcome::Reran,
        Some(LockChange::Replace(Box::new(new_lock))),
    ))
}

/// No cheap pre-check like `update_repo`'s — `git pull` itself must
/// contact the remote, so a prior `git ls-remote` would just double that.
fn update_plugin(
    entry: &PluginEntry,
    key: &str,
    lock: &Lock,
    layout: &Layout,
    force: bool,
) -> anyhow::Result<(Outcome, Option<LockChange>)> {
    let current_version = match lock.tool.get(key) {
        Some(tool) => tool.version.clone(),
        // A clone that predates plugin lock tracking — `sync` backfills it.
        None if layout.plugins_dir.join(key).exists() => {
            bail!("{key} is cloned but not yet tracked in rig.lock; run `rig sync` first")
        }
        None => return Err(not_installed(key)),
    };

    let new_lock = sources::plugin::update(entry, layout)?;
    let to = new_lock.version.clone();
    if !force && to == current_version {
        return Ok((
            Outcome::UpToDate {
                version: current_version,
            },
            None,
        ));
    }
    Ok((
        Outcome::Updated {
            from: current_version,
            to,
        },
        Some(LockChange::Replace(Box::new(new_lock))),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, BpickSpec, Common, Config, CurlEntry, Host};
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
            run: None,
        }
    }

    #[test]
    fn update_all_skips_curl_on_blanket_runs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(tmp.path(), "~/.local");
        let mut config = Config::default();
        config.curl.push(CurlEntry {
            name: "oh-my-zsh".to_string(),
            url: "https://invalid.example/install.sh".to_string(),
            common: config::test_common(),
        });
        let mut lock = Lock::default();

        // Unfiltered: curl is excluded, so nothing runs (the url above must
        // never be fetched) and nothing reports.
        let reports = update_all(
            &config,
            &mut lock,
            &layout,
            false,
            false,
            None,
            false,
            |_| {},
        );
        assert!(reports.is_empty());
    }

    #[test]
    fn update_all_skips_untracked_tools_on_blanket_runs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(tmp.path(), "~/.local");
        let mut config = Config::default();
        config.apt.push(AptEntry {
            name: "nala".to_string(),
            common: config::test_common(),
        });
        let mut lock = Lock::default();

        // Blanket: skipped, never reaching apt-get.
        let reports = update_all(
            &config,
            &mut lock,
            &layout,
            false,
            false,
            None,
            false,
            |_| {},
        );
        assert_eq!(reports.len(), 1);
        assert!(matches!(reports[0].outcome, Ok(Outcome::NotInstalled)));

        // Named: still an error, pointing at `rig install`.
        let filter = ["nala".to_string()];
        let reports = update_all(
            &config,
            &mut lock,
            &layout,
            false,
            false,
            Some(&filter),
            false,
            |_| {},
        );
        let message = reports[0].outcome.as_ref().unwrap_err().to_string();
        assert!(message.contains("not installed by rig"));
    }

    #[test]
    fn update_all_reports_unknown_names_as_failures() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(tmp.path(), "~/.local");
        let config = Config::default();
        let mut lock = Lock::default();

        let filter = ["hi".to_string()];
        let reports = update_all(
            &config,
            &mut lock,
            &layout,
            false,
            false,
            Some(&filter),
            false,
            |_| {},
        );
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].tool, "hi");
        let message = reports[0].outcome.as_ref().unwrap_err().to_string();
        assert!(message.contains("not in ~/.rig.toml"));
    }

    #[test]
    fn update_plugin_errors_when_never_cloned() {
        let lock = Lock::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(tmp.path(), "~/.local");
        let entry = autosuggestions_entry();

        let outcome = update_plugin(&entry, "zsh-autosuggestions", &lock, &layout, false);
        let message = outcome.unwrap_err().to_string();
        assert!(message.contains("not installed"));
        assert!(message.contains("rig install"));
    }

    #[test]
    fn update_plugin_points_at_sync_for_an_untracked_clone() {
        // Simulates a clone that predates plugin lock tracking: the dest
        // dir exists on disk, but `rig.lock` has no entry for it yet.
        let lock = Lock::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(tmp.path(), "~/.local");
        fs::create_dir_all(layout.plugins_dir.join("zsh-autosuggestions"))
            .expect("mkdir plugin clone");
        let entry = autosuggestions_entry();

        let outcome = update_plugin(&entry, "zsh-autosuggestions", &lock, &layout, false);
        let message = outcome.unwrap_err().to_string();
        assert!(message.contains("rig sync"));
    }

    #[test]
    fn errors_when_tool_is_not_yet_installed() {
        let lock = Lock::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = Layout::new(tmp.path(), "~/.local");
        let entry = delta_entry();

        let outcome = update_repo(&entry, "delta", &lock, &layout, false);
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

        let (outcome, change) =
            refresh_eval_cache(&resolved, "delta", &lock).expect("refresh should run");
        assert!(matches!(
            outcome,
            Outcome::EvalRefreshed {
                cacheable: Some(true)
            }
        ));
        match change {
            Some(LockChange::RefreshEval { output, .. }) => {
                assert_eq!(output.as_deref(), Some("fresh-output\n"));
            }
            other => panic!("expected a RefreshEval change, got {other:?}"),
        }
    }

    #[test]
    fn refresh_eval_cache_is_a_noop_without_eval_configured() {
        let mut lock = Lock::default();
        lock.tool
            .insert("delta".to_string(), crate::lock::test_tool_lock());
        let entry = delta_entry();
        let resolved = ResolvedEntry::Repo(&entry);

        let (outcome, change) =
            refresh_eval_cache(&resolved, "delta", &lock).expect("no-eval isn't an error");
        assert!(matches!(outcome, Outcome::NoEval));
        assert!(change.is_none());
    }

    #[test]
    fn refresh_eval_cache_errors_when_not_yet_installed() {
        let lock = Lock::default();
        let entry = delta_entry();
        let resolved = ResolvedEntry::Repo(&entry);

        let outcome = refresh_eval_cache(&resolved, "delta", &lock);
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

        let (outcome, _) =
            update_repo(&entry, "delta", &lock, &layout, false).expect("update should run");
        assert!(matches!(outcome, Outcome::UpToDate { .. }), "{outcome:?}");

        let (outcome, _) =
            update_repo(&entry, "delta", &lock, &layout, true).expect("forced update should run");
        assert!(matches!(outcome, Outcome::Updated { .. }), "{outcome:?}");
    }
}
