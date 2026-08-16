//! Shared installer mechanics: asset selection, download, extraction,
//! bin/completions collection with conflict checks, and setup hook
//! execution. Per-source-type modules (`repo.rs`, ...) orchestrate these
//! into the full install flow for their config shape.

pub mod apt;
pub mod cargo;
pub mod curl;
pub mod git;
pub mod node;
pub mod plugin;
pub mod repo;

use std::collections::HashMap;
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

use crate::config::{BinSpec, BpickSpec, Common, CompletionsSpec, EvalSpec, SetupSpec};
use crate::lock::Lock;
use crate::paths;
use crate::version::github::Asset;

/// Re-exported: definition moved to `config.rs` (where `ResolvedEntry` also needs it).
pub use crate::config::tool_key;

/// Shared HTTP client identity — `curl.rs`, `version/*`, and `download`
/// all speak for the same program.
pub const USER_AGENT: &str = concat!("rig/", env!("CARGO_PKG_VERSION"));

/// `Layout::new` hardcodes this same path unconditionally — no override
/// mechanism exists, so error messages can name it as a literal.
const CONFIG_PATH_HINT: &str = "~/.rig.toml";

/// Shared by `git.rs` (post-clone, before symlinking) and `plugin.rs`
/// (post-clone/pull, as the plugin's own version marker).
pub fn git_head_commit(repo_dir: &Path) -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_dir)
        .output()
        .context("failed to run `git rev-parse HEAD`")?;
    if !output.status.success() {
        bail!(
            "git rev-parse HEAD failed in {}: {}",
            repo_dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Applies configured `env` to an install subprocess — never inherit-only,
/// or install location depends on the caller's shell.
pub fn apply_env(cmd: &mut Command, env: &HashMap<String, String>) -> anyhow::Result<()> {
    if env.is_empty() {
        return Ok(());
    }
    let home = paths::home_dir()?;
    for (key, value) in env {
        cmd.env(key, paths::expand_tilde_in(&home, value));
    }
    Ok(())
}

/// For source types where the package manager places files itself
/// (`[[apt]]`, `[[curl]]`) — rig just records where PATH resolves it.
pub fn resolve_on_path(name: &str) -> anyhow::Result<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .output()
        .with_context(|| format!("failed to run `command -v {name}`"))?;
    if !output.status.success() {
        bail!("{name} is not on PATH");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Resolves every declared command name via PATH — `[[apt]]`/`[[curl]]`'s
/// equivalent of `collect_bins` for source types rig doesn't symlink into.
/// `preferred_dir` (checked before PATH) is `[[curl]]`'s pinned install dir;
/// `[[apt]]` passes `None` since dpkg places files itself.
pub fn resolve_declared_bins(
    bin: Option<&BinSpec>,
    tool: &str,
    preferred_dir: Option<&Path>,
    context: &str,
) -> anyhow::Result<Vec<String>> {
    BinSpec::declared_names(bin, tool)
        .iter()
        .map(|name| resolve_bin_name(name, preferred_dir, context))
        .collect()
}

fn resolve_bin_name(
    name: &str,
    preferred_dir: Option<&Path>,
    context: &str,
) -> anyhow::Result<String> {
    if let Some(dir) = preferred_dir {
        let managed = dir.join(name);
        if managed.exists() {
            return Ok(managed.display().to_string());
        }
        eprintln!(
            "warning: {name} landed outside {} after {context} — \
             recording its ambient PATH location instead",
            dir.display()
        );
    }
    resolve_on_path(name).with_context(|| format!("{name} not found on PATH after {context}"))
}

/// Shared by every "picked 0 or N>1 out of a known set" error, so they all
/// render candidates as bullets instead of a raw `{:?}` dump.
fn format_candidates<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    items
        .into_iter()
        .map(|item| format!("\n    - {item}"))
        .collect()
}

/// Kept as data, not a formatted message — only the caller (`repo.rs`) has
/// the release version and config path needed to suggest a real fix.
pub enum AssetPick<'a> {
    Found(&'a Asset),
    NoMatch {
        bpick_configured: bool,
        /// Assets whose name contains this host's arch and OS — a release
        /// lists every platform, and only these are relevant to suggest.
        relevant: Vec<&'a Asset>,
        all: &'a [Asset],
    },
    Ambiguous(Vec<&'a Asset>),
}

/// Explicit `bpick` wins; else a naive `<arch>-<os>` filter.
pub fn select_asset<'a>(assets: &'a [Asset], bpick: Option<&BpickSpec>) -> AssetPick<'a> {
    let arch = host_arch();
    let os = host_os();
    let is_host = |a: &&Asset| a.name.contains(arch) && a.name.contains(os);

    let (matches, bpick_configured) = match bpick {
        Some(spec) => {
            let patterns: Vec<&str> = match spec {
                BpickSpec::Single(p) => vec![p.as_str()],
                BpickSpec::Multiple(ps) => ps.iter().map(String::as_str).collect(),
            };
            let matches: Vec<&Asset> = assets
                .iter()
                .filter(|a| patterns.iter().any(|p| glob_match(p, &a.name)))
                .collect();
            (matches, true)
        }
        None => (assets.iter().filter(is_host).collect(), false),
    };

    match matches.len() {
        1 => AssetPick::Found(matches[0]),
        0 => AssetPick::NoMatch {
            bpick_configured,
            relevant: assets.iter().filter(is_host).collect(),
            all: assets,
        },
        _ => AssetPick::Ambiguous(matches),
    }
}

fn host_arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "unknown"
    }
}

