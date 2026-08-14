//! Generates `init.zsh` — fpath, env, `RIG_CMD` + `command_not_found`,
//! cached/live `eval` blocks, and the plugin load queue. Pure text
//! assembly; writing the file is the caller's job.

use std::collections::HashSet;

use crate::config::{self, BinSpec, Common, Config, EvalSpec, tool_key};
use crate::lock::{Lock, ToolLock};
use crate::paths::Layout;

/// Reads back `RIG_CMD` plus the `# rig-plugin:` markers `render_plugins`
/// leaves (plugins never enter `RIG_CMD`) — lets `sync` diff declared tools.
pub fn declared_tool_keys(init_zsh: &str) -> HashSet<String> {
    let mut keys = HashSet::new();
    let mut in_rig_cmd = false;
    for line in init_zsh.lines() {
        if let Some(plugin) = line.strip_prefix("# rig-plugin: ") {
            keys.insert(plugin.to_string());
        } else if in_rig_cmd {
            if line.trim() == ")" {
                in_rig_cmd = false;
            } else if let Some(name) = line.split_whitespace().nth(1) {
                keys.insert(name.to_string());
            }
        } else if line.trim() == "RIG_CMD=(" {
            in_rig_cmd = true;
        }
    }
    keys
}

pub fn render(config: &Config, lock: &Lock, layout: &Layout) -> String {
    let entries: Vec<(String, &Common)> = config::all_entries(config)
        .into_iter()
        .filter_map(|e| e.common().map(|c| (e.key(), c)))
        .collect();
    let eager: Vec<(String, &Common)> = entries.iter().filter(|(_, c)| !c.lazy).cloned().collect();

    let mut out = String::new();
    render_fpath(&mut out, layout);
    render_env(&mut out, &eager);
    render_run(&mut out, &eager);
    render_rig_cmd_and_handler(&mut out, config, layout);
    render_eval_blocks(&mut out, &eager, lock);
    render_lazy_blocks(&mut out, config, lock);
    render_plugins(&mut out, config, layout);
    out
}

fn render_fpath(out: &mut String, layout: &Layout) {
    out.push_str(&format!(
        "fpath=({} $fpath)\n\n",
        layout.completions_dir.display()
    ));
}

/// Rig, not the shell, is responsible for `~` expansion in `env`.
fn render_env(out: &mut String, entries: &[(String, &Common)]) {
    for (_, common) in entries {
        for (key, value) in &common.env {
            out.push_str(&format!(
                "export {key}=\"{}\"\n",
                expand_tilde_export(value)
            ));
        }
    }
    out.push('\n');
}

/// Plain shell text (e.g. an alias) — unlike `eval`, output isn't captured.
fn render_run(out: &mut String, entries: &[(String, &Common)]) {
    for (_, common) in entries {
        let Some(run) = &common.run else { continue };
        for line in run.lines() {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push('\n');
}

/// Rig expands a leading `~` itself; the rest is escaped so it can't
/// break out of the double-quoted `export` or trigger zsh substitution.
fn expand_tilde_export(value: &str) -> String {
    match value.strip_prefix("~/") {
        Some(rest) => format!("$HOME/{}", escape_dquoted(rest)),
        None if value == "~" => "$HOME".to_string(),
        None => escape_dquoted(value),
    }
}

fn escape_dquoted(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | '"' | '`' | '$') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// `RIG_CMD` covers every configured tool, installed or not.
fn render_rig_cmd_and_handler(out: &mut String, config: &Config, layout: &Layout) {
    out.push_str("typeset -gA RIG_CMD\nRIG_CMD=(\n");
    for entry in config::all_entries(config) {
        // A plugin is sourced, never invoked by command name.
        let Some(common) = entry.common() else {
            continue;
        };
        let key = entry.key();
        // The lock key stays scope-prefixed, but a real command is never
        // typed with the `@scope/` prefix — guess by last path segment.
        let default = tool_key(entry.name());
        for cmd in BinSpec::declared_names(common.bin.as_ref(), default) {
            out.push_str(&format!("  {cmd} {key}\n"));
        }
    }
    out.push_str(")\n\n");

    out.push_str(&format!(
        r#"command_not_found_handler() {{
  local tool="${{RIG_CMD[$1]}}"
  [[ -z "$tool" ]] && tool=$(rig which "$1" 2>/dev/null)
  [[ -z "$tool" ]] && {{ print -ru2 "zsh: command not found: $1"; return 127 }}
  rig install "$tool" || return 127
  source {init_zsh}
  rehash
  command "$@"
}}

"#,
        init_zsh = layout.init_zsh_path.display()
    ));
}

