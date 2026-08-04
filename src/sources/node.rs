//! `[[node]]` — bun-first, npm-fallback. Neither package manager gets
//! rig's own `pkg/<tool>/<version>` treatment: each owns its global
//! module tree, so rig records where it landed rather than relocating it.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, bail};
use time::OffsetDateTime;

use crate::config::{BinSpec, NodeEntry, NodeManager};
use crate::lock::{Lock, ToolLock};
use crate::paths::Layout;
use crate::version::npm;

use super::tool_key;

pub enum ResolvedManager {
    Bun,
    Npm,
}

impl ResolvedManager {
    fn binary_name(&self) -> &'static str {
        match self {
            Self::Bun => "bun",
            Self::Npm => "npm",
        }
    }

    /// Update reuses the recorded manager, never re-probes PATH.
    pub fn parse(recorded: &str) -> anyhow::Result<Self> {
        match recorded {
            "bun" => Ok(Self::Bun),
            "npm" => Ok(Self::Npm),
            other => bail!("unknown node package manager recorded in rig.lock: {other}"),
        }
    }
}

fn on_path(bin: &str) -> bool {
    super::resolve_on_path(bin).is_ok()
}

/// Explicit choices never fall back — only `auto` probes both.
fn detect_manager(configured: NodeManager) -> anyhow::Result<ResolvedManager> {
    match configured {
        NodeManager::Auto if on_path("bun") => Ok(ResolvedManager::Bun),
        NodeManager::Auto if on_path("npm") => Ok(ResolvedManager::Npm),
        NodeManager::Auto => bail!("neither bun nor npm found on PATH; run `rig install bun`"),
        NodeManager::Bun if on_path("bun") => Ok(ResolvedManager::Bun),
        NodeManager::Bun => bail!("bun is configured but not found on PATH"),
        NodeManager::Npm if on_path("npm") => Ok(ResolvedManager::Npm),
        NodeManager::Npm => bail!("npm is configured but not found on PATH"),
    }
}

pub fn install(
    entry: &NodeEntry,
    layout: &Layout,
    lock: &Lock,
    phase: super::Phase,
) -> anyhow::Result<ToolLock> {
    let manager = detect_manager(entry.manager)?;
    install_with_manager(entry, layout, lock, manager, phase)
}

pub fn install_with_manager(
    entry: &NodeEntry,
    layout: &Layout,
    lock: &Lock,
    manager: ResolvedManager,
    phase: super::Phase,
) -> anyhow::Result<ToolLock> {
    let info = npm::latest(&entry.name)?;
    let bin_names = declared_bin_names(entry, &info);

    let (root, bin_dir) = match manager {
        ResolvedManager::Bun => install_via_bun(entry, layout, &info.version, &bin_names, lock)?,
        ResolvedManager::Npm => install_via_npm(entry, &info.version, &bin_names, lock)?,
    };

    // No unpack dir for this source type, so setup/completions hooks get a
    // rig-created scratch dir instead.
    let scratch = layout.pkg_dir(tool_key(&entry.name), &info.version);
    fs::create_dir_all(&scratch)
        .with_context(|| format!("failed to create {}", scratch.display()))?;

    if let Some(setup) = &entry.common.setup {
        super::run_setup(setup, &scratch, phase, &entry.common.env)
            .with_context(|| format!("setup hook failed for {}", entry.name))?;
    }

    let mut bins = Vec::new();
    for cmd in &bin_names {
        let path = bin_dir.join(cmd);
        if !path.exists() {
            bail!(
                "expected {} to exist after installing {} via {}, but it doesn't",
                path.display(),
                entry.name,
                manager.binary_name(),
            );
        }
        bins.push(path.display().to_string());
    }
    let completions = super::collect_completions(
        &scratch,
        &scratch,
        &entry.common.completions,
        &layout.completions_dir,
        &entry.common.env,
    )?;

    let (eval_cacheable, eval_cached_output, eval_evidence) =
        super::resolve_eval_cache(entry.common.eval.as_ref());

    Ok(ToolLock {
        version: info.version,
        source: format!("node:{}", entry.name),
        installed_at: OffsetDateTime::now_utc(),
        bins,
        completions: completions
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        asset: None,
        size: None,
        pkg: Some(scratch.display().to_string()),
        manager: Some(manager.binary_name().to_string()),
        root: Some(root.display().to_string()),
        eval_cacheable,
        eval_cached_output,
        eval_evidence,
    })
}

fn declared_bin_names(entry: &NodeEntry, info: &npm::NpmInfo) -> Vec<String> {
    if let Some(spec) = &entry.common.bin {
        BinSpec::declared_names(Some(spec), tool_key(&entry.name))
    } else if !info.bin.is_empty() {
        info.bin.keys().cloned().collect()
    } else {
        vec![tool_key(&entry.name).to_string()]
    }
}

