//! `config.toml` schema, including `ResolvedEntry` for code that needs to
//! work across all six `[[source]]` vectors uniformly.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub repo: Vec<RepoEntry>,
    #[serde(default)]
    pub git: Vec<GitEntry>,
    #[serde(default)]
    pub cargo: Vec<CargoEntry>,
    #[serde(default)]
    pub node: Vec<NodeEntry>,
    #[serde(default)]
    pub apt: Vec<AptEntry>,
    #[serde(default)]
    pub curl: Vec<CurlEntry>,
    #[serde(default)]
    pub plugin: Vec<PluginEntry>,
}

/// Struct-level default so an absent `[settings]` table still yields
/// the documented defaults (prefix `~/.local`, parallel 8).
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub prefix: String,
    pub parallel: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            prefix: "~/.local".to_string(),
            parallel: 8,
        }
    }
}

/// Fields shared by `[[repo]]`, `[[git]]`, `[[cargo]]`, `[[node]]`,
/// `[[apt]]`, `[[curl]]`. `[[plugin]]` is Job B and does not share this shape.
#[derive(Debug, Deserialize)]
pub struct Common {
    pub description: Option<String>,
    pub bin: Option<BinSpec>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub eval: Option<EvalSpec>,
    #[serde(default = "default_completions")]
    pub completions: CompletionsSpec,
    pub setup: Option<SetupSpec>,
    /// Defers `env`/`eval` to first use of the tool's own command name —
    /// wrong for tools whose hooks must observe events from session start.
    #[serde(default)]
    pub lazy: bool,
    /// Only meaningful with `lazy`: `"key:widget"` — placeholder ZLE widget
    /// that runs the deferred setup, then rebinds `key` to the real one.
    pub bind: Option<String>,
}

/// Three TOML shapes for one field, resolved via untagged.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum BinSpec {
    Single(String),
    Multiple(Vec<String>),
    Mapped(HashMap<String, String>),
}

impl BinSpec {
    /// Command names as declared in config, not as found on disk —
    /// `RIG_CMD` must cover tools that aren't installed yet.
    pub fn declared_names(bin: Option<&BinSpec>, default: &str) -> Vec<String> {
        match bin {
            None => vec![default.to_string()],
            Some(BinSpec::Single(name)) => vec![name.clone()],
            Some(BinSpec::Multiple(names)) => names.clone(),
            Some(BinSpec::Mapped(map)) => map.values().cloned().collect(),
        }
    }
}

/// Default is `Enabled(true)`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum CompletionsSpec {
    Enabled(bool),
    Generate { generate: String },
    Path { path: String },
}

fn default_completions() -> CompletionsSpec {
    CompletionsSpec::Enabled(true)
}

/// Plain command, or `{ cmd, cache }` to override rig's auto probe.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum EvalSpec {
    Cmd(String),
    Detailed {
        cmd: String,
        /// `None` = let rig's cacheability probe decide.
        #[serde(default)]
        cache: Option<bool>,
    },
}

impl EvalSpec {
    pub fn command(&self) -> &str {
        match self {
            EvalSpec::Cmd(cmd) => cmd,
            EvalSpec::Detailed { cmd, .. } => cmd,
        }
    }

    pub fn cache_override(&self) -> Option<bool> {
        match self {
            EvalSpec::Cmd(_) => None,
            EvalSpec::Detailed { cache, .. } => *cache,
        }
    }
}

/// Single command, multiple steps, or install/update split.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SetupSpec {
    Single(String),
    Multiple(Vec<String>),
    Split { install: String, update: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Host {
    Github,
    Codeberg,
}

impl Host {
    /// The `source` field prefix in `rig.lock` (e.g. `"github:owner/repo"`).
    pub fn prefix(self) -> &'static str {
        match self {
            Host::Github => "github",
            Host::Codeberg => "codeberg",
        }
    }
}

fn default_host() -> Host {
    Host::Github
}

/// Asset-selection glob(s), no map form (unlike `bin`).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum BpickSpec {
    Single(String),
    Multiple(Vec<String>),
}