fn host_os() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "unknown"
    }
}

/// `*`-only glob — the doc never uses `?` or character classes.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }

    let last = parts.len() - 1;
    let mut rest = name;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == last {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(pos) => rest = &rest[pos + part.len()..],
                None => return false,
            }
        }
    }
    true
}

/// Downloads to `dest`, streaming through a sha256 while copying. Verifies
/// `expected_size` always; when the host published a `digest`, the hash too
/// — a mismatch means the asset was replaced upstream or corrupted in
/// transit, not just truncated.
pub fn download(
    url: &str,
    dest: &Path,
    expected_size: u64,
    digest: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut response = ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .with_context(|| format!("failed to download {url}"))?;

    let mut file =
        fs::File::create(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut reader = response.body_mut().as_reader();
    let mut written = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("failed to read from {url}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .with_context(|| format!("failed to write {}", dest.display()))?;
        written += n as u64;
    }

    if written != expected_size {
        bail!(
            "downloaded {written} bytes from {url}, expected {expected_size} \
             (truncated download or asset changed upstream)"
        );
    }
    verify_digest(digest, &hasher.finalize()).with_context(|| {
        format!(
            "sha256 of {} doesn't match the release's digest",
            dest.display()
        )
    })
}

/// `sha256:<hex>` is the only shape seen in the wild (GitHub emits it,
/// Codeberg doesn't). Unknown algorithms degrade to size-only checking,
/// matching the no-digest path — never hard-fail on a missing capability.
fn verify_digest(digest: Option<&str>, actual: &[u8]) -> anyhow::Result<()> {
    let Some(digest) = digest else {
        return Ok(());
    };
    let Some(hex) = digest.strip_prefix("sha256:") else {
        eprintln!("warning: unrecognized asset digest `{digest}`, skipping hash check");
        return Ok(());
    };
    let actual_hex = to_hex(actual);
    if !hex.eq_ignore_ascii_case(&actual_hex) {
        bail!("expected {hex}, got {actual_hex}");
    }
    Ok(())
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Symlinks repoint only after success — clear any stale `final_dir`,
/// then atomically rename `.partial` into place.
pub fn finalize_partial(partial_dir: &Path, final_dir: &Path) -> anyhow::Result<()> {
    if final_dir.exists() {
        fs::remove_dir_all(final_dir)
            .with_context(|| format!("failed to remove stale {}", final_dir.display()))?;
    }
    fs::rename(partial_dir, final_dir).with_context(|| {
        format!(
            "failed to move {} into place at {}",
            partial_dir.display(),
            final_dir.display()
        )
    })
}

/// Dispatches by extension; `extract_flag = Some(false)` means a raw
/// binary, copied as-is rather than unpacked. No checksum verification.
pub fn extract(archive: &Path, dest: &Path, extract_flag: Option<bool>) -> anyhow::Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("failed to create {}", dest.display()))?;

    if extract_flag == Some(false) {
        return copy_raw_binary(archive, dest);
    }

    let name = archive
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();

    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        extract_tar_gz(archive, dest)
    } else if name.ends_with(".zip") {
        extract_zip(archive, dest)
    } else if name.ends_with(".deb") {
        extract_deb(archive, dest)
    } else {
        bail!("don't know how to extract {name}; set `extract = false` if this is a raw binary")
    }
}

