# rig

> Tool installer + zsh loader, driven by a single TOML config.

rig installs CLI tools from a handful of sources and wires them into your
shell: PATH symlinks, completion `fpath`, environment variables, `eval`
output (cached when safe), lazy loading, and zsh plugins — all described in
one `~/.rig.toml`. Type an unconfigured command and rig installs the tool
on the spot via `command_not_found_handler`.

## Highlights

- **Seven source types** — GitHub/Codeberg release binaries, git source
  builds, `cargo install`, node packages (bun-first, npm fallback), apt
  packages, curl install scripts, and zsh plugins.
- **Config-driven** — no per-tool shell snippets to maintain; `init.zsh` is
  generated from `~/.rig.toml` by `rig sync`.
- **Self-healing** — a `command_not_found_handler` installs the right tool
  (or repairs a dangling install) the first time you use it, then reruns
  the command.
- **Eval cache** — rig probes an `eval` command for cacheability (does its
  output depend on cwd, `TERM`, `PATH`, `SHLVL`?) and inlines stable output
  into `init.zsh` for fast startup; unstable evals stay live.
- **Lazy loading** — defer a tool's `env`/`eval`/`run` until first use,
  optionally bound to a ZLE widget key.
- **Atomic updates** — each version lives in its own `pkg/<tool>/<version>/`
  dir; symlinks repoint only after the new build succeeds, and `rig.lock`
  writes are temp-file + rename.
- **Parallel installs** — `settings.parallel` workers with a whole-command
  advisory lock.
- **`rig doctor`** — surfaces config/shell drift (PATH ordering, dangling
  symlinks, shadowed completions, eval cache drift, and more).

## Installation

Add this to `~/.zshrc`:

```zsh
_rig_zsh=$HOME/.local/share/rig/rig.zsh
[[ -f $_rig_zsh ]] || {
  mkdir -p ${_rig_zsh:h}
  curl -fsSL -o $_rig_zsh \
    https://github.com/gitduk/rig/releases/latest/download/rig.zsh
}
source $_rig_zsh
```

On first login the bootstrap downloads the `rig` binary, runs `rig sync`,
and generates `init.zsh`. Subsequent shells only re-sync when the config
or binary is newer.

Prefer building from source (edition 2024, needs a recent Rust toolchain):

```sh
cargo install --path .
```

## Configuration

Everything lives in `~/.rig.toml`:

```toml
[settings]
prefix = "~/.local"   # install root; bins go to <prefix>/bin
parallel = 8          # max concurrent install/update workers

[[repo]]
name = "dandavison/delta"            # GitHub release binary
bpick = "*.tar.gz"
bin = { "delta" = "delta" }

[[git]]
name = "explosion-mental/wallust"    # source build
host = "codeberg"
setup = "cargo +nightly build --release"
bin = { "target/release/wallust" = "wallust" }

[[cargo]]
name = "eza-community/eza"           # cargo install, version from crates.io

[[node]]
name = "@openai/codex"               # bun, or npm via manager = "npm"
manager = "bun"

[[apt]]
name = "ripgrep"                     # system package; updates via apt
bin = "rg"

[[curl]]
name = "casey/just"                  # install script fetched and run
url = "https://just.systems/install.sh"
run = 'alias js="just"'

[[plugin]]
name = "zsh-users/zsh-autosuggestions"  # cloned; zsh sources it
source = "zsh-autosuggestions.zsh"
run = "_zsh_autosuggest_start"
```

### Sources

| Source | What rig does | Version source |
| --- | --- | --- |
| `[[repo]]` | Picks a release asset (explicit `bpick` glob or host-arch/os auto-match), downloads, extracts, symlinks bins | GitHub/Codeberg release |
| `[[git]]` | Clones, runs `setup` (build), symlinks bins | git HEAD |
| `[[cargo]]` | `cargo install` | crates.io |
| `[[node]]` | `bun add -g` / `npm i -g` (`manager = auto` probes bun first) | npm registry |
| `[[apt]]` | `apt-get install`; rig records the dpkg-reported version and where the bin landed | no pre-check — `rig update` reruns `apt-get install`, reports up to date when the version didn't change |
| `[[curl]]` | Fetches `url` and runs it as a script | none — rerun every `rig update` |
| `[[plugin]]` | Clones to `~/.local/share/rig/plugins/`; zsh `source`s `source` and runs `run` right after | git pull |

