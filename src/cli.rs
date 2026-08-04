//! Subcommand dispatch: install/update/remove/list/sync/doctor/which.
//! `main.rs` is a one-line wrapper around `run`.
use std::fs;
use std::process::ExitCode;

use anyhow::{Context as _, anyhow, bail};
use clap::{Parser, Subcommand};

use crate::config::{self, Config, ResolvedEntry};
use crate::doctor;
use crate::initzsh;
use crate::lock::{self, Lock};
use crate::paths::{self, Layout};
use crate::remove;
use crate::sources;
use crate::update;

#[derive(Parser)]
#[command(name = "rig", version, about = "Tool installer + zsh loader")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install one or more configured tools
    Install { tools: Vec<String> },
    /// Update tools (omit to update everything in config.toml)
    Update {
        tools: Vec<String>,
        #[arg(long)]
        force: bool,
    },
    /// Remove an installed tool's rig-owned files
    Remove { tools: Vec<String> },
    /// List configured tools and whether they're installed
    List,
    /// Regenerate init.zsh from the current config
    Sync,
    /// Run diagnostic checks
    Doctor,
    /// Resolve a command name to the tool that provides it
    Which { command: String },
}

pub fn run() -> ExitCode {
    match dispatch(Cli::parse().command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:?}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Install { tools } => cmd_install(tools),
        Command::Update { tools, force } => cmd_update(tools, force),
        Command::Remove { tools } => cmd_remove(tools),
        Command::List => cmd_list(),
        Command::Sync => cmd_sync(),
        Command::Doctor => cmd_doctor(),
        Command::Which { command } => cmd_which(command),
    }
}

struct Ctx {
    config: Config,
    lock: Lock,
    layout: Layout,
}

impl Ctx {
    fn load() -> anyhow::Result<Self> {
        let home = paths::home_dir()?;
        let config_path = home.join(".config/rig/config.toml");
        let config = config::load(&config_path)
            .with_context(|| format!("failed to load {}", config_path.display()))?;
        let layout = Layout::new(&home, &config.settings.prefix);
        let lock = lock::load_or_default(&layout.lock_path)?;
        Ok(Self {
            config,
            lock,
            layout,
        })
    }

    fn save(&self) -> anyhow::Result<()> {
        lock::save_and_sync(&self.config, &self.lock, &self.layout)
    }
}

fn cmd_install(tools: Vec<String>) -> anyhow::Result<()> {
    if tools.is_empty() {
        bail!("specify at least one tool to install");
    }
    let mut ctx = Ctx::load()?;

    // Save after every tool, not once at batch end — a killed process must
    // not lose lock state for tools that already finished installing.
    let mut failed = false;
    for name in &tools {
        match install_one(&mut ctx, name).and_then(|()| ctx.save()) {
            Ok(()) => {}
            Err(e) => {
                failed = true;
                eprintln!("{name}: error: {e:?}");
            }
        }
    }

    if failed {
        bail!("one or more tools failed to install");
    }
    Ok(())
}

fn install_one(ctx: &mut Ctx, requested: &str) -> anyhow::Result<()> {
    let resolved = config::resolve_tool(&ctx.config, requested)
        .ok_or_else(|| anyhow!("{requested} is not in config.toml"))?;
    let key = resolved.key();

    // Skip only if genuinely healthy — a dangling bin must still fall
    // through, since `command_not_found_handler` relies on this to self-heal.
    if let Some(existing) = ctx.lock.tool.get(&key) {
        let healthy = !existing.bins.is_empty()
            && existing
                .bins
                .iter()
                .all(|b| std::path::Path::new(b).exists());
        if healthy {
            println!(
                "{key} is already installed ({}) — use `rig update {key}` to check \
                 for a newer version, or `rig remove {key}` first to force a clean reinstall",
                existing.version
            );
            return Ok(());
        }
    }

    let new_lock = match resolved {
        ResolvedEntry::Repo(e) => {
            sources::repo::install(e, &ctx.layout, &ctx.lock, sources::Phase::Install)?
        }
        ResolvedEntry::Git(e) => {
            sources::git::install(e, &ctx.layout, &ctx.lock, sources::Phase::Install)?
        }
        ResolvedEntry::Cargo(e) => {
            sources::cargo::install(e, &ctx.layout, &ctx.lock, sources::Phase::Install)?
        }
        ResolvedEntry::Node(e) => {
            sources::node::install(e, &ctx.layout, &ctx.lock, sources::Phase::Install)?
        }
        ResolvedEntry::Apt(e) => sources::apt::install(e, &ctx.layout, sources::Phase::Install)?,
        ResolvedEntry::Curl(e) => sources::curl::install(e, &ctx.layout, sources::Phase::Install)?,
    };
    println!("installed {key} {}", new_lock.version);
    ctx.lock.tool.insert(key, new_lock);
    Ok(())
}