/// `.deb` is an `ar` archive wrapping `data.tar.*` — `dpkg-deb -x` unpacks
/// straight to `dest` without touching the system package database.
fn extract_deb(archive: &Path, dest: &Path) -> anyhow::Result<()> {
    let status = Command::new("dpkg-deb")
        .arg("-x")
        .arg(archive)
        .arg(dest)
        .status()
        .with_context(|| format!("failed to run `dpkg-deb -x {}`", archive.display()))?;
    if !status.success() {
        bail!("dpkg-deb -x {} failed ({status})", archive.display());
    }
    Ok(())
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> anyhow::Result<()> {
    let file =
        fs::File::open(archive).with_context(|| format!("failed to open {}", archive.display()))?;
    tar::Archive::new(flate2::read::GzDecoder::new(file))
        .unpack(dest)
        .with_context(|| format!("failed to unpack {}", archive.display()))
}

fn extract_zip(archive: &Path, dest: &Path) -> anyhow::Result<()> {
    let file =
        fs::File::open(archive).with_context(|| format!("failed to open {}", archive.display()))?;
    zip::ZipArchive::new(file)
        .with_context(|| format!("failed to read {} as a zip archive", archive.display()))?
        .extract(dest)
        .with_context(|| format!("failed to unpack {}", archive.display()))
}

fn copy_raw_binary(archive: &Path, dest: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let name = archive
        .file_name()
        .context("archive path has no file name")?;
    let dest_path = dest.join(name);
    fs::copy(archive, &dest_path).with_context(|| {
        format!(
            "failed to copy {} into {}",
            archive.display(),
            dest_path.display()
        )
    })?;

    // Raw (unextracted) assets are downloaded as plain data, not marked
    // executable — the archive itself never carries a +x bit.
    let mut perms = fs::metadata(&dest_path)
        .with_context(|| format!("failed to stat {}", dest_path.display()))?
        .permissions();
    perms.set_mode(perms.mode() | 0o111);
    fs::set_permissions(&dest_path, perms)
        .with_context(|| format!("failed to chmod +x {}", dest_path.display()))
}

fn walk_files(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in
            fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))?
        {
            let entry =
                entry.with_context(|| format!("failed to read entry in {}", dir.display()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    Ok(out)
}

fn find_by_name(files: &[PathBuf], name: &str) -> anyhow::Result<PathBuf> {
    let matches: Vec<&PathBuf> = files
        .iter()
        .filter(|p| p.file_name().and_then(|n| n.to_str()) == Some(name))
        .collect();
    match matches.len() {
        0 => {
            let present: Vec<String> = files
                .iter()
                .filter_map(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .collect();
            let list = format_candidates(present.iter().map(String::as_str));
            bail!(
                "no file named {name} found in the unpacked archive\nfiles present:{list}\n\
                 fix: set `bin` for this tool in {CONFIG_PATH_HINT}"
            )
        }
        1 => Ok(matches[0].clone()),
        _ => {
            let paths: Vec<String> = matches.iter().map(|p| p.display().to_string()).collect();
            let list = format_candidates(paths.iter().map(String::as_str));
            bail!(
                "multiple files named {name} found in the unpacked archive:{list}\n\
                 fix: narrow `bin` for this tool in {CONFIG_PATH_HINT}"
            )
        }
    }
}

/// Matches `pattern` against the path relative to `root`, not the basename —
/// a mapped `bin` key like `"target/release/wallust"` has directory components.
fn find_by_glob(root: &Path, files: &[PathBuf], pattern: &str) -> anyhow::Result<PathBuf> {
    let matches: Vec<&PathBuf> = files
        .iter()
        .filter(|p| {
            p.strip_prefix(root)
                .ok()
                .is_some_and(|rel| glob_match(pattern, &rel.to_string_lossy()))
        })
        .collect();
    match matches.len() {
        0 => {
            let present: Vec<String> = files
                .iter()
                .filter_map(|p| p.strip_prefix(root).ok())
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            let list = format_candidates(present.iter().map(String::as_str));
            bail!(
                "no file matches `{pattern}` in the unpacked archive\nfiles present:{list}\n\
                 fix: edit `bin` for this tool in {CONFIG_PATH_HINT}"
            )
        }
        1 => Ok(matches[0].clone()),
        _ => {
            let paths: Vec<String> = matches
                .iter()
                .filter_map(|p| p.strip_prefix(root).ok())
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            let list = format_candidates(paths.iter().map(String::as_str));
            bail!(
                "`{pattern}` matches multiple files in the unpacked archive:{list}\n\
                 fix: narrow `bin` for this tool in {CONFIG_PATH_HINT}"
            )
        }
    }
}

/// Rewrites a path under `from` to where it will live under `to` — lets
/// collection target the post-rename location (see `collect_bins`).
fn rebase(path: &Path, from: &Path, to: &Path) -> PathBuf {
    match path.strip_prefix(from) {
        Ok(rel) => to.join(rel),
        Err(_) => path.to_path_buf(),
    }
}

/// Runs `<bin> --version` against a freshly extracted, not-yet-live build.
/// `repo::install` calls this only for rig's own entry, before the symlink swap.
pub fn smoke_test_version(
    search_dir: &Path,
    bin_spec: Option<&BinSpec>,
    default_name: &str,
) -> anyhow::Result<()> {
    let (name, path) = resolve_bin_sources(search_dir, bin_spec, default_name)?
        .into_iter()
        .next()
        .context("no bin resolved to smoke-test")?;
    run_version_smoke(&path, &name)
}

/// Shared `--version` gate for freshly built/downloaded bins: a build that
/// can't even print its version must never go live. Used by the repo flow
/// and `rig update --self`'s raw-binary swap.
pub fn run_version_smoke(bin: &Path, label: &str) -> anyhow::Result<()> {
    let status = Command::new(bin)
        .arg("--version")
        .status()
        .with_context(|| format!("failed to run `{label} --version`"))?;
    if !status.success() {
        bail!("`{label} --version` exited with {status} — refusing to make this build live");
    }
    Ok(())
}

/// `collect_bins`'s resolution step, exposed so callers can find where the
/// bin landed without duplicating the `BinSpec` match.
fn resolve_bin_sources(
    search_dir: &Path,
    bin_spec: Option<&BinSpec>,
    default_name: &str,
) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let files = walk_files(search_dir)?;
    match bin_spec {
        None => Ok(vec![(
            default_name.to_string(),
            find_by_name(&files, default_name)?,
        )]),
        Some(BinSpec::Single(name)) => Ok(vec![(name.clone(), find_by_name(&files, name)?)]),
        Some(BinSpec::Multiple(names)) => names
            .iter()
            .map(|name| find_by_name(&files, name).map(|p| (name.clone(), p)))
            .collect(),
        Some(BinSpec::Mapped(map)) => map
            .iter()
            .map(|(pattern, cmd)| {
                find_by_glob(search_dir, &files, pattern).map(|p| (cmd.clone(), p))
            })
            .collect(),
    }
}

/// Resolves `bin` in `search_dir` (pre-rename `.partial`, so failure here
/// leaves the old version untouched) but links point at `link_root`.
pub fn collect_bins(
    search_dir: &Path,
    link_root: &Path,
    bin_spec: Option<&BinSpec>,
    default_name: &str,
    prefix_bin_dir: &Path,
    lock: &Lock,
) -> anyhow::Result<Vec<(PathBuf, PathBuf)>> {
    let resolved = resolve_bin_sources(search_dir, bin_spec, default_name)?;

    fs::create_dir_all(prefix_bin_dir)
        .with_context(|| format!("failed to create {}", prefix_bin_dir.display()))?;

    let mut installed = Vec::new();
    for (cmd, source) in resolved {
        let link_source = rebase(&source, search_dir, link_root);
        let target = prefix_bin_dir.join(&cmd);
        check_conflict(&target, default_name, lock)?;
        symlink_replacing(&link_source, &target)?;
        installed.push((link_source, target));
    }
    Ok(installed)
}

/// Serializes overwrite prompts across pool workers so concurrent
/// installs can't interleave stdin reads.
static CONFIRM_GUARD: Mutex<()> = Mutex::new(());

/// Prompts before deleting `target`. Refuses when stdin isn't a TTY so
/// scripts/CI never hang; callers keep the old error path on refusal.
fn confirm_overwrite(target: &Path) -> anyhow::Result<bool> {
    let _guard = CONFIRM_GUARD.lock().expect("confirm guard poisoned");
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    print!(
        "{} exists and isn't tracked by rig — delete it and let rig take over? [y/N] ",
        target.display()
    );
    std::io::stdout()
        .flush()
        .context("failed to flush overwrite prompt")?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("failed to read confirmation")?;
    Ok(answer_is_yes(&line))
}

fn answer_is_yes(line: &str) -> bool {
    let answer = line.trim();
    answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
}

/// A target that exists but isn't in `rig.lock` belongs to something
/// else — ask before taking it over, refuse rather than clobber silently.
/// Confirmation only grants permission; the delete happens at write time in
/// `symlink_replacing`, so a later step failing never loses the original.
fn check_conflict(target: &Path, tool: &str, lock: &Lock) -> anyhow::Result<()> {
    if !target.exists() {
        return Ok(());
    }
    let target_str = target.to_string_lossy();
    let tracked = |key: &str| {
        lock.tool
            .get(key)
            .is_some_and(|t| t.bins.iter().any(|b| b == target_str.as_ref()))
    };
    // Another rig tool's live bin — taking it over would silently rewire
    // that tool's command, so refuse outright.
    if let Some(owner) = lock.tool.keys().find(|k| *k != tool && tracked(k)) {
        bail!(
            "{} is installed by rig tool `{owner}` — refusing to overwrite another tool's bin",
            target.display()
        );
    }
    if tracked(tool) {
        return Ok(()); // own stale bin: symlink_replacing refreshes it
    }
    if target.is_dir() {
        bail!(
            "{} is a directory and isn't tracked by rig.lock — remove it yourself, rig won't",
            target.display()
        );
    }
    if confirm_overwrite(target)? {
        return Ok(());
    }
    bail!(
        "{} already exists and isn't tracked by rig.lock — refusing to overwrite",
        target.display()
    )
}

fn symlink_replacing(source: &Path, target: &Path) -> anyhow::Result<()> {
    // Anything at `target` was vetted by check_conflict: rig's own stale
    // symlink, or an untracked file the user confirmed. Remove only now,
    // right before the new link lands, so an earlier step failing keeps
    // the original intact. symlink_metadata (not exists) so broken links
    // still count as present.
    if fs::symlink_metadata(target).is_ok() {
        fs::remove_file(target)
            .with_context(|| format!("failed to remove stale {}", target.display()))?;
    }
    std::os::unix::fs::symlink(source, target).with_context(|| {
        format!(
            "failed to symlink {} -> {}",
            target.display(),
            source.display()
        )
    })
}

/// Resolves `completions` in `search_dir`, symlinking into
/// `completions_dir` against `link_root` — same rationale as `collect_bins`.
pub fn collect_completions(
    search_dir: &Path,
    generate_cwd: &Path,
    link_root: &Path,
    spec: &CompletionsSpec,
    completions_dir: &Path,
    env: &HashMap<String, String>,
) -> anyhow::Result<Vec<PathBuf>> {
    match spec {
        CompletionsSpec::Enabled(false) => Ok(Vec::new()),
        CompletionsSpec::Enabled(true) => {
            let files = walk_files(search_dir)?;
            let sources: Vec<PathBuf> = underscore_files(&files)
                .into_iter()
                .map(|p| rebase(&p, search_dir, link_root))
                .collect();
            link_completions(&sources, completions_dir)
        }
        CompletionsSpec::Path { path } => {
            let source = link_root.join(path);
            link_completions(std::slice::from_ref(&source), completions_dir)
        }
        CompletionsSpec::Generate { generate } => {
            run_shell(generate, generate_cwd, env)?;
            let files = walk_files(search_dir)?;
            let sources: Vec<PathBuf> = underscore_files(&files)
                .into_iter()
                .map(|p| rebase(&p, search_dir, link_root))
                .collect();
            link_completions(&sources, completions_dir)
        }
    }
}

fn underscore_files(files: &[PathBuf]) -> Vec<PathBuf> {
    files
        .iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('_'))
        })
        .cloned()
        .collect()
}

