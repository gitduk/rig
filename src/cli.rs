//! Subcommand dispatch: install/update/remove/list/sync/doctor/which.
//! `main.rs` is a one-line wrapper around `run`.
use std::fs;
use std::io::IsTerminal as _;
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
    Sync {
        /// Re-probe every tool's `eval` cacheability in place
        #[arg(long)]
        force: bool,
    },
    /// Run diagnostic checks
    Doctor,
    /// Resolve a command name to the tool that provides it
    Which { command: String },
    /// Print a shell completion script for `rig` itself
    Completions { shell: clap_complete::Shell },
}

pub fn run() -> ExitCode {
    match dispatch(Cli::parse().command) {
        Ok(code) => code,
        Err(e) => {
            print_error(None, &e);
            ExitCode::FAILURE
        }
    }
}

/// Replaces anyhow's `{:?}` "Caused by:" chain — every `bail!` here is
/// already a self-contained sentence, so print it as plain paragraphs.
/// Color lives only here, never in the message text itself (which also
/// flows through `.to_string()` in tests and could end up in a log file).
fn print_error(prefix: Option<&str>, err: &anyhow::Error) {
    let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let (bold_red, bold_green, dim, reset) = if color {
        ("\x1b[1;31m", "\x1b[1;32m", "\x1b[2m", "\x1b[0m")
    } else {
        ("", "", "", "")
    };

    let mut chain = err.chain();
    let head = chain.next().expect("anyhow::Error always has a message");
    match prefix {
        Some(p) => eprintln!("{bold_red}{p}:{reset} {head}"),
        None => eprintln!("{bold_red}error:{reset} {head}"),
    }
    for cause in chain {
        for line in cause.to_string().lines() {
            // `- `/`+ `/`| ` come from config.rs::render_entry_diff — old
            // value, suggested value, reproduced context; else stays dim.
            if let Some(rest) = line.strip_prefix("- ") {
                eprintln!("{bold_red}  - {rest}{reset}");
            } else if let Some(rest) = line.strip_prefix("+ ") {
                eprintln!("{bold_green}  + {rest}{reset}");
            } else if let Some(rest) = line.strip_prefix("| ") {
                eprintln!("    {rest}");
            } else {
                eprintln!("{dim}  {line}{reset}");
            }
        }
    }
}

