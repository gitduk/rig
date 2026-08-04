//! Generates `init.zsh` — fpath, env, `RIG_CMD` + `command_not_found`,
//! cached/live `eval` blocks, and the plugin load queue. Pure text
//! assembly; writing the file is the caller's job.

use std::collections::HashSet;

use crate::config::{self, BinSpec, Common, Config, tool_key};
use crate::lock::Lock;
use crate::paths::Layout;

/// Reads a previously generated `init.zsh`'s `RIG_CMD` block back out —
/// lets `rig sync` diff old vs new declared tools without a config snapshot.
pub fn declared_tool_keys(init_zsh: &str) -> HashSet<String> {
    init_zsh
        .lines()
        .skip_while(|line| line.trim() != "RIG_CMD=(")
        .skip(1)
        .take_while(|line| line.trim() != ")")
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(str::to_string)
        .collect()
}

pub fn render(config: &Config, lock: &Lock, layout: &Layout) -> String {
    let entries: Vec<(String, &Common)> = config::all_entries(config)
        .into_iter()
        .map(|e| (e.key(), e.common()))
        .collect();

    let mut out = String::new();
    render_fpath(&mut out, layout);
    render_env(&mut out, &entries);
    render_rig_cmd_and_handler(&mut out, config, layout);
    render_eval_blocks(&mut out, &entries, lock);
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
        let key = entry.key();
        // The lock key stays scope-prefixed, but a real command is never
        // typed with the `@scope/` prefix — guess by last path segment.
        let default = tool_key(entry.name());
        for cmd in BinSpec::declared_names(entry.common().bin.as_ref(), default) {
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
        let cmd = eval.command();

        match tool_lock.eval_cacheable {
            Some(true) => {
                if let Some(cached) = &tool_lock.eval_cached_output {
                    out.push_str(&format!(
                        "# cached: {cmd} (evidence: none — probed clean)\n{cached}\n\n"
                    ));
                } else {
                    out.push_str(&format!("eval \"$({cmd})\"\n\n"));
                }
            }
            Some(false) | None => {
                out.push_str(&format!("eval \"$({cmd})\"\n\n"));
            }
        }
    }
}

/// Plugins are sourced in config order — no defer mechanism exists yet
/// (measured it wasn't worth building).
fn render_plugins(out: &mut String, config: &Config, layout: &Layout) {
    for plugin in &config.plugin {
        let key = tool_key(&plugin.name);
        let clone_dir = layout.plugins_dir.join(key);
        if let Some(source) = &plugin.source {
            out.push_str(&format!("source {}\n", clone_dir.join(source).display()));
        }
        if let Some(atload) = &plugin.atload {
            out.push_str(atload);
            out.push('\n');
        }
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CompletionsSpec, EvalSpec, Host, PluginEntry, RepoEntry};
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
                description: None,
                bin: None,
                env: HashMap::new(),
                eval,
                completions: CompletionsSpec::Enabled(true),
                setup: None,
            },
        }
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
    fn plugin_source_and_atload_are_emitted() {
        let mut config = Config::default();
        config.plugin.push(PluginEntry {
            name: "zsh-users/zsh-autosuggestions".to_string(),
            source: Some("zsh-autosuggestions.zsh".to_string()),
            defer: 1,
            atload: Some("_zsh_autosuggest_start".to_string()),
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
            source: Some("zsh-autosuggestions.zsh".to_string()),
            defer: 1,
            atload: Some("ZSH_AUTOSUGGEST_STRATEGY=(match_prev_cmd history)".to_string()),
        });

        let mut lock = Lock::default();
        lock.tool.insert(
            "delta".to_string(),
            tool_lock_with_eval(Some(true), Some("alias cat=bat".to_string())),
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
