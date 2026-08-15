//! `rig.lock` schema — installed-tool state, keyed by tool name. `ToolLock`
//! stays flat rather than a tagged enum per source type: the field sets for
//! repo/git-style and node-style entries don't collide, so a tag would add
//! ceremony without resolving any ambiguity.

use std::collections::BTreeMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::config::Config;
use crate::initzsh;
use crate::paths::Layout;

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct Lock {
    #[serde(default)]
    pub tool: BTreeMap<String, ToolLock>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ToolLock {
    pub version: String,
    pub source: String,
    #[serde(with = "time::serde::rfc3339")]
    pub installed_at: OffsetDateTime,
    pub bins: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completions: Vec<String>,

    // repo/git-specific
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pkg: Option<String>,

    // node-specific
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manager: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,

    // Eval cacheability verdict, re-probed on every update.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_cacheable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_cached_output: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub eval_evidence: Vec<String>,
}

pub fn parse(input: &str) -> anyhow::Result<Lock> {
    toml::from_str(input).context("failed to parse rig.lock")
}

/// Shared base for tests that need a `ToolLock` — [TEST-1]: extract once a
/// 2nd test file needs full construction. Override fields with `..`.
#[cfg(test)]
pub fn test_tool_lock() -> ToolLock {
    ToolLock {
        version: "1.0.0".to_string(),
        source: "github:test/test".to_string(),
        installed_at: OffsetDateTime::UNIX_EPOCH,
        bins: Vec::new(),
        completions: Vec::new(),
        asset: None,
        size: None,
        pkg: None,
        manager: None,
        root: None,
        eval_cacheable: None,
        eval_cached_output: None,
        eval_evidence: Vec::new(),
    }
}

pub fn load(path: &Path) -> anyhow::Result<Lock> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    parse(&text).with_context(|| format!("failed to parse {}", path.display()))
}

/// First run has no `rig.lock` yet — that's not an error.
pub fn load_or_default(path: &Path) -> anyhow::Result<Lock> {
    if path.exists() {
        load(path)
    } else {
        Ok(Lock::default())
    }
}

/// `fs::write` truncates in place, so a disk-full write can destroy the
/// old content before the new content lands. Temp file + `rename` instead.
fn atomic_write(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if path.exists() {
        let backup = backup_path(path);
        fs::copy(path, &backup).with_context(|| {
            format!(
                "failed to back up {} to {}",
                path.display(),
                backup.display()
            )
        })?;
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let tmp = path.with_extension(format!("{ext}.tmp.{}", std::process::id()));
    fs::write(&tmp, contents).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("failed to move {} into place", path.display()))
}

/// `rig.lock` -> `.rig.lock`: one-generation rollback for a bad-but-valid
/// write, which `atomic_write` alone can't catch.
fn backup_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("backup");
    path.with_file_name(format!(".{file_name}"))
}

pub fn save(lock: &Lock, path: &Path) -> anyhow::Result<()> {
    let text = toml::to_string_pretty(lock).context("failed to serialize rig.lock")?;
    atomic_write(path, &text)
}

/// Lock must land before init.zsh. Call after every tool, not once at
/// batch end, so a killed process doesn't lose already-finished work.
pub fn save_and_sync(config: &Config, lock: &Lock, layout: &Layout) -> anyhow::Result<()> {
    save(lock, &layout.lock_path)?;
    let rendered = initzsh::render(config, lock, layout);
    atomic_write(&layout.init_zsh_path, &rendered)
}

/// Advisory cross-process lock on the state directory: held for a whole
/// command so writers can't interleave and lose each other's `rig.lock` entries.
pub struct StateLock {
    _file: fs::File,
}

impl StateLock {
    /// Blocks until the lock is free. For user-initiated writers where
    /// silently skipping the write would be wrong.
    pub fn acquire(path: &Path) -> anyhow::Result<Self> {
        match Self::flock(path, false)? {
            Some(lock) => Ok(lock),
            // Unreachable: a blocking flock only returns on success or error.
            None => bail!(
                "blocking lock on {} returned without acquiring",
                path.display()
            ),
        }
    }

    /// `Ok(None)` when another rig process holds the lock. Lets a
    /// shell-startup `rig sync` bow out instead of hanging — the holder
    /// regenerates init.zsh when it finishes, so nothing is lost.
    pub fn try_acquire(path: &Path) -> anyhow::Result<Option<Self>> {
        Self::flock(path, true)
    }

    fn flock(path: &Path, nonblocking: bool) -> anyhow::Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            // First run has no state_dir yet; the lock file must exist
            // before any writer can race on it.
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            // Only ever flock'd, never written — keep any existing content.
            .truncate(false)
            .open(path)
            .with_context(|| format!("failed to open lock file {}", path.display()))?;

