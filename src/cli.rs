//! Subcommand dispatch: install/update/remove/list/sync/doctor/which.
//! `main.rs` is a one-line wrapper around `run`.
use std::collections::HashSet;
use std::fs;
use std::io::IsTerminal as _;
use std::process::{Command as ProcessCommand, ExitCode};

use anyhow::{Context as _, anyhow, bail};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::config::{self, Config, ResolvedEntry};
use crate::doctor;
use crate::initzsh;
use crate::lock::{self, Lock, ToolLock};
use crate::paths::{self, Layout};
use crate::pool;
use crate::remove;
use crate::sources;
use crate::update;
use crate::version::{UNVERSIONED, display_version};

#[derive(Parser)]
#[command(name = "rig", version, about = "Tool installer + zsh loader")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install one or more configured tools
    #[command(visible_alias = "i")]
    Install { tools: Vec<String> },
    /// Update tools (omit to update everything in config.toml)
    #[command(visible_alias = "u")]
    Update {
        tools: Vec<String>,
        #[arg(short, long)]
        force: bool,
        /// Update rig itself (binary + bootstrap) instead of configured tools
        #[arg(long = "self", conflicts_with = "tools")]
        self_update: bool,
    },
    /// Remove an installed tool's rig-owned files
    #[command(visible_alias = "rm")]
    Remove { tools: Vec<String> },
    /// List configured tools and whether they're installed
    #[command(visible_alias = "ls")]
    List,
    /// Regenerate init.zsh from the current config
    #[command(visible_alias = "s")]
    Sync {
        /// Re-probe every tool's `eval` cacheability in place
        #[arg(long)]
        force: bool,
    },
    /// Run diagnostic checks
    #[command(visible_alias = "d")]
    Doctor,
    /// Open the config file in $EDITOR (falls back to vi)
    #[command(visible_alias = "e")]
    Edit,
    /// Resolve a command name to the tool that provides it
    #[command(visible_alias = "w")]
    Which { command: String },
    /// Print a shell completion script for `rig` itself
    #[command(visible_alias = "c")]
    Completions { shell: clap_complete::Shell },
}

pub fn run() -> ExitCode {
    let matches = build_command().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        // Matches came from our own command, so this can't realistically
        // fail — but let clap render/exit rather than unwrap.
        Err(e) => e.exit(),
    };
    match dispatch(cli.command) {
        Ok(code) => code,
        Err(e) => {
            print_error(None, &e);
            ExitCode::FAILURE
        }
    }
}

/// clap 4.6 renders aliases only as trailing `[alias: i]`; hand-render the
/// Commands block for cargo-style `install, i` (name/aliases stay intact).
fn build_command() -> clap::Command {
    let cmd = Cli::command();
    let template = format!(
        "{{about-with-newline}}\n{{usage-heading}} {{usage}}\n\n\
         Commands:\n{}\n\nOptions:\n{{options}}",
        render_commands(&cmd)
    );
    cmd.help_template(template)
}