/// `[[repo]]`: GitHub/Codeberg release binaries.
#[derive(Debug, Deserialize)]
pub struct RepoEntry {
    pub name: String,
    #[serde(default = "default_host")]
    pub host: Host,
    pub bpick: Option<BpickSpec>,
    /// `None` = auto-detect; `Some(false)` = raw binary, no unpacking.
    pub extract: Option<bool>,
    #[serde(flatten)]
    pub common: Common,
}

/// `[[git]]`: source build. Hook only builds; rig places `bin`.
#[derive(Debug, Deserialize)]
pub struct GitEntry {
    pub name: String,
    #[serde(default = "default_host")]
    pub host: Host,
    #[serde(flatten)]
    pub common: Common,
}

/// `[[cargo]]`: `cargo install`, version resolved via crates.io.
#[derive(Debug, Deserialize)]
pub struct CargoEntry {
    pub name: String,
    #[serde(flatten)]
    pub common: Common,
}

/// `[[node]]`: bun-first, npm-fallback.
#[derive(Debug, Deserialize)]
pub struct NodeEntry {
    pub name: String,
    #[serde(default)]
    pub manager: NodeManager,
    #[serde(flatten)]
    pub common: Common,
}

/// `Auto` probes bun then npm; explicit choices never fall back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeManager {
    #[default]
    Auto,
    Bun,
    Npm,
}

/// `[[apt]]`: system package, left to `apt upgrade` for updates.
#[derive(Debug, Deserialize)]
pub struct AptEntry {
    pub name: String,
    #[serde(flatten)]
    pub common: Common,
}

/// `[[curl]]`: install script fetched from `url` and run.
#[derive(Debug, Deserialize)]
pub struct CurlEntry {
    pub name: String,
    pub url: String,
    #[serde(flatten)]
    pub common: Common,
}

/// `[[plugin]]`: Job B, zsh loads it. Rig only clones/updates.
#[derive(Debug, Deserialize)]
pub struct PluginEntry {
    pub name: String,
    pub source: Option<String>,
    /// Parsed but not acted on — a defer mechanism wasn't worth building,
    /// so `initzsh.rs` sources plugins in config order instead.
    #[serde(default)]
    #[allow(dead_code)]
    pub defer: u32,
    pub atload: Option<String>,
}

/// The default command name doubles as the tool's short identity
/// (lockfile key, pkg dir name) — the last path segment of `name`.
pub fn tool_key(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// One entry from any of the six `[[source]]` vectors, for code that needs
/// to work across all of them (CLI dispatch, `rig list`, init.zsh gen).
pub enum ResolvedEntry<'a> {
    Repo(&'a RepoEntry),
    Git(&'a GitEntry),
    Cargo(&'a CargoEntry),
    Node(&'a NodeEntry),
    Apt(&'a AptEntry),
    Curl(&'a CurlEntry),
}

impl<'a> ResolvedEntry<'a> {
    pub fn name(&self) -> &'a str {
        match self {
            Self::Repo(e) => &e.name,
            Self::Git(e) => &e.name,
            Self::Cargo(e) => &e.name,
            Self::Node(e) => &e.name,
            Self::Apt(e) => &e.name,
            Self::Curl(e) => &e.name,
        }
    }

    pub fn common(&self) -> &'a Common {
        match self {
            Self::Repo(e) => &e.common,
            Self::Git(e) => &e.common,
            Self::Cargo(e) => &e.common,
            Self::Node(e) => &e.common,
            Self::Apt(e) => &e.common,
            Self::Curl(e) => &e.common,
        }
    }

    /// `[[node]]` keys on the full (possibly scoped) package name; every
    /// other source type keys on the short `tool_key`.
    pub fn key(&self) -> String {
        match self {
            Self::Node(e) => e.name.clone(),
            other => tool_key(other.name()).to_string(),
        }
    }
}

pub fn all_entries(config: &Config) -> Vec<ResolvedEntry<'_>> {
    let mut entries = Vec::new();
    entries.extend(config.repo.iter().map(ResolvedEntry::Repo));
    entries.extend(config.git.iter().map(ResolvedEntry::Git));
    entries.extend(config.cargo.iter().map(ResolvedEntry::Cargo));
    entries.extend(config.node.iter().map(ResolvedEntry::Node));
    entries.extend(config.apt.iter().map(ResolvedEntry::Apt));
    entries.extend(config.curl.iter().map(ResolvedEntry::Curl));
    entries
}

/// Matches either form so `rig install delta` and `rig install
/// dandavison/delta` both work.
pub fn resolve_tool<'a>(config: &'a Config, requested: &str) -> Option<ResolvedEntry<'a>> {
    all_entries(config)
        .into_iter()
        .find(|e| e.key() == requested || e.name() == requested)
}

pub fn parse(input: &str) -> anyhow::Result<Config> {
    toml::from_str(input).context("failed to parse config.toml")
}

pub fn load(path: &Path) -> anyhow::Result<Config> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    parse(&text).with_context(|| format!("failed to parse {}", path.display()))
}