        let op = if nonblocking {
            libc::LOCK_EX | libc::LOCK_NB
        } else {
            libc::LOCK_EX
        };
        loop {
            let rc = unsafe { libc::flock(file.as_raw_fd(), op) };
            if rc == 0 {
                return Ok(Some(Self { _file: file }));
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                // A signal interrupted the blocking wait — retry, don't fail.
                Some(code) if code == libc::EINTR => continue,
                // Non-blocking and another process holds it — not an error.
                Some(code) if nonblocking && code == libc::EWOULDBLOCK => return Ok(None),
                _ => bail!("failed to lock {}: {err}", path.display()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::format_description::well_known::Rfc3339;

    fn ts(s: &str) -> OffsetDateTime {
        OffsetDateTime::parse(s, &Rfc3339).expect("valid RFC3339 fixture timestamp")
    }

    #[test]
    fn round_trips_mixed_entries() {
        let mut lock = Lock::default();
        lock.tool.insert(
            "delta".to_string(),
            ToolLock {
                version: "0.18.2".to_string(),
                source: "github:dandavison/delta".to_string(),
                installed_at: ts("2026-08-03T12:00:00Z"),
                bins: vec!["~/.local/bin/delta".to_string()],
                completions: vec!["~/.local/share/rig/completions/_delta".to_string()],
                asset: Some("delta-0.18.2-x86_64-unknown-linux-gnu.tar.gz".to_string()),
                size: Some(4_823_104),
                pkg: Some("~/.local/share/rig/pkg/delta/0.18.2".to_string()),
                manager: None,
                root: None,
                eval_cacheable: None,
                eval_cached_output: None,
                eval_evidence: Vec::new(),
            },
        );
        lock.tool.insert(
            "@openai/codex".to_string(),
            ToolLock {
                version: "0.142.2".to_string(),
                source: "node:@openai/codex".to_string(),
                installed_at: ts("2026-08-03T12:05:00Z"),
                bins: vec!["~/.local/bin/codex".to_string()],
                completions: vec![],
                asset: None,
                size: None,
                pkg: None,
                manager: Some("bun".to_string()),
                root: Some("~/.local/install/global/node_modules/@openai/codex".to_string()),
                eval_cacheable: None,
                eval_cached_output: None,
                eval_evidence: Vec::new(),
            },
        );

        let text = toml::to_string_pretty(&lock).expect("serializes");
        let reparsed = parse(&text).expect("re-parses what we just wrote");

        assert_eq!(reparsed.tool.len(), 2);
        let delta = &reparsed.tool["delta"];
        assert_eq!(delta.size, Some(4_823_104));
        assert_eq!(delta.manager, None);

        let codex = &reparsed.tool["@openai/codex"];
        assert_eq!(codex.manager.as_deref(), Some("bun"));
        assert_eq!(codex.asset, None);
        assert!(codex.completions.is_empty());
    }

    #[test]
    fn load_or_default_on_missing_file() {
        let lock = load_or_default(Path::new("/nonexistent/rig.lock")).expect("no error");
        assert!(lock.tool.is_empty());
    }

    #[test]
    fn state_lock_creates_its_lock_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let lock_path = tmp.path().join("rig.lock.lock");
        assert!(!lock_path.exists());

        let _lock = StateLock::acquire(&lock_path).expect("acquire on a fresh state_dir");
        assert!(lock_path.exists(), "lock file must be created");
    }

    #[test]
    fn state_lock_blocks_a_second_opener_until_released() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let tmp = tempfile::tempdir().expect("tempdir");
        let lock_path = tmp.path().join("rig.lock.lock");
        let first = StateLock::acquire(&lock_path).expect("first opener");

        // flock is per open-file-description, not per process: a second
        // open + flock from another thread blocks like a second process.
        let (tx, rx) = mpsc::channel();
        let second_path = lock_path.clone();
        let holder = std::thread::spawn(move || {
            let start = Instant::now();
            let _second = StateLock::acquire(&second_path).expect("second opener");
            tx.send(start.elapsed()).expect("send elapsed");
        });

        std::thread::sleep(Duration::from_millis(200));
        drop(first);
        let waited = rx.recv().expect("second opener finished");
        holder.join().expect("holder joined");

        assert!(
            waited >= Duration::from_millis(100),
            "second opener must have blocked, waited {waited:?}"
        );
    }

    #[test]
    fn try_acquire_yields_none_while_held() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let lock_path = tmp.path().join("rig.lock.lock");

        // Held for the whole test — its fd never closes, so a contending
        // try_acquire deterministically sees EWOULDBLOCK, no fork-race flake.
        let _held = StateLock::acquire(&lock_path).expect("first holder");
        assert!(
            StateLock::try_acquire(&lock_path)
                .expect("try_acquire must not error")
                .is_none(),
            "try_acquire must yield None while the lock is held"
        );
    }

    #[test]
    fn save_and_sync_writes_both_lock_and_init_zsh() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).expect("mkdir home");
        let layout = crate::paths::Layout::new(&home, "~/.local");

        let config = crate::config::Config::default();
        let lock = Lock::default();
        save_and_sync(&config, &lock, &layout).expect("should write both files");

        assert!(layout.lock_path.exists());
        assert!(layout.init_zsh_path.exists());
    }

    #[test]
    fn save_backs_up_the_previous_generation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("rig.lock");
        let backup = tmp.path().join(".rig.lock");

        let mut lock = Lock::default();
        save(&lock, &path).expect("first save");
        assert!(!backup.exists(), "nothing to back up on first save");

        lock.tool.insert(
            "delta".to_string(),
            ToolLock {
                version: "0.18.2".to_string(),
                source: "github:dandavison/delta".to_string(),
                installed_at: ts("2026-08-03T12:00:00Z"),
                bins: vec!["~/.local/bin/delta".to_string()],
                completions: vec![],
                asset: None,
                size: None,
                pkg: None,
                manager: None,
                root: None,
                eval_cacheable: None,
                eval_cached_output: None,
                eval_evidence: Vec::new(),
            },
        );
        save(&lock, &path).expect("second save");

        let backed_up = fs::read_to_string(&backup).expect("backup exists after second save");
        assert!(
            !backed_up.contains("delta"),
            "backup should hold the pre-update (empty) lock, not the new one"
        );
        let current = fs::read_to_string(&path).expect("current lock exists");
        assert!(current.contains("delta"));
    }
}