/// `eval` only appears once a tool is installed and probed; an unprobed
/// installed tool falls back to a live call, never silence.
fn render_eval_blocks(out: &mut String, entries: &[(String, &Common)], lock: &Lock) {
    for (key, common) in entries {
        let Some(eval) = &common.eval else { continue };
        let Some(tool_lock) = lock.tool.get(key) else {
            continue;
        };
        out.push_str(&render_eval_text(eval, tool_lock));
        out.push('\n');
    }
}

/// Cached -> inline the captured output; else a plain `eval` call. Shared
/// so the eager and lazy render paths can't disagree on this decision.
fn render_eval_text(eval: &EvalSpec, tool_lock: &ToolLock) -> String {
    let cmd = eval.command();
    if tool_lock.eval_cacheable == Some(true)
        && let Some(cached) = &tool_lock.eval_cached_output
    {
        format!("# cached: {cmd} (evidence: none — probed clean)\n{cached}\n")
    } else {
        format!("eval \"$({cmd})\"\n")
    }
}

/// `lazy = true`: env + eval + run move into a shared init, run once from
/// whichever trigger fires first (command name, or `bind`'s ZLE widget).
fn render_lazy_blocks(out: &mut String, config: &Config, lock: &Lock) {
    for entry in config::all_entries(config) {
        // A plugin has no `Common` and thus can never be `lazy`.
        let Some(common) = entry.common() else {
            continue;
        };
        if !common.lazy || (common.env.is_empty() && common.eval.is_none() && common.run.is_none())
        {
            continue;
        }
        let key = entry.key();
        let Some(tool_lock) = lock.tool.get(&key) else {
            continue;
        };
        let names = BinSpec::declared_names(common.bin.as_ref(), tool_key(entry.name()));
        let bound = common.bind.as_deref().and_then(|spec| spec.split_once(':'));
        if names.is_empty() && bound.is_none() {
            continue;
        }

        let ident = zsh_ident(&key);
        let init_fn = format!("_rig_lazy_init_{ident}");

        let mut body = String::new();
        if !names.is_empty() {
            body.push_str(&format!("  unfunction {} 2>/dev/null\n", names.join(" ")));
        }
        for (env_key, value) in &common.env {
            body.push_str(&format!(
                "  export {env_key}=\"{}\"\n",
                expand_tilde_export(value)
            ));
        }
        if let Some(eval) = &common.eval {
            for line in render_eval_text(eval, tool_lock).lines() {
                body.push_str(&format!("  {line}\n"));
            }
        }
        if let Some(run) = &common.run {
            for line in run.lines() {
                body.push_str(&format!("  {line}\n"));
            }
        }
        out.push_str(&format!("{init_fn}() {{\n{body}}}\n\n"));

        // Rebinding on every trigger path (not just the widget one) lets a
        // typed command name fix up `bind`'s key too, whichever fires first.
        let rebind = bound
            .map(|(key_seq, widget)| format!("  bindkey '{key_seq}' {widget}\n"))
            .unwrap_or_default();
        for name in &names {
            out.push_str(&format!(
                "{name}() {{\n  {init_fn}\n{rebind}  {name} \"$@\"\n}}\n\n"
            ));
        }

        if let Some((key_seq, widget)) = bound {
            let widget_fn = format!("_rig_lazy_bind_{ident}");
            out.push_str(&format!(
                "{widget_fn}() {{\n  {init_fn}\n  bindkey '{key_seq}' {widget}\n  zle {widget}\n}}\n\
                 zle -N {widget_fn}\n\
                 bindkey '{key_seq}' {widget_fn}\n\n"
            ));
        }
    }
}

