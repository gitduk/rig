//! `rig.lock` schema — installed-tool state, keyed by tool name. `ToolLock`
//! stays flat rather than a tagged enum per source type: the field sets for
//! repo/git-style and node-style entries don't collide, so a tag would add
//! ceremony without resolving any ambiguity.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::config::Config;
use crate::initzsh;
use crate::paths::Layout;

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Lock {
    #[serde(default)]
    pub tool: BTreeMap<String, ToolLock>,
}

#[derive(Debug, Deserialize, Serialize)]
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

pub fn save(lock: &Lock, path: &Path) -> anyhow::Result<()> {
    let text = toml::to_string_pretty(lock).context("failed to serialize rig.lock")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

/// Lock must land before init.zsh. Call after every tool, not once at
/// batch end, so a killed process doesn't lose already-finished work.
pub fn save_and_sync(config: &Config, lock: &Lock, layout: &Layout) -> anyhow::Result<()> {
    save(lock, &layout.lock_path)?;
    let rendered = initzsh::render(config, lock, layout);
    if let Some(parent) = layout.init_zsh_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&layout.init_zsh_path, rendered)
        .with_context(|| format!("failed to write {}", layout.init_zsh_path.display()))
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
}