fn dispatch(command: Command) -> anyhow::Result<ExitCode> {
    match command {
        Command::Install { tools } => cmd_install(tools),
        Command::Update { tools, force } => cmd_update(tools, force),
        Command::Remove { tools } => cmd_remove(tools),
        Command::List => cmd_list().map(|()| ExitCode::SUCCESS),
        Command::Sync { force } => cmd_sync(force),
        Command::Doctor => cmd_doctor().map(|()| ExitCode::SUCCESS),
        Command::Which { command } => cmd_which(command).map(|()| ExitCode::SUCCESS),
        Command::Completions { shell } => {
            cmd_completions(shell);
            Ok(ExitCode::SUCCESS)
        }
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
        let config_path = home.join(".rig.toml");
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

fn cmd_install(tools: Vec<String>) -> anyhow::Result<ExitCode> {
    if tools.is_empty() {
        bail!("specify at least one tool to install");
    }
    let mut ctx = Ctx::load()?;

    // Save after every tool, not once at batch end — a killed process must
    // not lose lock state for tools that already finished installing.
    let mut failed = false;
    for name in &tools {
        match install_entry(&mut ctx, name).and_then(|()| ctx.save()) {
            Ok(()) => {}
            Err(e) => {
                failed = true;
                print_error(Some(name), &e);
            }
        }
    }

    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn install_entry(ctx: &mut Ctx, requested: &str) -> anyhow::Result<()> {
    let resolved = config::resolve_tool(&ctx.config, requested)
        .ok_or_else(|| anyhow!("{requested} is not in config.toml"))?;
    let key = resolved.key();

    // Skip only if genuinely healthy — a dangling bin must still fall
    // through, since `command_not_found_handler` relies on this to self-heal.
    if let Some(existing) = ctx.lock.tool.get(&key) {
        let healthy = match resolved {
            // A plugin has no bins — "healthy" means its clone still exists.
            ResolvedEntry::Plugin(_) => existing
                .pkg
                .as_deref()
                .is_some_and(|p| std::path::Path::new(p).exists()),
            _ => {
                !existing.bins.is_empty()
                    && existing
                        .bins
                        .iter()
                        .all(|b| std::path::Path::new(b).exists())
            }
        };
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
        ResolvedEntry::Plugin(e) => sources::plugin::install(e, &ctx.layout)?,
    };
    println!("installed {key} {}", new_lock.version);
    ctx.lock.tool.insert(key, new_lock);
    Ok(())
}

fn cmd_update(tools: Vec<String>, force: bool) -> anyhow::Result<ExitCode> {
    let mut ctx = Ctx::load()?;
    let filter = (!tools.is_empty()).then_some(tools.as_slice());
    let reports = update::update_all(
        &ctx.config,
        &mut ctx.lock,
        &ctx.layout,
        force,
        false,
        filter,
    );

    Ok(if print_reports(&reports) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Shared by `cmd_update` and `cmd_sync --force`. Returns whether any
/// report failed.
fn print_reports(reports: &[update::Report]) -> bool {
    let mut failed = false;
    for report in reports {
        match &report.outcome {
            Ok(outcome) => println!("{}: {}", report.tool, describe_outcome(outcome)),
            Err(e) => {
                failed = true;
                print_error(Some(&report.tool), e);
            }
        }
    }
    failed
}

fn describe_outcome(outcome: &update::Outcome) -> String {
    match outcome {
        update::Outcome::UpToDate { version } => format!("up to date ({version})"),
        update::Outcome::Updated { from, to } => format!("updated {from} -> {to}"),
        update::Outcome::Reran => "rerun".to_string(),
        update::Outcome::EvalRefreshed { cacheable } => {
            let label = match cacheable {
                Some(true) => "cacheable",
                Some(false) => "not cacheable, stays live",
                None => "probe failed, stays live",
            };
            format!("eval refreshed ({label})")
        }
        update::Outcome::NoEval => "no eval configured, nothing to refresh".to_string(),
    }
}

fn cmd_remove(tools: Vec<String>) -> anyhow::Result<ExitCode> {
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
                print_error(Some(name), &e);
            }
        }
    }

    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
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
    let entries = config::all_entries(&ctx.config);
    for entry in &entries {
        let key = entry.key();
        let marker = if ctx.lock.tool.contains_key(&key) {
            "\u{25cf}"
        } else {
            "\u{25cb}"
        };
        match entry.description() {
            Some(desc) => println!("{marker} {key} - {desc}"),
            None => println!("{marker} {key}"),
        }
    }
    Ok(())
}

/// Plugins aren't installed on demand like binaries — `sync` clones any
/// that are missing so their `source` line in init.zsh resolves. `--force`
/// additionally re-probes every tool's `eval` cacheability in place.
fn cmd_sync(force: bool) -> anyhow::Result<ExitCode> {
    let mut ctx = Ctx::load()?;

    let mut failed = false;
    if force {
        let reports =
            update::update_all(&ctx.config, &mut ctx.lock, &ctx.layout, false, true, None);
        failed |= print_reports(&reports);
    }

    let old_keys = fs::read_to_string(&ctx.layout.init_zsh_path)
        .map(|content| initzsh::declared_tool_keys(&content))
        .unwrap_or_default();

    let mut missing = Vec::new();
    // Indexed: the backfill branch below needs `&mut ctx` to save, which a
    // live borrow from `for plugin in &ctx.config.plugin` would rule out.
    for i in 0..ctx.config.plugin.len() {
        let key = config::tool_key(&ctx.config.plugin[i].name).to_string();
        let dest = ctx.layout.plugins_dir.join(&key);
        if dest.exists() {
            // Backfill clones that predate plugin tracking in `rig.lock`,
            // saved right after — same reason as `cmd_install`'s per-tool save.
            if !ctx.lock.tool.contains_key(&key) {
                let backfilled =
                    sources::plugin::read_installed(&ctx.config.plugin[i], &ctx.layout);
                let name = ctx.config.plugin[i].name.clone();
                let result = backfilled.and_then(|tool_lock| {
                    ctx.lock.tool.insert(key, tool_lock);
                    ctx.save()
                });
                if let Err(e) = result {
                    failed = true;
                    print_error(Some(&name), &e);
                }
            }
            continue;
        }
        missing.push(ctx.config.plugin[i].name.clone());
    }
    // Goes through the same path as `rig install` — a plugin's `sync`-time
    // clone shouldn't skip whatever `install_entry` does around it.
    for name in &missing {
        if let Err(e) = install_entry(&mut ctx, name).and_then(|()| ctx.save()) {
            failed = true;
            print_error(Some(name), &e);
        }
    }

    // Unconditional: `sync`'s job is to regenerate init.zsh from the
    // current config even when no plugin needed cloning or backfilling.
    ctx.save()?;
    write_own_completions(&ctx.layout)?;
    report_tool_set_diff(&old_keys, &ctx.config);
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn cmd_completions(shell: clap_complete::Shell) {
    let mut cmd = <Cli as clap::CommandFactory>::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
}

/// `rig` isn't installed via its own `[[repo]]`/`[[cargo]]` flow, so nothing
/// else keeps its completions in `completions_dir` current — sync does it.
fn write_own_completions(layout: &Layout) -> anyhow::Result<()> {
    let mut cmd = <Cli as clap::CommandFactory>::command();
    let mut buf = Vec::new();
    clap_complete::generate(clap_complete::Shell::Zsh, &mut cmd, "rig", &mut buf);
    fs::create_dir_all(&layout.completions_dir)
        .with_context(|| format!("failed to create {}", layout.completions_dir.display()))?;
    let dest = layout.completions_dir.join("_rig");
    fs::write(&dest, buf).with_context(|| format!("failed to write {}", dest.display()))
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
        // A plugin is sourced, never invoked by command name.
        let Some(common) = entry.common() else {
            continue;
        };
        let key = entry.key();
        let default = config::tool_key(entry.name());
        let names = config::BinSpec::declared_names(common.bin.as_ref(), default);
        if names.contains(&command) {
            println!("{key}");
            return Ok(());
        }
    }
    bail!("no configured tool provides `{command}`")
}
