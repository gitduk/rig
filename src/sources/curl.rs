//! `[[curl]]`: installer scripts. No clean version concept exists for this
//! source type (update reliability "depends on the script"), so `bins` are
//! resolved the same way as `[[apt]]` — via PATH, after the script has done
//! whatever placement it does on its own. No script integrity check, by
//! design: a hash pin breaks on every upstream roll and misses the binary
//! (script downloads it itself). Guarantees: TLS (ureq default), and file
//! execution from pkg/<tool>/install rather than `curl | sh`.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail};
use time::OffsetDateTime;

use crate::config::CurlEntry;
use crate::lock::ToolLock;
use crate::paths::Layout;

use super::{
    Phase, USER_AGENT, apply_env, collect_completions, resolve_declared_bins, run_setup, tool_key,
};

pub fn install(entry: &CurlEntry, layout: &Layout, phase: Phase) -> anyhow::Result<ToolLock> {
    let tool = tool_key(&entry.name);
    // `[[curl]]` has no version to compare, so an update is a plain rerun —
    // `Phase::verb`'s "updating" would overstate what happens here.
    match phase {
        Phase::Install => eprintln!("{tool}: installing via curl ({})", entry.url),
        Phase::Update => eprintln!("{tool}: rerunning install script ({})", entry.url),
    }
    let script = fetch_script(&entry.url)?;

    let work_dir = layout.tool_pkg_dir(tool).join("install");
    fs::create_dir_all(&work_dir)
        .with_context(|| format!("failed to create {}", work_dir.display()))?;

    // Run as its own file, not via `sh -c` — real installers often declare
    // `#!/bin/bash` and use syntax a POSIX `sh` (e.g. dash) can't parse.
    let script_path = work_dir.join("install.sh");
    fs::write(&script_path, &script)
        .with_context(|| format!("failed to write {}", script_path.display()))?;
    let mut perms = fs::metadata(&script_path)
        .with_context(|| format!("failed to stat {}", script_path.display()))?
        .permissions();
    perms.set_mode(perms.mode() | 0o111);
    fs::set_permissions(&script_path, perms)
        .with_context(|| format!("failed to chmod +x {}", script_path.display()))?;

    fs::create_dir_all(&layout.prefix_bin_dir)
        .with_context(|| format!("failed to create {}", layout.prefix_bin_dir.display()))?;

    let mut command = Command::new(&script_path);
    command.current_dir(&work_dir);
    command.env("PATH", pinned_path(&layout.prefix_bin_dir));
    apply_env(&mut command, &entry.common.env)?;
    let status = command
        .status()
        .with_context(|| format!("failed to run install script for {tool}"))?;
    if !status.success() {
        bail!("install script for {tool} failed ({status})");
    }

    if let Some(setup) = &entry.common.setup {
        run_setup(setup, &work_dir, phase, &entry.common.env)
            .with_context(|| format!("setup hook failed for {tool}"))?;
    }

    let bins = resolve_declared_bins(
        entry.common.bin.as_ref(),
        tool,
        Some(&layout.prefix_bin_dir),
        &format!("running {tool}'s install script"),
    )?;
    let completions = collect_completions(
        &work_dir,
        &work_dir,
        &work_dir,
        &entry.common.completions,
        &layout.completions_dir,
        &entry.common.env,
    )?;

    let (eval_cacheable, eval_cached_output, eval_evidence) =
        super::resolve_eval_cache(entry.common.eval.as_ref());

    Ok(ToolLock {
        version: crate::version::UNVERSIONED.to_string(),
        source: format!("curl:{}", entry.name),
        installed_at: OffsetDateTime::now_utc(),
        bins,
        completions: completions
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        asset: None,
        size: None,
        pkg: Some(work_dir.display().to_string()),
        manager: None,
        root: None,
        eval_cacheable,
        eval_cached_output,
        eval_evidence,
    })
}

/// Prepends `prefix_bin_dir` so installers that scan `$PATH` for the first
/// writable directory land there, not on some other writable PATH entry.
fn pinned_path(prefix_bin_dir: &Path) -> OsString {
    let mut path = OsString::from(prefix_bin_dir);
    if let Some(existing) = std::env::var_os("PATH") {
        path.push(":");
        path.push(existing);
    }
    path
}

fn fetch_script(url: &str) -> anyhow::Result<String> {
    ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .with_context(|| format!("failed to download {url}"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("failed to read script body from {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_path_prepends_prefix_bin_dir() {
        // Recomputed, not hardcoded — must hold even where PATH is unset.
        let expected_suffix = std::env::var_os("PATH")
            .map(|p| format!(":{}", p.to_string_lossy()))
            .unwrap_or_default();

        let result = pinned_path(Path::new("/opt/tools/bin"));
        assert_eq!(
            result.to_string_lossy(),
            format!("/opt/tools/bin{expected_suffix}")
        );
    }
}