/// `toml::from_str` discards source spans, so this re-reads as text and
/// marks `key`'s line git-diff style: `-` old value, `+` `replacement`.
pub fn render_entry_diff(
    config_path: &Path,
    name: &str,
    key: &str,
    replacement: Option<&str>,
) -> Option<String> {
    let text = fs::read_to_string(config_path).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    let name_line = format!("name = \"{name}\"");

    let name_idx = lines.iter().position(|l| l.trim() == name_line)?;
    let start = (0..name_idx)
        .rev()
        .find(|&i| lines[i].trim_start().starts_with("[["))?;
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with("[["))
        .map_or(lines.len(), |offset| start + 1 + offset);

    let block: Vec<&str> = lines[start..end]
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    let key_prefix = format!("{key} =");
    let key_exists = block.iter().any(|l| l.starts_with(&key_prefix));

    let mut out = Vec::new();
    for &line in &block {
        if line.starts_with(&key_prefix) {
            out.push(format!("- {line}"));
            if let Some(value) = replacement {
                out.push(format!("+ {key} = \"{value}\""));
            }
        } else {
            out.push(format!("| {line}"));
            if !key_exists
                && line == name_line
                && let Some(value) = replacement
            {
                out.push(format!("+ {key} = \"{value}\""));
            }
        }
    }
    Some(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[settings]
prefix = "~/.local"
parallel = 4

[[repo]]
name = "dandavison/delta"
bpick = ["*.tar.gz"]
bin = { "delta" = "delta" }
completions = true

[[git]]
name = "explosion-mental/wallust"
host = "codeberg"
setup = "cargo +nightly build --release"
bin = { "target/release/wallust" = "wallust" }

[[node]]
name = "@openai/codex"
manager = "bun"

[[plugin]]
name = "zsh-users/zsh-autosuggestions"
source = "zsh-autosuggestions.zsh"
defer = 1
atload = "_zsh_autosuggest_start"
"#;

    #[test]
    fn parses_full_example() {
        let cfg = parse(SAMPLE).expect("sample config should parse");

        assert_eq!(cfg.settings.parallel, 4);

        assert_eq!(cfg.repo.len(), 1);
        assert_eq!(cfg.repo[0].host, Host::Github); // defaulted, not in TOML
        assert!(matches!(
            cfg.repo[0].common.completions,
            CompletionsSpec::Enabled(true)
        ));
        assert!(
            matches!(&cfg.repo[0].common.bin, Some(BinSpec::Mapped(m)) if m.get("delta") == Some(&"delta".to_string()))
        );

        assert_eq!(cfg.git[0].host, Host::Codeberg);
        assert!(
            matches!(&cfg.git[0].common.setup, Some(SetupSpec::Single(s)) if s.starts_with("cargo"))
        );

        assert_eq!(cfg.node[0].manager, NodeManager::Bun);

        assert_eq!(cfg.plugin[0].defer, 1);
        assert_eq!(
            cfg.plugin[0].source.as_deref(),
            Some("zsh-autosuggestions.zsh")
        );
    }

    #[test]
    fn settings_default_when_omitted() {
        let cfg = parse("").expect("empty config should parse with defaults");
        assert_eq!(cfg.settings.prefix, "~/.local");
        assert_eq!(cfg.settings.parallel, 8);
    }
}
