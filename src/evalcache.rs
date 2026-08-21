//! `eval` cacheability probing. Deny-list only — any judge hit means
//! "not cacheable"; new judges can only make the verdict more conservative,
//! never flip a cacheable verdict back.

use std::collections::HashSet;
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

    let tmpdir = std::env::var("TMPDIR").unwrap_or_default();
    reasons.extend(content_hits(&baseline, &current_path, &tmpdir));

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
fn content_hits(output: &str, path: &str, tmpdir: &str) -> Vec<String> {
    let mut hits = Vec::new();
    if output.contains("/run/user/") {
        hits.push("content:/run/user/".to_string());
    }
    if output.contains("/proc/") {
        hits.push("content:/proc/".to_string());
    }
    if contains_tmp_dir(output, tmpdir) {
        hits.push("content:tmp-dir".to_string());
    }
    if output.contains(&std::process::id().to_string()) {
        hits.push("content:pid".to_string());
    }
    if path_component_hits(output, path) >= 3 {
        hits.push("content:path-components".to_string());
    }
    hits
}

fn contains_tmp_dir(output: &str, tmpdir: &str) -> bool {
    let tmpdir = tmpdir.trim_end_matches('/');
    if !tmpdir.is_empty() && contains_path_component(output, tmpdir) {
        return true;
    }
    // mktemp(1)'s own convention, e.g. `/tmp/tmp.XXXXXXXXXX`.
    output
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '.')
        .any(|word| word.len() > 4 && word.starts_with("tmp."))
}

/// Takes `PATH` as a parameter rather than reading it internally, so the
/// counting rules are testable without mutating process environment.
fn path_component_hits(output: &str, path: &str) -> usize {
    let mut seen = HashSet::new();
    path.split(':')
        // A trailing slash names the same directory; bare `/` matches every
        // path and is evidence of nothing.
        .map(|component| component.trim_end_matches('/'))
        .filter(|component| {
            !component.is_empty()
                && seen.insert(*component)
                && contains_path_component(output, component)
        })
        .count()
}

/// Non-empty `component` only. Plain `contains` overcounts twice: a duplicated
/// `PATH` entry is one directory, and `/bin` sits inside `~/.local/bin`.
fn contains_path_component(output: &str, component: &str) -> bool {
    output.match_indices(component).any(|(idx, matched)| {
        let starts_clean = output[..idx]
            .chars()
            .next_back()
            .is_none_or(|prev| !is_path_char(prev));
        let ends_clean = output[idx + matched.len()..]
            .chars()
            .next()
            .is_none_or(|next| next == '/' || !is_path_char(next));
        starts_clean && ends_clean
    })
}

fn is_path_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '.' | '-' | '_' | '/')
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
    fn duplicate_path_entries_count_as_one_directory() {
        let path = "/home/k/.local/bin:/home/k/.local/bin:/home/k/.local/bin";
        assert_eq!(
            path_component_hits("run /home/k/.local/bin/starship", path),
            1
        );
    }

    #[test]
    fn short_component_does_not_match_inside_a_longer_path() {
        // `/bin` is a suffix of `/home/k/.local/bin`, not an occurrence of it.
        assert_eq!(
            path_component_hits("/home/k/.local/bin/starship", "/bin"),
            0
        );
        // `/usr/bin` must not be credited to the unrelated `/usr/bin2`.
        assert_eq!(path_component_hits("look in /usr/bin2/foo", "/usr/bin"), 0);
    }

    #[test]
    fn component_counts_at_a_path_boundary() {
        assert_eq!(path_component_hits("added /bin to it", "/bin"), 1);
        assert_eq!(path_component_hits("/bin/sh -c", "/bin"), 1);
        assert_eq!(path_component_hits("\"/bin\"", "/bin"), 1);
    }

    #[test]
    fn distinct_baked_in_directories_still_trip_the_judge() {
        let path = "/opt/a/bin:/opt/b/bin:/opt/c/bin:/opt/d/bin";
        let output = "PATH=/opt/a/bin:/opt/b/bin:/opt/c/bin";
        assert_eq!(path_component_hits(output, path), 3);
    }

    #[test]
    fn trailing_slashes_do_not_break_the_match() {
        assert_eq!(path_component_hits("/usr/bin/ls", "/usr/bin/"), 1);
        // Same directory written two ways is still one directory.
        assert_eq!(path_component_hits("/usr/bin/ls", "/usr/bin:/usr/bin/"), 1);
    }

    #[test]
    fn bare_root_component_is_not_evidence() {
        assert_eq!(path_component_hits("/usr/bin/ls", "/"), 0);
    }

    #[test]
    fn empty_path_components_are_skipped() {
        assert_eq!(path_component_hits("anything", ""), 0);
        assert_eq!(path_component_hits("/bin/sh", "::/bin::"), 1);
    }

    #[test]
    fn tmpdir_matches_a_clean_component() {
        assert!(contains_tmp_dir(
            "export TMPDIR=/tmp/rig-probe",
            "/tmp/rig-probe"
        ));
        assert!(contains_tmp_dir(
            "cd /tmp/rig-probe && echo ok",
            "/tmp/rig-probe"
        ));
    }

    #[test]
    fn tmpdir_with_trailing_slash_matches() {
        assert!(contains_tmp_dir(
            "export TMPDIR=/tmp/rig-probe",
            "/tmp/rig-probe/"
        ));
    }

    #[test]
    fn tmpdir_does_not_match_inside_a_longer_path() {
        // `/tmp/rig` is a prefix of `/tmp/rig-probe`, not an occurrence of it.
        assert!(!contains_tmp_dir(
            "export TMPDIR=/tmp/rig-probe",
            "/tmp/rig"
        ));
    }

    #[test]
    fn empty_tmpdir_falls_back_to_mktemp_convention() {
        assert!(!contains_tmp_dir("export TMPDIR=", ""));
        assert!(contains_tmp_dir("created /tmp/tmp.abc123", ""));
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