/// Same rationale as `[[cargo]]`'s `--root`, applied to bun: don't inherit
/// `$BUN_INSTALL` from whatever the caller's shell happens to have.
fn install_via_bun(
    entry: &NodeEntry,
    layout: &Layout,
    version: &str,
    bin_names: &[String],
    lock: &Lock,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    let prefix_dir = layout
        .prefix_bin_dir
        .parent()
        .context("prefix_bin_dir must have a parent")?;

    // Bun writes into rig's shared prefix_bin_dir, so it needs the same
    // untracked-conflict guard collect_bins gives every other source.
    for name in bin_names {
        super::check_conflict(&layout.prefix_bin_dir.join(name), lock)?;
    }

    let mut command = Command::new("bun");
    command
        .args(["install", "-g", &format!("{}@{version}", entry.name)])
        .env("BUN_INSTALL", prefix_dir);
    super::apply_env(&mut command, &entry.common.env)?;
    let status = command
        .status()
        .with_context(|| format!("failed to run `bun install -g {}`", entry.name))?;
    if !status.success() {
        bail!("bun install -g {} failed ({status})", entry.name);
    }

    let root = prefix_dir
        .join("install/global/node_modules")
        .join(&entry.name);
    Ok((root, layout.prefix_bin_dir.clone()))
}

fn install_via_npm(
    entry: &NodeEntry,
    version: &str,
    bin_names: &[String],
    lock: &Lock,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    let output = Command::new("npm")
        .args(["prefix", "-g"])
        .output()
        .context("failed to run `npm prefix -g`")?;
    if !output.status.success() {
        bail!("npm prefix -g failed");
    }
    let npm_prefix = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let bin_dir = npm_prefix.join("bin");

    for name in bin_names {
        super::check_conflict(&bin_dir.join(name), lock)?;
    }

    let mut command = Command::new("npm");
    command.args(["install", "-g", &format!("{}@{version}", entry.name)]);
    super::apply_env(&mut command, &entry.common.env)?;
    let status = command
        .status()
        .with_context(|| format!("failed to run `npm install -g {}`", entry.name))?;
    if !status.success() {
        bail!("npm install -g {} failed ({status})", entry.name);
    }

    let root = npm_prefix.join("lib/node_modules").join(&entry.name);
    Ok((root, bin_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Common, CompletionsSpec};
    use std::collections::HashMap;

    #[test]
    fn manager_round_trips_through_its_recorded_name() {
        assert!(matches!(
            ResolvedManager::parse(ResolvedManager::Bun.binary_name()),
            Ok(ResolvedManager::Bun)
        ));
        assert!(matches!(
            ResolvedManager::parse(ResolvedManager::Npm.binary_name()),
            Ok(ResolvedManager::Npm)
        ));
        assert!(ResolvedManager::parse("yarn").is_err());
    }

    #[test]
    fn install_via_bun_refuses_an_untracked_conflicting_bin() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        let layout = Layout::new(&home, "~/.local");
        std::fs::create_dir_all(&layout.prefix_bin_dir).expect("mkdir prefix_bin_dir");
        std::fs::write(layout.prefix_bin_dir.join("cowsay"), b"#!/bin/sh\n")
            .expect("write foreign bin");

        let entry = cowsay_entry();
        let lock = Lock::default();
        let err = install_via_bun(&entry, &layout, "1.0.0", &["cowsay".to_string()], &lock)
            .expect_err("an untracked file already at the target path must block the install");
        assert!(err.to_string().contains("cowsay"));
    }

    fn cowsay_entry() -> NodeEntry {
        NodeEntry {
            name: "cowsay".to_string(),
            manager: NodeManager::Bun,
            common: Common {
                description: None,
                bin: None,
                env: HashMap::new(),
                eval: None,
                completions: CompletionsSpec::Enabled(false),
                setup: None,
            },
        }
    }

    #[test]
    #[ignore = "installs a real (tiny) npm package via bun; run with `cargo test -- --ignored`"]
    fn installs_cowsay_via_bun_into_a_scratch_prefix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        let layout = Layout::new(&home, "~/.local");

        let entry = cowsay_entry();
        let lock = Lock::default();
        let tool_lock = install(&entry, &layout, &lock, crate::sources::Phase::Install)
            .expect("cowsay should install via bun");

        assert_eq!(tool_lock.manager.as_deref(), Some("bun"));
        // cowsay's package.json declares two binaries — both should resolve.
        assert_eq!(tool_lock.bins.len(), 2);
        for bin in &tool_lock.bins {
            assert!(std::path::Path::new(bin).exists(), "{bin} should exist");
        }
    }
}
