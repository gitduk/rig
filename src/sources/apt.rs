//! `[[apt]]`: system packages via `apt-get`. `rig update` compares the
//! locked version against apt's local candidate and only reruns the install
//! (under sudo) when they differ. dpkg owns file placement system-wide, so there's no
//! `pkg/<tool>/<version>` tree here and no symlinks for rig to create; it
//! just records what dpkg reports.

use std::process::Command;

use anyhow::{Context, bail};
use time::OffsetDateTime;

use crate::config::AptEntry;
use crate::lock::ToolLock;
use crate::paths::Layout;

use crate::output::{progress, warning};

use super::{
    Phase, apply_env, collect_completions, resolve_declared_bins, run_child_in, run_setup, tool_key,
};

pub fn install(entry: &AptEntry, layout: &Layout, phase: Phase) -> anyhow::Result<ToolLock> {
    let tool = tool_key(&entry.name);

    progress!("{tool}: {} via sudo apt-get install", phase.verb());
    // Only on install: every `rig update` reruns apt-get, and repeating the
    // caveat on each one is noise rather than news.
    if matches!(phase, Phase::Install) {
        warning!(
            "{tool}: warning: apt packages land outside rig's pkg tree; \
             `rig remove` will run `apt-get remove` to undo"
        );
    }
    let mut command = Command::new("sudo");
    command.args(["apt-get", "install", "-y"]).arg(&entry.name);
    apply_env(&mut command, &entry.common.env)?;
    // sudo prompts on /dev/tty, so capturing apt's chatter on update keeps
    // the prompt visible; a fresh install still streams its progress.
    run_child_in(
        &mut command,
        &format!("`sudo apt-get install -y {}`", entry.name),
        crate::output::streaming() && matches!(phase, Phase::Install),
    )?;

    let version = dpkg_version(&entry.name)?;

    // No unpack dir for this source type, so setup/completions hooks get a
    // rig-created scratch dir instead.
    let scratch = layout.pkg_dir(tool, &version);
    std::fs::create_dir_all(&scratch)
        .with_context(|| format!("failed to create {}", scratch.display()))?;

    if let Some(setup) = &entry.common.setup {
        run_setup(tool, setup, &scratch, phase, &entry.common.env)
            .with_context(|| format!("setup hook failed for {tool}"))?;
    }

    let bins = resolve_declared_bins(
        entry.common.bin.as_ref(),
        tool,
        None,
        &format!("installing {}", entry.name),
    )?;
    let completions = collect_completions(
        tool,
        &scratch,
        &scratch,
        &scratch,
        &entry.common.completions,
        &layout.completions_dir,
        &entry.common.env,
    )?;

    let (eval_cacheable, eval_cached_output, eval_evidence) =
        super::resolve_eval_cache(tool, entry.common.eval.as_ref());

    Ok(ToolLock {
        version,
        source: format!("apt:{}", entry.name),
        installed_at: OffsetDateTime::now_utc(),
        bins,
        completions: completions
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        asset: None,
        size: None,
        pkg: Some(scratch.display().to_string()),
        manager: None,
        root: None,
        eval_cacheable,
        eval_cached_output,
        eval_evidence,
    })
}

fn dpkg_version(package: &str) -> anyhow::Result<String> {
    let output = Command::new("dpkg-query")
        .args(["-W", "-f=${Version}", package])
        .output()
        .with_context(|| format!("failed to run `dpkg-query` for {package}"))?;
    if !output.status.success() {
        bail!(
            "dpkg-query couldn't find {package} after install: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The version `apt-get install` would land, from the local package index
/// (no sudo, no network). `None` when apt has no candidate or can't be read.
pub fn candidate_version(package: &str) -> Option<String> {
    let output = Command::new("apt-cache")
        .args(["policy", package])
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_candidate(&String::from_utf8_lossy(&output.stdout))
}

fn parse_candidate(policy: &str) -> Option<String> {
    policy
        .lines()
        .find_map(|l| l.trim().strip_prefix("Candidate:"))
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "(none)")
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_candidate_from_policy_output() {
        let policy = "git:\n  Installed: 1:2.53.0-1ubuntu1\n  Candidate: 1:2.53.0-1ubuntu2\n";
        assert_eq!(
            parse_candidate(policy).as_deref(),
            Some("1:2.53.0-1ubuntu2")
        );
    }

    #[test]
    fn no_candidate_when_apt_has_none() {
        assert_eq!(
            parse_candidate("foo:\n  Installed: (none)\n  Candidate: (none)\n"),
            None
        );
        assert_eq!(parse_candidate(""), None);
    }
}