fn cmd_update(tools: Vec<String>, force: bool) -> anyhow::Result<()> {
    let mut ctx = Ctx::load()?;
    let filter = (!tools.is_empty()).then_some(tools.as_slice());
    let reports = update::update_all(&ctx.config, &mut ctx.lock, &ctx.layout, force, filter);

    let mut failed = false;
    for report in &reports {
        match &report.outcome {
            Ok(outcome) => println!("{}: {}", report.tool, describe_outcome(outcome)),
            Err(e) => {
                failed = true;
                eprintln!("{}: error: {e:?}", report.tool);
            }
        }
    }

    for plugin in &ctx.config.plugin {
        let dest = ctx.layout.plugins_dir.join(config::tool_key(&plugin.name));
        if !dest.exists() {
            continue; // not cloned yet — `rig sync` handles that, not update
        }
        match sources::plugin::update(plugin, &ctx.layout) {
            Ok(()) => println!("{}: pulled", plugin.name),
            Err(e) => {
                failed = true;
                eprintln!("{}: error: {e:?}", plugin.name);
            }
        }
    }

    if failed {
        bail!("one or more tools failed to update");
    }
    Ok(())
}

fn describe_outcome(outcome: &update::Outcome) -> String {
    match outcome {
        update::Outcome::UpToDate { version } => format!("up to date ({version})"),
        update::Outcome::Updated { from, to } => format!("updated {from} -> {to}"),
        update::Outcome::Reran => "rerun".to_string(),
    }
}

fn cmd_remove(tools: Vec<String>) -> anyhow::Result<()> {
    if tools.is_empty() {
        bail!("specify at least one tool to remove");
    }
    let mut ctx = Ctx::load()?;

    let mut failed = false;
    for name in &tools {
        match remove_one(&mut ctx, name).and_then(|()| ctx.save()) {
            Ok(()) => {}
            Err(e) => {
                failed = true;
                eprintln!("{name}: error: {e:?}");
            }
        }
    }

    if failed {
        bail!("one or more tools failed to remove");
    }
    Ok(())
}

fn remove_one(ctx: &mut Ctx, requested: &str) -> anyhow::Result<()> {
    let key = config::resolve_tool(&ctx.config, requested)
        .map(|r| r.key())
        .unwrap_or_else(|| requested.to_string());
    let tool_lock = ctx
        .lock
        .tool
        .get(&key)
        .ok_or_else(|| anyhow!("{key} is not installed"))?;
    remove::remove(tool_lock, &ctx.layout)?;
    ctx.lock.tool.remove(&key);
    println!("removed {key}");
    Ok(())
}

fn cmd_list() -> anyhow::Result<()> {
    let ctx = Ctx::load()?;
    for entry in config::all_entries(&ctx.config) {
        let key = entry.key();
        let marker = if ctx.lock.tool.contains_key(&key) {
            "\u{25cf}"
        } else {
            "\u{25cb}"
        };
        match &entry.common().description {
            Some(desc) => println!("{marker} {key}  {desc}"),
            None => println!("{marker} {key}"),
        }
    }
    Ok(())
}

/// Plugins aren't installed on demand like binaries — `sync` clones any
/// that are missing so their `source` line in init.zsh resolves.
fn cmd_sync() -> anyhow::Result<()> {
    let ctx = Ctx::load()?;

    let old_keys = fs::read_to_string(&ctx.layout.init_zsh_path)
        .map(|content| initzsh::declared_tool_keys(&content))
        .unwrap_or_default();

    let mut failed = false;
    for plugin in &ctx.config.plugin {
        let dest = ctx.layout.plugins_dir.join(config::tool_key(&plugin.name));
        if dest.exists() {
            continue;
        }
        match sources::plugin::install(plugin, &ctx.layout) {
            Ok(()) => println!("cloned plugin {}", plugin.name),
            Err(e) => {
                failed = true;
                eprintln!("{}: error: {e:?}", plugin.name);
            }
        }
    }

    ctx.save()?;
    report_tool_set_diff(&old_keys, &ctx.config);
    if failed {
        bail!("one or more plugins failed to clone");
    }
    Ok(())
}

/// `rig sync` can fire unattended on shell startup — this is the only
/// feedback that a manual `config.toml` edit actually took effect.
fn report_tool_set_diff(old_keys: &std::collections::HashSet<String>, config: &Config) {
    let new_keys: std::collections::HashSet<String> = config::all_entries(config)
        .into_iter()
        .map(|e| e.key())
        .collect();

    let mut added: Vec<&String> = new_keys.difference(old_keys).collect();
    let mut removed: Vec<&String> = old_keys.difference(&new_keys).collect();
    added.sort();
    removed.sort();

    if !added.is_empty() {
        println!(
            "+ added: {}",
            added
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !removed.is_empty() {
        println!(
            "- removed: {}",
            removed
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

fn cmd_doctor() -> anyhow::Result<()> {
    let ctx = Ctx::load()?;
    let findings = doctor::run(&ctx.config, &ctx.lock, &ctx.layout);
    if findings.is_empty() {
        println!("no issues found");
        return Ok(());
    }
    for finding in &findings {
        let label = match finding.severity {
            doctor::Severity::Warning => "warning",
            doctor::Severity::Info => "info",
        };
        println!("[{label}] {}", finding.message);
    }
    Ok(())
}

fn cmd_which(command: String) -> anyhow::Result<()> {
    let ctx = Ctx::load()?;
    for entry in config::all_entries(&ctx.config) {
        let key = entry.key();
        let default = config::tool_key(entry.name());
        let names = config::BinSpec::declared_names(entry.common().bin.as_ref(), default);
        if names.contains(&command) {
            println!("{key}");
            return Ok(());
        }
    }
    bail!("no configured tool provides `{command}`")
}
