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
    /// Max concurrent tools in `rig install`; `1` = strictly serial.
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
    /// Plain shell text (e.g. `alias js="just"`) run verbatim, never
    /// captured like `eval`. `lazy` below defers it the same way.
    pub run: Option<RunSpec>,
    #[serde(default = "default_completions")]
    pub completions: CompletionsSpec,
    pub setup: Option<SetupSpec>,
    /// Defers `env`/`eval`/`run` to first use of the tool's own command
    /// name — wrong for tools whose hooks must observe events from start.
    #[serde(default)]
    pub lazy: bool,
    /// Only meaningful with `lazy`: `"key:widget"` — placeholder ZLE widget
    /// that runs the deferred setup, then rebinds `key` to the real one.
    pub bind: Option<String>,
}

/// Every `[[source]]` type's tests build a `Common` with mostly-default
/// fields — shared here so a new field only needs updating once ([TEST-1]).
#[cfg(test)]
pub fn test_common() -> Common {
    Common {
        description: None,
        bin: None,
        env: HashMap::new(),
        eval: None,
        run: None,
        completions: default_completions(),
        setup: None,
        lazy: false,
        bind: None,
    }
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

/// One line, or several — plain shell text for `run`, taken verbatim.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum RunSpec {
    Single(String),
    Multiple(Vec<String>),
}