fn link_completions(sources: &[PathBuf], completions_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    fs::create_dir_all(completions_dir)
        .with_context(|| format!("failed to create {}", completions_dir.display()))?;

    sources
        .iter()
        .map(|source| {
            let name = source
                .file_name()
                .with_context(|| format!("{} has no file name", source.display()))?;
            let target = completions_dir.join(name);
            symlink_replacing(source, &target)?;
            Ok(target)
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub enum Phase {
    Install,
    Update,
}

/// `setup`: one field covering both install and update — see the `Split`
/// variant for the rare case that genuinely needs to differ.
pub fn run_setup(
    spec: &SetupSpec,
    cwd: &Path,
    phase: Phase,
    env: &HashMap<String, String>,
) -> anyhow::Result<()> {
    match spec {
        SetupSpec::Single(cmd) => run_shell(cmd, cwd, env),
        SetupSpec::Multiple(cmds) => {
            for cmd in cmds {
                run_shell(cmd, cwd, env)?;
            }
            Ok(())
        }
        SetupSpec::Split { install, update } => {
            let cmd = match phase {
                Phase::Install => install,
                Phase::Update => update,
            };
            run_shell(cmd, cwd, env)
        }
    }
}

/// `sudo` as a bare word or a `/sudo`-suffixed path, with shell wrapping
/// (`(`/`$(`, `&&`/`|`/`;`) stripped — catches `command sudo`,
/// `/usr/bin/sudo`, and sudo inside a subshell. False positives on `echo
/// sudo` are acceptable: this is a warning, and misses are the real risk.
fn mentions_sudo(cmd: &str) -> bool {
    cmd.split_whitespace().any(|word| {
        // Only shell-syntax characters get stripped: `/` and `-` survive so
        // `/usr/bin/sudo` and `sudo -n` still match.
        let bare = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '-');
        bare == "sudo" || bare.ends_with("/sudo")
    })
}

/// Hooks that reach outside rig's control (via `sudo`) are allowed, but
/// must not be silent about it.
fn run_shell(cmd: &str, cwd: &Path, env: &HashMap<String, String>) -> anyhow::Result<()> {
    if mentions_sudo(cmd) {
        eprintln!(
            "warning: setup hook runs `sudo` — writes outside rig's control, \
             uninstall will be incomplete: {cmd}"
        );
    }

    let mut command = Command::new("sh");
    command.arg("-c").arg(cmd).current_dir(cwd);
    apply_env(&mut command, env)?;
    let status = command
        .status()
        .with_context(|| format!("failed to run setup command: {cmd}"))?;

    if !status.success() {
        bail!("setup command failed ({status}): {cmd}");
    }
    Ok(())
}

/// Resolves `eval` to (cacheable?, output to embed, probe evidence).
/// Probe/capture failures degrade to "not cacheable" — never fail the install.
pub fn resolve_eval_cache(eval: Option<&EvalSpec>) -> (Option<bool>, Option<String>, Vec<String>) {
    let Some(eval) = eval else {
        return (None, None, Vec::new());
    };

    match eval.cache_override() {
        Some(false) => return (Some(false), None, Vec::new()),
        Some(true) => {
            let output = crate::evalcache::capture(eval.command())
                .inspect_err(|e| {
                    eprintln!("warning: eval capture failed for `{}`: {e}", eval.command())
                })
                .ok();
            return (Some(true), output, Vec::new());
        }
        None => {}
    }

    match crate::evalcache::probe(eval.command()) {
        Ok(probe) => {
            let output = probe.cacheable.then_some(probe.baseline_output);
            (Some(probe.cacheable), output, probe.reasons)
        }
        Err(e) => {
            eprintln!(
                "warning: eval cacheability probe failed for `{}`, falling back to live eval: {e}",
                eval.command()
            );
            (None, None, Vec::new())
        }
    }
}

/// The `bin`/`completions`/`eval` tail shared by `repo.rs`, `git.rs`, and
/// `cargo.rs` once each has staged files into `final_dir`.
pub struct CollectedArtifacts {
    pub bins: Vec<(PathBuf, PathBuf)>,
    pub completions: Vec<PathBuf>,
}

/// Deliberately excludes eval-cache resolution: it must run after
/// `finalize_partial`, once a bin's symlink target actually exists.
pub fn collect_artifacts(
    search_dir: &Path,
    link_root: &Path,
    common: &Common,
    tool: &str,
    prefix_bin_dir: &Path,
    completions_dir: &Path,
    lock: &Lock,
) -> anyhow::Result<CollectedArtifacts> {
    let bins = collect_bins(
        search_dir,
        link_root,
        common.bin.as_ref(),
        tool,
        prefix_bin_dir,
        lock,
    )
    .with_context(|| format!("failed to collect binaries for {tool}"))?;

    // `./tool ...` in a `generate` command must run where the archive
    // actually placed the bin, not always the extraction root.
    let generate_cwd = resolve_bin_sources(search_dir, common.bin.as_ref(), tool)
        .ok()
        .and_then(|resolved| resolved.into_iter().next())
        .and_then(|(_, source)| source.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| search_dir.to_path_buf());
    let completions = collect_completions(
        search_dir,
        &generate_cwd,
        link_root,
        &common.completions,
        completions_dir,
        &common.env,
    )
    .with_context(|| format!("failed to collect completions for {tool}"))?;

    Ok(CollectedArtifacts { bins, completions })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_env_sets_and_expands_tilde_values() {
        let mut map = HashMap::new();
        map.insert("RIG_TEST_PLAIN".to_string(), "hello".to_string());
        map.insert("RIG_TEST_TILDE".to_string(), "~/.local".to_string());

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo $RIG_TEST_PLAIN:$RIG_TEST_TILDE");
        apply_env(&mut cmd, &map).expect("apply_env should succeed");

        let output = cmd.output().expect("sh should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let home = paths::home_dir().expect("$HOME must be set to run this test");
        assert_eq!(stdout.trim(), format!("hello:{}/.local", home.display()));
    }

    fn write_fake_bin(dir: &Path, name: &str, script: &str) {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join(name);
        fs::write(&path, script).expect("write fake bin");
        let mut perms = fs::metadata(&path).expect("stat fake bin").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).expect("chmod +x fake bin");
    }

    #[test]
    fn smoke_test_version_passes_a_working_build() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_fake_bin(tmp.path(), "rig", "#!/bin/sh\nexit 0\n");

        smoke_test_version(tmp.path(), None, "rig").expect("`--version` exiting 0 should pass");
    }

    #[test]
    fn smoke_test_version_rejects_a_broken_build() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_fake_bin(tmp.path(), "rig", "#!/bin/sh\nexit 1\n");

        let err = smoke_test_version(tmp.path(), None, "rig")
            .expect_err("`--version` exiting non-zero must fail the smoke test");
        assert!(err.to_string().contains("--version"));
    }

    #[test]
    fn resolve_bin_name_prefers_the_preferred_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let preferred_dir = tmp.path().join("bin");
        fs::create_dir_all(&preferred_dir).expect("mkdir preferred_dir");
        fs::write(preferred_dir.join("witr"), b"binary").expect("write fake binary");

        let resolved = resolve_bin_name("witr", Some(&preferred_dir), "test")
            .expect("preferred-dir bin should resolve without touching PATH");

        assert_eq!(resolved, preferred_dir.join("witr").display().to_string());
    }

    #[test]
    fn resolve_bin_name_falls_back_to_ambient_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let preferred_dir = tmp.path().join("bin");
        // Never created — nothing landed there, e.g. a script with its own
        // install-location convention (`zb`'s `ZEROBREW_PREFIX`).

        let resolved = resolve_bin_name("sh", Some(&preferred_dir), "test")
            .expect("`sh` should resolve via ambient PATH");

        assert!(resolved.ends_with("/sh"));
    }

    #[test]
    fn extract_false_marks_the_copied_binary_executable() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let archive = tmp.path().join("witr-linux-amd64");
        fs::write(&archive, b"not a real binary").expect("write fake asset");
        // Downloads land with the ordinary, non-executable default mode.
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o644)).expect("chmod source");

        let dest = tmp.path().join("dest");
        extract(&archive, &dest, Some(false)).expect("copy_raw_binary should succeed");

        let copied = dest.join("witr-linux-amd64");
        let mode = fs::metadata(&copied)
            .expect("stat copied file")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "copied raw binary must be executable");
    }

    #[test]
    fn glob_matches_prefix_suffix_and_middle() {
        assert!(glob_match("delta", "delta"));
        assert!(!glob_match("delta", "delta2"));
        assert!(glob_match("nvim-*.appimage", "nvim-0.10.0.appimage"));
        assert!(!glob_match("nvim-*.appimage", "nvim-0.10.0.tar.gz"));
        assert!(glob_match("*-musl", "pueue-x86_64-musl"));
        assert!(glob_match("*.tar.gz", "delta-0.18.2.tar.gz"));
    }

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.to_string(),
            url: format!("https://example.invalid/{name}"),
            size: 0,
            digest: None,
        }
    }

    #[test]
    fn select_asset_prefers_explicit_bpick() {
        let assets = vec![
            asset("delta-x86_64-unknown-linux-gnu.tar.gz"),
            asset("delta-x86_64-unknown-linux-musl.tar.gz"),
        ];
        let bpick = BpickSpec::Single("*-musl.tar.gz".to_string());
        let AssetPick::Found(picked) = select_asset(&assets, Some(&bpick)) else {
            panic!("bpick should match exactly one asset");
        };
        assert!(picked.name.contains("musl"));
    }

    #[test]
    fn select_asset_reports_ambiguous_host_match() {
        let assets = vec![
            asset("delta-x86_64-unknown-linux-gnu.tar.gz"),
            asset("delta-x86_64-unknown-linux-musl.tar.gz"),
        ];
        let AssetPick::Ambiguous(matches) = select_asset(&assets, None) else {
            panic!("gnu and musl should both match x86_64-linux");
        };
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn select_asset_reports_no_match_with_relevant_empty() {
        let assets = vec![asset("delta-aarch64-apple-darwin.tar.gz")];
        let AssetPick::NoMatch {
            bpick_configured,
            relevant,
            all,
        } = select_asset(&assets, None)
        else {
            panic!("darwin build shouldn't match linux host");
        };
        assert!(!bpick_configured);
        assert!(relevant.is_empty());
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn collect_bins_resolves_a_nested_mapped_path() {
        // wallust-style layout: only `target/release/wallust` exists, no
        // top-level `wallust` — a basename-only match can never succeed here.
        let tmp = tempfile::tempdir().expect("tempdir");
        let unpacked = tmp.path().join("unpacked");
        let build_dir = unpacked.join("target/release");
        fs::create_dir_all(&build_dir).expect("mkdir build dir");
        fs::write(build_dir.join("wallust"), b"binary").expect("write fake binary");
        // A same-named decoy elsewhere must not satisfy the path-qualified pattern.
        fs::write(unpacked.join("wallust"), b"decoy").expect("write decoy");

        let mut map = std::collections::HashMap::new();
        map.insert("target/release/wallust".to_string(), "wallust".to_string());
        let bin_spec = BinSpec::Mapped(map);

        let bin_dir = tmp.path().join("bin");
        let lock = Lock::default();
        let installed = collect_bins(
            &unpacked,
            &unpacked,
            Some(&bin_spec),
            "wallust",
            &bin_dir,
            &lock,
        )
        .expect("nested path should resolve");

        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].0, build_dir.join("wallust"));
    }

    #[test]
    fn answer_is_yes_matches_y_and_yes_case_insensitively() {
        for yes in ["y", "Y", "yes", "YES", "Yes", " y \n"] {
            assert!(answer_is_yes(yes), "{yes:?} should confirm");
        }
        for no in ["n", "no", "N", "", "maybe", "yep"] {
            assert!(!answer_is_yes(no), "{no:?} should not confirm");
        }
    }

    #[test]
    fn untracked_bin_conflict_still_refuses_when_stdin_is_not_a_tty() {
        // `cargo test` inherits the terminal's stdin, where the prompt would
        // block forever — only assert the refusal path when stdin is a pipe.
        if std::io::stdin().is_terminal() {
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).expect("mkdir bin dir");
        fs::write(bin_dir.join("zoxide"), b"manual install").expect("write decoy");

        let lock = Lock::default();
        let err = collect_bins(
            tmp.path(),
            tmp.path(),
            Some(&BinSpec::Single("zoxide".to_string())),
            "zoxide",
            &bin_dir,
            &lock,
        )
        .expect_err("untracked conflict must fail");

        assert!(
            err.to_string().contains("refusing to overwrite"),
            "unexpected error: {err}"
        );
        assert!(
            bin_dir.join("zoxide").exists(),
            "decoy must survive a refusal"
        );
    }

    #[test]
    fn refuses_overwriting_a_bin_tracked_by_another_tool() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("bin").join("foo");
        fs::create_dir_all(target.parent().expect("parent")).expect("mkdir bin dir");
        fs::write(&target, b"live bin").expect("write bin");

        let mut lock = Lock::default();
        let mut other = crate::lock::test_tool_lock();
        other.bins = vec![target.display().to_string()];
        lock.tool.insert("other".to_string(), other);

        let err = check_conflict(&target, "me", &lock)
            .expect_err("another tool's tracked bin must be refused");
        assert!(
            err.to_string().contains("another tool's bin"),
            "unexpected error: {err}"
        );
        assert!(target.exists(), "refusal must leave the bin alone");
    }

    #[test]
    fn own_tracked_bin_passes_without_deleting() {
        // Deleting is deferred to symlink_replacing, so check_conflict must
        // return Ok while leaving the (stale) symlink in place.
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("bin").join("foo");
        fs::create_dir_all(target.parent().expect("parent")).expect("mkdir bin dir");
        fs::write(&target, b"stale").expect("write stale bin");

        let mut lock = Lock::default();
        let mut mine = crate::lock::test_tool_lock();
        mine.bins = vec![target.display().to_string()];
        lock.tool.insert("me".to_string(), mine);

        check_conflict(&target, "me", &lock).expect("own stale bin should pass");
        assert!(target.exists(), "check_conflict must not delete");
    }

    #[test]
    fn generate_completions_run_from_where_the_bin_actually_landed() {
        // uv-style layout: the archive wraps everything in a version dir,
        // so `./uv ...` from search_dir's root would find nothing there.
        let tmp = tempfile::tempdir().expect("tempdir");
        let search_dir = tmp.path().join("partial");
        let wrapped = search_dir.join("uv-x86_64-unknown-linux-gnu");
        fs::create_dir_all(&wrapped).expect("mkdir wrapped dir");

        use std::os::unix::fs::PermissionsExt;

        let script = "#!/bin/sh\necho '#compdef uv' > _uv\n";
        let bin_path = wrapped.join("uv");
        fs::write(&bin_path, script).expect("write fake uv");
        let mut perms = fs::metadata(&bin_path).expect("stat").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin_path, perms).expect("chmod +x");

        let common = Common {
            bin: Some(BinSpec::Single("uv".to_string())),
            completions: CompletionsSpec::Generate {
                generate: "./uv --dummy".to_string(),
            },
            ..crate::config::test_common()
        };
        let lock = Lock::default();
        let artifacts = collect_artifacts(
            &search_dir,
            &search_dir,
            &common,
            "uv",
            &tmp.path().join("bin"),
            &tmp.path().join("completions"),
            &lock,
        )
        .expect("generate command should find ./uv in its own directory");

        assert_eq!(artifacts.completions.len(), 1);
        let generated =
            fs::read_to_string(wrapped.join("_uv")).expect("_uv should exist next to the bin");
        assert_eq!(generated.trim(), "#compdef uv");
    }

    #[test]
    fn a_failed_collect_never_destroys_the_old_version() {
        // finalize_partial must never run before collect_artifacts succeeds
        // — otherwise a collect failure leaves neither version intact.
        let tmp = tempfile::tempdir().expect("tempdir");

        let final_dir = tmp.path().join("pkg/delta/0.18.2");
        fs::create_dir_all(&final_dir).expect("mkdir final_dir");
        fs::write(final_dir.join("delta"), b"old binary").expect("write old binary");

        let prefix_bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&prefix_bin_dir).expect("mkdir prefix_bin_dir");
        let bin_link = prefix_bin_dir.join("delta");
        std::os::unix::fs::symlink(final_dir.join("delta"), &bin_link).expect("symlink old binary");

        let mut lock = Lock::default();
        lock.tool.insert(
            "delta".to_string(),
            crate::lock::ToolLock {
                bins: vec![bin_link.display().to_string()],
                ..crate::lock::test_tool_lock()
            },
        );

        // New version's `.partial` is missing the expected binary entirely.
        let partial_dir = tmp.path().join("pkg/delta/0.19.0.partial");
        fs::create_dir_all(&partial_dir).expect("mkdir partial_dir");
        let new_final_dir = tmp.path().join("pkg/delta/0.19.0");

        let common = Common {
            completions: CompletionsSpec::Enabled(false),
            ..crate::config::test_common()
        };
        let result = collect_artifacts(
            &partial_dir,
            &new_final_dir,
            &common,
            "delta",
            &prefix_bin_dir,
            &tmp.path().join("completions"),
            &lock,
        )
        .and_then(|_| finalize_partial(&partial_dir, &new_final_dir));

        assert!(
            result.is_err(),
            "collect should fail: no `delta` in partial_dir"
        );
        assert!(final_dir.exists(), "old version dir must survive");
        assert_eq!(
            fs::read(final_dir.join("delta")).expect("old binary should be readable"),
            b"old binary"
        );
        assert!(bin_link.is_symlink(), "old symlink must be untouched");
        assert!(
            !new_final_dir.exists(),
            "finalize_partial must never have run"
        );
    }

    /// Serves `body` exactly once over HTTP on a loopback port, returning
    /// the URL — enough surface for `download`'s size/digest checks.
    fn serve_once(body: Vec<u8>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("test server addr");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept test client");
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).expect("write head");
            stream.write_all(&body).expect("write body");
        });
        format!("http://{addr}/asset")
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        to_hex(&Sha256::digest(bytes))
    }

    #[test]
    fn verify_digest_matches_and_degrades() {
        let actual = Sha256::digest(b"payload");
        let hex = to_hex(&actual);
        verify_digest(Some(&format!("sha256:{hex}")), &actual).expect("matching hex passes");

        let err = verify_digest(Some(&format!("sha256:{}", "0".repeat(64))), &actual)
            .expect_err("mismatching hex must fail");
        assert!(err.to_string().contains("expected"), "unexpected: {err}");

        verify_digest(None, &actual).expect("missing digest passes");
        verify_digest(Some("md5:beef"), &actual).expect("unknown algorithm degrades");
    }

    #[test]
    fn download_accepts_a_matching_digest() {
        let body = b"rig digest test payload".to_vec();
        let url = serve_once(body.clone());
        let tmp = tempfile::tempdir().expect("tempdir");
        let digest = format!("sha256:{}", sha256_hex(&body));
        download(
            &url,
            &tmp.path().join("asset"),
            body.len() as u64,
            Some(&digest),
        )
        .expect("matching digest must pass");
    }

    #[test]
    fn download_rejects_a_mismatched_digest() {
        let body = b"rig digest test payload".to_vec();
        let url = serve_once(body.clone());
        let tmp = tempfile::tempdir().expect("tempdir");
        let digest = format!("sha256:{}", "0".repeat(64));
        let err = download(
            &url,
            &tmp.path().join("asset"),
            body.len() as u64,
            Some(&digest),
        )
        .expect_err("wrong digest must fail");
        assert!(err.to_string().contains("digest"), "unexpected: {err}");
    }

    #[test]
    fn download_without_digest_only_checks_size() {
        let body = b"rig digest test payload".to_vec();
        let url = serve_once(body.clone());
        let tmp = tempfile::tempdir().expect("tempdir");
        download(&url, &tmp.path().join("asset"), body.len() as u64, None)
            .expect("no digest must pass");
    }

    #[test]
    fn download_degrades_on_unknown_digest_algorithm() {
        let body = b"rig digest test payload".to_vec();
        let url = serve_once(body.clone());
        let tmp = tempfile::tempdir().expect("tempdir");
        download(
            &url,
            &tmp.path().join("asset"),
            body.len() as u64,
            Some("md5:beef"),
        )
        .expect("unknown algorithm degrades to size-only");
    }

    #[test]
    fn download_still_fails_on_size_mismatch() {
        let body = b"rig digest test payload".to_vec();
        let url = serve_once(body.clone());
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = download(&url, &tmp.path().join("asset"), body.len() as u64 + 1, None)
            .expect_err("size mismatch must fail");
        assert!(err.to_string().contains("expected"), "unexpected: {err}");
    }

    #[test]
    fn mentions_sudo_catches_paths_command_and_subshells() {
        assert!(mentions_sudo("sudo apt install x"));
        assert!(mentions_sudo("command sudo apt install x"));
        assert!(mentions_sudo("/usr/bin/sudo apt install x"));
        assert!(mentions_sudo("(sudo apt install x)"));
        assert!(mentions_sudo("$(sudo apt install x)"));
        assert!(mentions_sudo("echo hi && sudo make install"));
        assert!(mentions_sudo("sudo; make install"));
        assert!(mentions_sudo("make install && /usr/bin/sudo make install"));
        assert!(!mentions_sudo("make install"));
        assert!(!mentions_sudo("ls -la"));
    }
}