/// A tool key can contain `@`, `/`, `-` — none valid in a zsh identifier.
fn zsh_ident(key: &str) -> String {
    key.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Plugins are sourced in config order, and each plugin's `run` is emitted
/// right after its `source` — the functions `run` may call are only defined
/// once the plugin has been sourced.
fn render_plugins(out: &mut String, config: &Config, layout: &Layout) {
    for plugin in &config.plugin {
        let key = tool_key(&plugin.name);
        out.push_str(&format!("# rig-plugin: {key}\n"));
        let clone_dir = layout.plugins_dir.join(key);
        if let Some(source) = &plugin.source {
            out.push_str(&format!("source {}\n", clone_dir.join(source).display()));
        }
        if let Some(run) = &plugin.run {
            for line in run.lines() {
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, EvalSpec, Host, PluginEntry, RepoEntry, RunSpec};
    use crate::lock::ToolLock;
    use std::collections::HashMap;
    use std::path::Path;
    use time::OffsetDateTime;

    fn layout() -> Layout {
        Layout::new(Path::new("/home/kaige"), "~/.local")
    }

    fn repo_entry(name: &str, eval: Option<EvalSpec>) -> RepoEntry {
        RepoEntry {
            name: name.to_string(),
            host: Host::Github,
            bpick: None,
            extract: None,
            common: Common {
                eval,
                ..config::test_common()
            },
        }
    }

    fn lazy_repo_entry(
        name: &str,
        env: HashMap<String, String>,
        eval: Option<EvalSpec>,
        bind: Option<&str>,
    ) -> RepoEntry {
        RepoEntry {
            name: name.to_string(),
            host: Host::Github,
            bpick: None,
            extract: None,
            common: Common {
                env,
                eval,
                lazy: true,
                bind: bind.map(str::to_string),
                ..config::test_common()
            },
        }
    }

    #[test]
    fn lazy_tool_defers_env_and_eval_into_a_stub_function() {
        let mut env = HashMap::new();
        env.insert("ATUIN_TMUX_POPUP".to_string(), "false".to_string());
        let mut config = Config::default();
        config.repo.push(lazy_repo_entry(
            "atuinsh/atuin",
            env,
            Some(EvalSpec::Cmd("atuin init zsh".to_string())),
            None,
        ));

        let mut lock = Lock::default();
        lock.tool
            .insert("atuin".to_string(), tool_lock_with_eval(None, None));

        let out = render(&config, &lock, &layout());
        let lines: Vec<&str> = out.lines().collect();

        assert!(!lines.contains(&"export ATUIN_TMUX_POPUP=\"false\""));
        assert!(!lines.contains(&"eval \"$(atuin init zsh)\""));
        assert!(lines.contains(&"_rig_lazy_init_atuin() {"));
        assert!(lines.contains(&"  unfunction atuin 2>/dev/null"));
        assert!(lines.contains(&"  export ATUIN_TMUX_POPUP=\"false\""));
        assert!(lines.contains(&"  eval \"$(atuin init zsh)\""));
        assert!(lines.contains(&"atuin() {"));
        assert!(lines.contains(&"  _rig_lazy_init_atuin"));
        assert!(lines.contains(&"  atuin \"$@\""));
    }

    #[test]
    fn lazy_tool_not_yet_installed_is_skipped() {
        let mut config = Config::default();
        config.repo.push(lazy_repo_entry(
            "atuinsh/atuin",
            HashMap::new(),
            Some(EvalSpec::Cmd("atuin init zsh".to_string())),
            None,
        ));

        let out = render(&config, &Lock::default(), &layout());

        assert!(!out.contains("atuin() {"));
    }

    #[test]
    fn lazy_bind_wires_a_placeholder_widget_that_rebinds_on_first_use() {
        let mut config = Config::default();
        config.repo.push(lazy_repo_entry(
            "denisidoro/navi",
            HashMap::new(),
            Some(EvalSpec::Cmd("navi widget zsh".to_string())),
            Some("^g:_navi_widget"),
        ));

        let mut lock = Lock::default();
        lock.tool
            .insert("navi".to_string(), tool_lock_with_eval(None, None));

        let out = render(&config, &lock, &layout());
        let lines: Vec<&str> = out.lines().collect();

        assert!(lines.contains(&"_rig_lazy_bind_navi() {"));
        assert!(lines.contains(&"  _rig_lazy_init_navi"));
        assert!(lines.contains(&"  bindkey '^g' _navi_widget"));
        assert!(lines.contains(&"  zle _navi_widget"));
        assert!(lines.contains(&"zle -N _rig_lazy_bind_navi"));
        assert!(lines.contains(&"bindkey '^g' _rig_lazy_bind_navi"));
        // The command-name path rebinds too, so whichever trigger fires
        // first leaves the key pointed at the real widget, not the stub.
        assert!(lines.contains(&"navi() {"));
        assert!(lines.contains(&"  _rig_lazy_init_navi"));
    }

    #[test]
    fn expand_tilde_export_escapes_dquote_metacharacters() {
        assert_eq!(
            expand_tilde_export(r#"say "hi" `whoami` $USER \done"#),
            r#"say \"hi\" \`whoami\` \$USER \\done"#
        );
        assert_eq!(
            expand_tilde_export("~/configs/$secret"),
            r"$HOME/configs/\$secret"
        );
        assert_eq!(expand_tilde_export("~"), "$HOME");
    }

    #[test]
    fn eager_run_is_rendered_verbatim_at_top_level() {
        let mut config = Config::default();
        config.repo.push(RepoEntry {
            name: "casey/just".to_string(),
            host: Host::Github,
            bpick: None,
            extract: None,
            common: Common {
                run: Some(RunSpec::Single(r#"alias js="just""#.to_string())),
                ..config::test_common()
            },
        });

        let out = render(&config, &Lock::default(), &layout());
        let lines: Vec<&str> = out.lines().collect();

        assert!(lines.contains(&r#"alias js="just""#));
    }

    #[test]
    fn lazy_run_is_deferred_into_the_shared_init_function() {
        let mut config = Config::default();
        config.repo.push(RepoEntry {
            name: "dandavison/delta".to_string(),
            host: Host::Github,
            bpick: None,
            extract: None,
            common: Common {
                run: Some(RunSpec::Multiple(vec![
                    "alias d=delta".to_string(),
                    "alias dd=\"delta --diff-so-fancy\"".to_string(),
                ])),
                lazy: true,
                ..config::test_common()
            },
        });

        let mut lock = Lock::default();
        lock.tool
            .insert("delta".to_string(), tool_lock_with_eval(None, None));

        let out = render(&config, &lock, &layout());
        let lines: Vec<&str> = out.lines().collect();

        // Not rendered before the tool's own init function ran.
        assert!(!lines.contains(&"alias d=delta"));
        assert!(lines.contains(&"_rig_lazy_init_delta() {"));
        assert!(lines.contains(&"  alias d=delta"));
        assert!(lines.contains(&"  alias dd=\"delta --diff-so-fancy\""));
    }

    #[test]
    fn rig_cmd_covers_uninstalled_tools() {
        let mut config = Config::default();
        config.repo.push(repo_entry("dandavison/delta", None));

        let lock = Lock::default();
        let out = render(&config, &lock, &layout());

        assert!(out.contains("RIG_CMD"));
        assert!(out.contains("delta delta"));
        assert!(out.contains("command_not_found_handler"));
    }

    #[test]
    fn declared_tool_keys_round_trips_through_a_rendered_rig_cmd_block() {
        let mut config = Config::default();
        config.repo.push(repo_entry("dandavison/delta", None));
        config.repo.push(repo_entry("sharkdp/fd", None));

        let out = render(&config, &Lock::default(), &layout());
        let keys = declared_tool_keys(&out);

        assert_eq!(keys.len(), 2);
        assert!(keys.contains("delta"));
        assert!(keys.contains("fd"));
    }

    /// A plugin never enters `RIG_CMD` — without this, `sync`'s diff report
    /// would call every plugin newly "added" on every single run.
    #[test]
    fn declared_tool_keys_also_round_trips_plugin_markers() {
        let mut config = Config::default();
        config.repo.push(repo_entry("dandavison/delta", None));
        config.plugin.push(PluginEntry {
            name: "zsh-users/zsh-autosuggestions".to_string(),
            description: None,
            source: Some("zsh-autosuggestions.zsh".to_string()),
            run: None,
        });

        let out = render(&config, &Lock::default(), &layout());
        let keys = declared_tool_keys(&out);

        assert_eq!(keys.len(), 2);
        assert!(keys.contains("delta"));
        assert!(keys.contains("zsh-autosuggestions"));
    }

    #[test]
    fn cached_eval_is_inlined_and_live_eval_is_not() {
        let mut config = Config::default();
        config.repo.push(repo_entry(
            "ajeetdsouza/zoxide",
            Some(EvalSpec::Cmd("zoxide init zsh".to_string())),
        ));
        config.repo.push(repo_entry(
            "jdx/mise",
            Some(EvalSpec::Cmd("mise activate zsh".to_string())),
        ));

        let mut lock = Lock::default();
        lock.tool.insert(
            "zoxide".to_string(),
            tool_lock_with_eval(Some(true), Some("_zoxide_hook() { :; }".to_string())),
        );
        lock.tool
            .insert("mise".to_string(), tool_lock_with_eval(Some(false), None));

        let out = render(&config, &lock, &layout());

        assert!(out.contains("# cached: zoxide init zsh"));
        assert!(out.contains("_zoxide_hook"));
        assert!(out.contains("eval \"$(mise activate zsh)\""));
    }

    fn tool_lock_with_eval(cacheable: Option<bool>, cached_output: Option<String>) -> ToolLock {
        ToolLock {
            version: "0.0.0".to_string(),
            source: "github:test/test".to_string(),
            installed_at: OffsetDateTime::UNIX_EPOCH,
            bins: vec![],
            completions: vec![],
            asset: None,
            size: None,
            pkg: None,
            manager: None,
            root: None,
            eval_cacheable: cacheable,
            eval_cached_output: cached_output,
            eval_evidence: vec![],
        }
    }

    #[test]
    fn plugin_source_and_run_are_emitted() {
        let mut config = Config::default();
        config.plugin.push(PluginEntry {
            name: "zsh-users/zsh-autosuggestions".to_string(),
            description: None,
            source: Some("zsh-autosuggestions.zsh".to_string()),
            run: Some(RunSpec::Single("_zsh_autosuggest_start".to_string())),
        });

        let out = render(&config, &Lock::default(), &layout());

        assert!(out.contains(
            "/home/kaige/.local/share/rig/plugins/zsh-autosuggestions/zsh-autosuggestions.zsh"
        ));
        assert!(out.contains("_zsh_autosuggest_start"));
    }

    #[test]
    fn rendered_output_is_syntactically_valid_zsh() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let mut config = Config::default();
        config.repo.push(repo_entry(
            "dandavison/delta",
            Some(EvalSpec::Detailed {
                cmd: "starship init zsh".to_string(),
                cache: Some(false),
            }),
        ));
        config.plugin.push(PluginEntry {
            name: "zsh-users/zsh-autosuggestions".to_string(),
            description: None,
            source: Some("zsh-autosuggestions.zsh".to_string()),
            run: Some(RunSpec::Single(
                "ZSH_AUTOSUGGEST_STRATEGY=(match_prev_cmd history)".to_string(),
            )),
        });
        let mut env = HashMap::new();
        env.insert("ATUIN_TMUX_POPUP".to_string(), "false".to_string());
        config.repo.push(lazy_repo_entry(
            "atuinsh/atuin",
            env,
            Some(EvalSpec::Cmd("atuin init zsh".to_string())),
            Some("^r:_atuin_search_widget"),
        ));
        config.repo.push(RepoEntry {
            name: "casey/just".to_string(),
            host: Host::Github,
            bpick: None,
            extract: None,
            common: Common {
                run: Some(RunSpec::Single(r#"alias js="just""#.to_string())),
                ..config::test_common()
            },
        });

        let mut lock = Lock::default();
        lock.tool.insert(
            "delta".to_string(),
            tool_lock_with_eval(Some(true), Some("alias cat=bat".to_string())),
        );
        lock.tool.insert(
            "atuin".to_string(),
            tool_lock_with_eval(
                Some(true),
                Some(
                    "function __atuin_hook() {\n  local x=\"quoted `cmd` \\$val\"\n}\nprecmd_functions+=(__atuin_hook)"
                        .to_string(),
                ),
            ),
        );

        let out = render(&config, &lock, &layout());

        let mut child = Command::new("zsh")
            .arg("-n")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("zsh should be on PATH");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(out.as_bytes())
            .expect("write to zsh -n");
        let result = child.wait_with_output().expect("zsh -n should run");

        assert!(
            result.status.success(),
            "zsh -n rejected generated init.zsh:\n{}\n--- output ---\n{out}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
