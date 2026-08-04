//! `eval` cacheability probing. Deny-list only — any judge hit means
//! "not cacheable"; new judges can only make the verdict more conservative,
//! never flip a cacheable verdict back.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail};

#[derive(Debug)]
pub struct Probe {
    pub cacheable: bool,
    /// Which judges fired — becomes the source-comment evidence in init.zsh.
    pub reasons: Vec<String>,
    /// The command's first-run output, for the caller to cache if `cacheable`.
    pub baseline_output: String,
}

/// Single invocation, no differential re-runs — for `cache = true`, which
/// already asserts cacheability and just needs output to embed.
pub fn capture(cmd: &str) -> anyhow::Result<String> {
    run(cmd, None, &[])
}

pub fn probe(cmd: &str) -> anyhow::Result<Probe> {
    let baseline = run(cmd, None, &[])?;
    let mut reasons = Vec::new();

    let cwd_root = run(cmd, Some(Path::new("/")), &[])?;
    let cwd_tmp = run(cmd, Some(Path::new("/tmp")), &[])?;
    if cwd_root != cwd_tmp {
        reasons.push("cwd".to_string());
    }

    let term_variant = run(
        cmd,
        None,
        &[("TERM", "dumb".to_string()), ("COLUMNS", "40".to_string())],
    )?;
    if term_variant != baseline {
        reasons.push("TERM".to_string());
    }

    let current_path = std::env::var("PATH").unwrap_or_default();
    let probe_dir = "/tmp/rig-eval-probe-does-not-exist";
    let path_variant = run(
        cmd,
        None,
        &[("PATH", format!("{current_path}:{probe_dir}"))],
    )?;
    if path_variant != baseline {
        reasons.push("PATH".to_string());
    }

    let shlvl_variant = run(cmd, None, &[("SHLVL", "9".to_string())])?;
    if shlvl_variant != baseline {
        reasons.push("SHLVL".to_string());
    }

    let rerun = run(cmd, None, &[])?;
    if rerun != baseline {
        reasons.push("nondeterministic".to_string());
    }

    reasons.extend(content_hits(&baseline));

    Ok(Probe {
        cacheable: reasons.is_empty(),
        reasons,
        baseline_output: baseline,
    })
}

fn run(cmd: &str, cwd: Option<&Path>, env_overrides: &[(&str, String)]) -> anyhow::Result<String> {
    let mut command = Command::new("sh");
    command.arg("-c").arg(cmd);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    for (key, value) in env_overrides {
        command.env(key, value);
    }

    let output = command
        .output()
        .with_context(|| format!("failed to run probe command: {cmd}"))?;
    if !output.status.success() {
        bail!("probe command failed ({}): {cmd}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Content-family fallback: baked-in session-specific data that the
/// differential probes above might not happen to disturb.
fn content_hits(output: &str) -> Vec<String> {
    let mut hits = Vec::new();
    if output.contains("/run/user/") {
        hits.push("content:/run/user/".to_string());
    }
    if output.contains("/proc/") {
        hits.push("content:/proc/".to_string());
    }
    if contains_tmp_dir(output) {
        hits.push("content:tmp-dir".to_string());
    }
    if output.contains(&std::process::id().to_string()) {
        hits.push("content:pid".to_string());
    }
    if path_component_hits(output) >= 3 {
        hits.push("content:path-components".to_string());
    }
    hits
}

fn contains_tmp_dir(output: &str) -> bool {
    if let Ok(tmpdir) = std::env::var("TMPDIR")
        && !tmpdir.is_empty()
        && output.contains(&tmpdir)
    {
        return true;
    }
    // mktemp(1)'s own convention, e.g. `/tmp/tmp.XXXXXXXXXX`.
    output
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '.')
        .any(|word| word.len() > 4 && word.starts_with("tmp."))
}

fn path_component_hits(output: &str) -> usize {
    std::env::var("PATH")
        .map(|path| {
            path.split(':')
                .filter(|component| !component.is_empty() && output.contains(component))
                .count()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_output_is_cacheable() {
        let result = probe("echo static-output").expect("probe should run");
        assert!(result.cacheable, "reasons: {:?}", result.reasons);
    }

    #[test]
    fn cwd_dependent_output_is_not_cacheable() {
        let result = probe("pwd").expect("probe should run");
        assert!(!result.cacheable);
        assert!(result.reasons.contains(&"cwd".to_string()));
    }

    #[test]
    fn path_dependent_output_is_not_cacheable() {
        let result = probe("echo $PATH").expect("probe should run");
        assert!(!result.cacheable);
        assert!(result.reasons.contains(&"PATH".to_string()));
    }

    #[test]
    fn nondeterministic_output_is_not_cacheable() {
        let result = probe("date +%s%N").expect("probe should run");
        assert!(!result.cacheable);
        assert!(result.reasons.contains(&"nondeterministic".to_string()));
    }

    #[test]
    #[ignore = "depends on tools installed on this machine; run with `cargo test -- --ignored`"]
    fn matches_real_measured_verdicts() {
        // Real measurements on this machine: starship/zoxide cache clean,
        // mise doesn't (PATH-rewriting activate script).
        let starship = probe("starship init zsh").expect("starship should be on PATH");
        assert!(starship.cacheable, "reasons: {:?}", starship.reasons);

        let zoxide = probe("zoxide init zsh").expect("zoxide should be on PATH");
        assert!(zoxide.cacheable, "reasons: {:?}", zoxide.reasons);

        let mise = probe("mise activate zsh").expect("mise should be on PATH");
        assert!(!mise.cacheable);
        assert!(
            mise.reasons.contains(&"PATH".to_string()),
            "reasons: {:?}",
            mise.reasons
        );
    }
}
