# Changelog

## Unreleased

### Breaking

- **`setup` hooks now run under `sh -e`.** The first failing command fails
  the hook. Previously only the last command's exit status counted, so
  `mv a .; mv b .; rm -rf */` reported success even when both `mv`s failed.
  A hook that relies on a step failing harmlessly needs `|| true` on that
  step.
- **Duplicate tool keys are rejected when the config loads.** Two entries
  that resolve to the same key (e.g. `[[repo]] casey/just` and
  `[[curl]] casey/just`) used to overwrite each other's `rig.lock` record,
  and `rig update just` ran both. Every command now refuses to load such a
  config until one entry is removed.

### Changed

- `rig install` / `rig update` output for parallel runs on a terminal: one
  live progress line instead of interleaved step logs; child process output
  (builds, `git`, install scripts, setup hooks) is captured and shown, last
  20 lines, only under the tool that failed.
- `rig update` summary: names aligned, failures last, a tally line
  (`updated 3 · up to date 40 · failed 1`) with elapsed time; blanket runs
  fold "up to date" tools into the tally, named tools always get a line.
- Update progress lines show both versions: `updating 1.58.0 -> 1.59.0`.
- `[[apt]]` tools run first, one at a time, with the terminal attached so
  `sudo` can prompt.
- `rig update --self` no longer uses the GitHub API, so it can't hit the
  60 requests/hour anonymous limit. It reads the latest tag from the
  release page redirect and checks the archive against its `.sha256` file.

### Added

- `-v` / `--verbose` on `rig install` and `rig update`: stream every step
  and child output live, as before. A single tool, or non-terminal output,
  streams by default.
- GitHub release lookups send a token from `GITHUB_TOKEN`, `GH_TOKEN`, or
  `gh auth token`, raising the API limit from 60 to 5000 requests/hour.
- `rig update --self` and the `rig.zsh` bootstrap support aarch64 Linux and
  x86_64/aarch64 macOS, not just x86_64 Linux.