/// One `name, alias  About` row per subcommand, name column padded. Appends the
/// auto-generated `help` row, which isn't in the derived command until built.
fn render_commands(cmd: &clap::Command) -> String {
    let mut rows: Vec<(String, String)> = cmd
        .get_subcommands()
        .map(|sub| {
            let mut name = sub.get_name().to_string();
            for alias in sub.get_all_aliases() {
                name.push_str(", ");
                name.push_str(alias);
            }
            let about = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
            (name, about)
        })
        .collect();
    if !rows.iter().any(|(n, _)| n == "help") {
        rows.push((
            "help".to_string(),
            "Print this message or the help of the given subcommand(s)".to_string(),
        ));
    }

    let width = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    rows.iter()
        .map(|(name, about)| format!("  {name:<width$}  {about}"))
        .collect::<Vec<_>>()
        .join("\n")
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
        Command::Update {
            self_update: true,
            force,
            ..
        } => cmd_self_update(force),
        Command::Update {
            tools,
            force,
            self_update: false,
        } => cmd_update(tools, force),
        Command::Remove { tools } => cmd_remove(tools),
        Command::List => cmd_list().map(|()| ExitCode::SUCCESS),
        Command::Sync { force } => cmd_sync(force),
        Command::Doctor => cmd_doctor().map(|()| ExitCode::SUCCESS),
        Command::Edit => cmd_edit().map(|()| ExitCode::SUCCESS),
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

/// Commands that rewrite state serialize on a whole-command advisory lock;
/// acquired before `Ctx::load` so the read-modify-write cycle is atomic
/// against other rig processes. state_dir is prefix-independent, so no
/// config is needed to find the lock file.
fn state_lock_path() -> anyhow::Result<std::path::PathBuf> {
    let home = paths::home_dir()?;
    Ok(Layout::new(&home, "~").lock_file)
}

fn acquire_state_lock() -> anyhow::Result<lock::StateLock> {
    lock::StateLock::acquire(&state_lock_path()?)
}

fn cmd_install(tools: Vec<String>) -> anyhow::Result<ExitCode> {
    if tools.is_empty() {
        bail!("specify at least one tool to install");
    }
    let _state_lock = acquire_state_lock()?;
    let Ctx {
        config,
        mut lock,
        layout,
    } = Ctx::load()?;
    let workers = config.settings.parallel as usize;

    // Normalize to lock keys and dedup: `rig install delta dandavison/delta`
    // is one tool. Two workers racing on the same key would both pass the
    // snapshot-based conflict check, then trip each other's "already exists
    // and isn't tracked" guard on the freshly-created symlink.
    let mut seen = HashSet::new();
    let tools: Vec<String> = tools
        .into_iter()
        .map(|t| {
            config::resolve_tool(&config, &t)
                .map(|r| r.key())
                .unwrap_or(t)
        })
        .filter(|key| seen.insert(key.clone()))
        .collect();

    // Workers only read shared state, so the conflict check runs against a
    // snapshot taken at batch start. The merge below keeps the first-recorded
    // lock entry — same outcome as the serial path, where the second install
    // of an already-tracked tool is a no-op.
    let lock_snapshot = lock.clone();

    // Save after every tool, not once at batch end — a killed process must
    // not lose lock state for tools that already finished installing.
    let mut failed = false;
    pool::run_pool(
        tools,
        workers,
        |name| install_one(&config, &lock_snapshot, &layout, name),
        |name, result| match result {
            Err(e) => {
                failed = true;
                print_error(Some(&name), &e);
            }
            Ok((key, outcome)) => {
                let message = describe_install_outcome(&outcome);
                let new_lock = match outcome {
                    InstallOutcome::AlreadyCurrent { .. } => None,
                    InstallOutcome::Installed(new) => Some(new),
                    InstallOutcome::Updated { lock: new, .. } => Some(new),
                };
                match new_lock {
                    None => println!("{key}: {message}"),
                    Some(entry) => {
                        lock.tool.insert(key.clone(), entry);
                        if let Err(e) = lock::save_and_sync(&config, &lock, &layout) {
                            failed = true;
                            print_error(Some(&name), &e);
                        } else {
                            println!("{key}: {message}");
                        }
                    }
                }
            }
        },
    );

    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// The three ways installing one tool can resolve — distinct because
/// `rig install` folds in an update check for an already-installed tool.
enum InstallOutcome {
    /// Freshly installed, or repaired from a dangling/unhealthy state.
    Installed(ToolLock),
    /// Was already installed; a newer version replaced it.
    Updated { from: String, lock: ToolLock },
    /// Already installed and already the latest.
    AlreadyCurrent { version: String },
}

/// Install-path counterpart to `describe_outcome`, worded identically so the
/// two commands can't drift into reporting the same result differently.
fn describe_install_outcome(outcome: &InstallOutcome) -> String {
    match outcome {
        InstallOutcome::Installed(lock) if lock.version == UNVERSIONED => "installed".to_string(),
        InstallOutcome::Installed(lock) => {
            format!("installed {}", display_version(&lock.version))
        }
        InstallOutcome::Updated { from, lock } => format!(
            "updated {} -> {}",
            display_version(from),
            display_version(&lock.version)
        ),
        InstallOutcome::AlreadyCurrent { version } => {
            format!("up to date ({})", display_version(version))
        }
    }
}

/// Installs one tool. Pure: reads `config`/`lock`/`layout`, never mutates
/// them; the caller merges the returned lock (if any) and persists.
fn install_one(
    config: &Config,
    lock: &Lock,
    layout: &Layout,
    requested: &str,
) -> anyhow::Result<(String, InstallOutcome)> {
    let resolved = config::resolve_tool(config, requested)
        .ok_or_else(|| anyhow!("{requested} is not in config.toml"))?;
    let key = resolved.key();

    // Skip only if genuinely healthy — a dangling bin must still fall
    // through, since `command_not_found_handler` relies on this to self-heal.
    if let Some(existing) = lock.tool.get(&key) {
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
            // Already installed — fold in an update check so `rig install`
            // also pulls a newer version when one exists (`rig update` flow).
            let from = existing.version.clone();
            let outcome = match update::check_and_update(config, lock, layout, requested)? {
                Some(new) => InstallOutcome::Updated { from, lock: new },
                None => InstallOutcome::AlreadyCurrent { version: from },
            };
            return Ok((key, outcome));
        }
    }

    let new_lock = match resolved {
        ResolvedEntry::Repo(e) => sources::repo::install(e, layout, lock, sources::Phase::Install)?,
        ResolvedEntry::Git(e) => sources::git::install(e, layout, lock, sources::Phase::Install)?,
        ResolvedEntry::Cargo(e) => {
            sources::cargo::install(e, layout, lock, sources::Phase::Install)?
        }
        ResolvedEntry::Node(e) => sources::node::install(e, layout, lock, sources::Phase::Install)?,
        ResolvedEntry::Apt(e) => sources::apt::install(e, layout, sources::Phase::Install)?,
        ResolvedEntry::Curl(e) => sources::curl::install(e, layout, sources::Phase::Install)?,
        ResolvedEntry::Plugin(e) => sources::plugin::install(e, layout)?,
    };
    Ok((key, InstallOutcome::Installed(new_lock)))
}

/// Serial wrapper for callers that need the lock merged immediately
/// (`cmd_sync`'s plugin backfill) — same semantics as `install_one` + merge.
fn install_entry(ctx: &mut Ctx, requested: &str) -> anyhow::Result<()> {
    let (key, outcome) = install_one(&ctx.config, &ctx.lock, &ctx.layout, requested)?;
    println!("{key}: {}", describe_install_outcome(&outcome));
    match outcome {
        InstallOutcome::Installed(tool_lock)
        | InstallOutcome::Updated {
            lock: tool_lock, ..
        } => {
            ctx.lock.tool.insert(key, tool_lock);
        }
        InstallOutcome::AlreadyCurrent { .. } => {}
    }
    Ok(())
}

/// `rig update --self` — rig manages its own binary and bootstrap, no
/// config entry needed. Version comes from the build, not `rig.lock`.
fn cmd_self_update(force: bool) -> anyhow::Result<ExitCode> {
    let _state_lock = acquire_state_lock()?;
    let mut ctx = Ctx::load()?;
    // A `[[repo]] gitduk/rig` entry left over from before self-management
    // still works (`rig update rig` runs the repo flow) but is redundant.
    if config::resolve_tool(&ctx.config, "rig").is_some() {
        eprintln!(
            "rig: warning: the `[[repo]] gitduk/rig` config entry is redundant — \
             rig updates itself via `rig update --self`; you can remove it"
        );
    }
    let outcome = crate::self_update::update_self(force, &ctx.layout, &mut ctx.lock)?;
    if matches!(outcome, update::Outcome::Updated { .. }) {
        // Persists the retired `rig.lock` entry; init.zsh rewrite is
        // skipped when nothing changed.
        ctx.save()?;
    }
    println!("rig: {}", describe_outcome(&outcome));
    Ok(ExitCode::SUCCESS)
}

fn cmd_update(tools: Vec<String>, force: bool) -> anyhow::Result<ExitCode> {
    let _state_lock = acquire_state_lock()?;
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
    // `update_all` returns completion order, i.e. network-latency order —
    // sorted so two runs of the same command produce comparable output.
    let mut reports: Vec<&update::Report> = reports.iter().collect();
    reports.sort_by(|a, b| a.tool.cmp(&b.tool));

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
        update::Outcome::UpToDate { version } => {
            format!("up to date ({})", display_version(version))
        }
        update::Outcome::Updated { from, to } => format!(
            "updated {} -> {}",
            display_version(from),
            display_version(to)
        ),
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
    let _state_lock = acquire_state_lock()?;
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
    remove::remove(&key, tool_lock, &ctx.layout)?;
    ctx.lock.tool.remove(&key);
    println!("{key}: removed");
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
    // Non-blocking: `rig sync` runs unattended on shell startup, so it skips
    // rather than hang behind a slow install — the holder regenerates init.zsh.
    let Some(_state_lock) = lock::StateLock::try_acquire(&state_lock_path()?)? else {
        eprintln!("rig: warning: another rig process is running; skipping sync");
        return Ok(ExitCode::SUCCESS);
    };
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
    let contents = String::from_utf8_lossy(&buf);
    lock::atomic_write(&dest, &contents)
        .with_context(|| format!("failed to write {}", dest.display()))
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

/// Open the config file in $EDITOR, splitting arguments on whitespace
/// instead of a shell (supports flags like `code --wait`). Empty falls back to vi.
fn cmd_edit() -> anyhow::Result<()> {
    let home = paths::home_dir()?;
    let layout = Layout::new(&home, "~");
    let editor = std::env::var("EDITOR").unwrap_or_default();
    let parts: Vec<&str> = editor.split_whitespace().collect();
    let program = parts.first().copied().unwrap_or("vi");
    let mut cmd = ProcessCommand::new(program);
    cmd.args(parts.iter().skip(1));
    cmd.arg(&layout.config_path);
    let status = cmd
        .status()
        .with_context(|| format!("failed to run editor `{editor}`"))?;
    if !status.success() {
        bail!("editor exited with {status}");
    }
    Ok(())
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