impl RunSpec {
    /// `Single` may be a `'''...'''` block of several physical lines —
    /// split and drop blanks so callers get one statement per entry.
    pub fn lines(&self) -> Vec<&str> {
        match self {
            RunSpec::Single(text) => text.lines().filter(|l| !l.trim().is_empty()).collect(),
            RunSpec::Multiple(lines) => lines.iter().map(String::as_str).collect(),
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
/// No `Common` here — `bin`/`eval`/`setup`/`lazy`/`bind` describe an
/// installed *command*, and a plugin is never invoked by name. `run` is
/// the one shared field: same shape as `Common::run`, but rendered right
/// after `source` (plugins are always eager — no `lazy`/`bind`).
#[derive(Debug, Deserialize)]
pub struct PluginEntry {
    pub name: String,
    pub description: Option<String>,
    pub source: Option<String>,
    /// Plain shell text run right after the plugin is sourced — e.g. a
    /// function the plugin just defined.
    pub run: Option<RunSpec>,
}

/// The default command name doubles as the tool's short identity
/// (lockfile key, pkg dir name) — the last path segment of `name`.
pub fn tool_key(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// One entry from any of the seven `[[source]]` vectors, for code that needs
/// to work across all of them (CLI dispatch, `rig list`, init.zsh gen).
pub enum ResolvedEntry<'a> {
    Repo(&'a RepoEntry),
    Git(&'a GitEntry),
    Cargo(&'a CargoEntry),
    Node(&'a NodeEntry),
    Apt(&'a AptEntry),
    Curl(&'a CurlEntry),
    Plugin(&'a PluginEntry),
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
            Self::Plugin(e) => &e.name,
        }
    }

    /// `None` for `Plugin` — it has no `Common`, and a missing `bin` there
    /// doesn't mean "named after the tool" the way it does for the other six.
    pub fn common(&self) -> Option<&'a Common> {
        match self {
            Self::Repo(e) => Some(&e.common),
            Self::Git(e) => Some(&e.common),
            Self::Cargo(e) => Some(&e.common),
            Self::Node(e) => Some(&e.common),
            Self::Apt(e) => Some(&e.common),
            Self::Curl(e) => Some(&e.common),
            Self::Plugin(_) => None,
        }
    }

    pub fn description(&self) -> Option<&'a str> {
        match self {
            Self::Plugin(e) => e.description.as_deref(),
            other => other.common().and_then(|c| c.description.as_deref()),
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
    entries.extend(config.plugin.iter().map(ResolvedEntry::Plugin));
    entries
}

/// Matches key, full name, or a declared command name (`rg` for
/// `BurntSushi/ripgrep`) — same identity `rig which` already resolves by.
pub fn resolve_tool<'a>(config: &'a Config, requested: &str) -> Option<ResolvedEntry<'a>> {
    all_entries(config).into_iter().find(|e| {
        e.key() == requested
            || e.name() == requested
            || e.common().is_some_and(|c| {
                BinSpec::declared_names(c.bin.as_ref(), tool_key(e.name()))
                    .iter()
                    .any(|n| n == requested)
            })
    })
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
run = "_zsh_autosuggest_start"
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

        assert!(
            matches!(&cfg.plugin[0].run, Some(RunSpec::Single(s)) if s == "_zsh_autosuggest_start")
        );
        assert_eq!(
            cfg.plugin[0].source.as_deref(),
            Some("zsh-autosuggestions.zsh")
        );
    }

    #[test]
    fn resolve_tool_matches_key_name_or_declared_bin() {
        let cfg = parse(
            r#"
[[repo]]
name = "BurntSushi/ripgrep"
bin = "rg"
"#,
        )
        .expect("sample config should parse");

        assert_eq!(resolve_tool(&cfg, "ripgrep").unwrap().key(), "ripgrep");
        assert_eq!(
            resolve_tool(&cfg, "BurntSushi/ripgrep").unwrap().key(),
            "ripgrep"
        );
        assert_eq!(resolve_tool(&cfg, "rg").unwrap().key(), "ripgrep");
        assert!(resolve_tool(&cfg, "not-configured").is_none());
    }

    #[test]
    fn settings_default_when_omitted() {
        let cfg = parse("").expect("empty config should parse with defaults");
        assert_eq!(cfg.settings.prefix, "~/.local");
        assert_eq!(cfg.settings.parallel, 8);
    }

    #[test]
    fn run_field_parses_single_and_multiple_forms() {
        let cfg = parse(
            r#"
[[curl]]
name = "casey/just"
url = "https://just.systems/install.sh"
run = 'alias js="just"'

[[curl]]
name = "foo/bar"
url = "https://example.com/install.sh"
run = ["alias a=\"b\"", "alias c=\"d\""]
"#,
        )
        .expect("should parse both run shapes");

        assert_eq!(
            cfg.curl[0].common.run.as_ref().unwrap().lines(),
            vec![r#"alias js="just""#]
        );
        assert_eq!(
            cfg.curl[1].common.run.as_ref().unwrap().lines(),
            vec![r#"alias a="b""#, r#"alias c="d""#]
        );
    }

    #[test]
    fn run_field_parses_a_triple_quoted_block_into_separate_lines() {
        let cfg = parse(
            "
[[curl]]
name = \"casey/just\"
url = \"https://just.systems/install.sh\"
run = '''
alias a=\"b\"

alias c=\"d\"
'''
",
        )
        .expect("should parse a triple-quoted run block");

        assert_eq!(
            cfg.curl[0].common.run.as_ref().unwrap().lines(),
            vec![r#"alias a="b""#, r#"alias c="d""#]
        );
    }

    #[test]
    fn run_field_accepts_all_three_empty_shapes() {
        let cfg = parse(
            r#"
[[curl]]
name = "a/a"
url = "https://example.com"
run = ""

[[curl]]
name = "b/b"
url = "https://example.com"
run = []

[[curl]]
name = "c/c"
url = "https://example.com"
run = ''''''
"#,
        )
        .expect("all three empty run shapes should parse");

        for entry in &cfg.curl {
            assert_eq!(
                entry.common.run.as_ref().unwrap().lines(),
                Vec::<&str>::new()
            );
        }
    }

    #[test]
    fn run_field_parses_a_basic_multiline_block_with_escapes() {
        let cfg = parse(
            "
[[curl]]
name = \"casey/just\"
url = \"https://just.systems/install.sh\"
run = \"\"\"
alias js=\\\"just\\\"
alias jl=\\\"just --list\\\"
\"\"\"
",
        )
        .expect("should parse a basic (double-quoted) multiline run block");

        assert_eq!(
            cfg.curl[0].common.run.as_ref().unwrap().lines(),
            vec![r#"alias js="just""#, r#"alias jl="just --list""#]
        );
    }
}