### Shared fields

`[[repo]]`, `[[git]]`, `[[cargo]]`, `[[node]]`, `[[apt]]`, and `[[curl]]`
share these fields (plus their own: `host`/`bpick`/`extract` on `[[repo]]`,
`manager` on `[[node]]`, `url` on `[[curl]]`):

| Field | Description |
| --- | --- |
| `description` | Shown by `rig list`. |
| `bin` | Command name(s) to expose. `"name"`, a list, or a map of path-glob → command. Defaults to the last path segment of `name`. |
| `env` | Environment variables exported in `init.zsh` (or at first use with `lazy`). Leading `~` is expanded and the rest escaped. |
| `eval` | Shell to run at startup, e.g. `"zoxide init zsh"`. Plain string, or `{ cmd, cache }` to force cacheability. |
| `run` | Plain shell text (aliases, etc.) run verbatim, never captured — single line, list, or a `'''...'''` block. |
| `completions` | `true` (default: link `_*` files from the install), `false`, `{ path = "..." }` (link one file), or `{ generate = "cmd" }` (run a generator in the install dir). |
| `setup` | Hook run during install (and update, unless `{ install, update }`). Used for builds. `sudo` here prints a warning — it writes outside rig's control. |
| `lazy` | Defer `env`/`eval`/`run` to first use of the tool's command name. |
| `bind` | With `lazy`: `"key:widget"` — a placeholder ZLE widget runs the deferred setup, then rebinds the key to the real widget. |

## Commands

| Command | Alias | Description |
| --- | --- | --- |
| `rig install <tool...>` | `i` | Install tools (folds in an update check for already-installed ones). |
| `rig update [tool...]` | `u` | Update everything, or just the named tools. `--force` reinstalls. |
| `rig remove <tool...>` | `rm` | Remove a tool's rig-owned files and lock entry. |
| `rig list` | `ls` | Configured tools, marked installed/not. |
| `rig sync [--force]` | `s` | Regenerate `init.zsh` from the config; clones missing plugins. `--force` re-probes every tool's eval cacheability. |
| `rig doctor` | `d` | Diagnostic checks against config, lock, and shell. |
| `rig which <command>` | `w` | Resolve a command name to the tool that provides it. |
| `rig completions <shell>` | `c` | Print a completion script for rig itself. |

Tools can be addressed by lock key, full name (`dandavison/delta`), or any
declared command name (`rg`), so `rig install rg` and `rig update delta`
just work.

## How it works

- **State** lives in `~/.local/share/rig/`: `rig.lock` (installed-tool
  state, TOML), `init.zsh` (generated, sourced by the bootstrap),
  `completions/` (linked `_*` files, added to `fpath`), `pkg/<tool>/<ver>/`
  (one dir per version, swapped atomically), and `plugins/`.
- **`rig.lock`** records version, source, bins, completions, package
  location, and the eval-cache verdict per tool. Writes go through a
  temp-file + rename with a one-generation `.rig.lock` backup, and land
  before `init.zsh` is regenerated — a killed process never loses finished
  work.
- **Writers serialize** on an advisory flock (`rig.lock.lock`) held for the
  whole command; shell-startup `rig sync` uses a non-blocking variant so it
  bows out instead of hanging behind a slow install.
- **Eval caching** probes a command's output for dependence on cwd, `TERM`,
  `PATH`, and `SHLVL`. Cacheable output is inlined into `init.zsh` with the
  probe evidence as a comment; otherwise the eval stays a live call.
  `rig sync --force` re-probes in place, and `rig doctor` flags drift.
- **`command_not_found_handler`** looks the command up in the generated
  `RIG_CMD` map (covering not-yet-installed tools), runs `rig install`, re-
  sources `init.zsh`, `rehash`, and reruns the command.

## Development

```sh
cargo build --release
cargo test
```

The test suite renders `init.zsh` from synthetic configs and validates it
with `zsh -n`. Releases are cut by the `release` GitHub Action: the version
in `Cargo.toml` is the single source of truth, and each bump auto-tags and
publishes the `rig` binary plus `rig.zsh`.
