//! `[[apt]]`: system packages via `apt-get`. Updates are left to `apt
//! upgrade` — rig only handles the initial install. dpkg owns file
//! placement system-wide, so there's no `pkg/<tool>/<version>` tree here
//! and no symlinks for rig to create; it just records what dpkg reports.

use std::process::Command;

use anyhow::{Context, bail};
use time::OffsetDateTime;

use crate::config::AptEntry;
use crate::lock::ToolLock;
use crate::paths::Layout;

use super::{Phase, apply_env, collect_completions, resolve_declared_bins, run_setup, tool_key};

pub fn install(entry: &AptEntry, layout: &Layout, phase: Phase) -> anyhow::Result<ToolLock> {
    let tool = tool_key(&entry.name);

    eprintln!(
        "installing {} via `sudo apt-get install` — outside rig's own pkg tree, \
         `rig remove` will run `apt-get remove` to undo it",
        entry.name
    );
    let mut command = Command::new("sudo");
    command.args(["apt-get", "install", "-y"]).arg(&entry.name);
    apply_env(&mut command, &entry.common.env)?;
    let status = command
        .status()
        .with_context(|| format!("failed to run `sudo apt-get install -y {}`", entry.name))?;
    if !status.success() {
        bail!("apt-get install -y {} failed ({status})", entry.name);
    }

    let version = dpkg_version(&entry.name)?;

    // No unpack dir for this source type, so setup/completions hooks get a
    // rig-created scratch dir instead.
    let scratch = layout.pkg_dir(tool, &version);
    std::fs::create_dir_all(&scratch)
        .with_context(|| format!("failed to create {}", scratch.display()))?;

    if let Some(setup) = &entry.common.setup {
        run_setup(setup, &scratch, phase, &entry.common.env)
            .with_context(|| format!("setup hook failed for {tool}"))?;
    }

    let bins = resolve_declared_bins(
        entry.common.bin.as_ref(),
        tool,
        &format!("installing {}", entry.name),
    )?;
    let completions = collect_completions(
        &scratch,
        &scratch,
        &scratch,
        &entry.common.completions,
        &layout.completions_dir,
        &entry.common.env,
    )?;

    let (eval_cacheable, eval_cached_output, eval_evidence) =
        super::resolve_eval_cache(entry.common.eval.as_ref());

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
