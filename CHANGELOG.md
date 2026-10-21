# Changelog

## Unreleased

### Changed

- Parallel `rig update` on a terminal prints each tool's result above the
  live counter as it lands, like `rig install`; failures get a one-line
  notice there and their details at the end. The summary no longer repeats
  those lines, and the "not installed by rig (skipped)" list is gone (its
  count stays in the tally).
- Output polish: `--force` on a current version reports `reinstalled 1.2`
  instead of `updated 1.2 -> 1.2` (results, progress and tally alike);
  `install` and `update` result lines (failure notices included) share
  alignment and highlighting; `rig list` aligns descriptions;
  every tool-scoped warning names its tool; the "let rig take over?" prompt
  goes to stderr with its tool name and is no longer appended to or erased
  by the parallel counter; `rig update --self` no longer echoes the new
  build's `--version` output.

## 1.1.0 - 2026-10-21

### Breaking

- **Archives with a single top-level directory are unwrapped.** `[[repo]]`
  tarballs and zips like `bat-v0.26.1-x86_64-unknown-linux-gnu/bat` now land
  as `bat`, the same as `tar --strip-components=1` (`__MACOSX`, `.DS_Store`
  and `Thumbs.db` are ignored and dropped; `.deb` is never unwrapped). Path
  `bin` globs and `setup` paths that named the wrapper (`"bat-*/bat"`,
  `"*/ccs"`, `setcap ... */sing-box`) must drop it. Installed versions keep
  their layout until the next update, and a failing glob leaves the old
  version live.

### Changed

- Installs and updates clean up after themselves. Once the new version is
  recorded in `rig.lock`, every other entry in that tool's `pkg/<tool>/`
  dir (superseded versions, interrupted `.partial`s) is deleted, and the
  downloaded archive is removed from `cache/` right after unpacking. Rolling
  back now means re-downloading. Existing piles shrink on each tool's next
  update; `cache/` leftovers from older rigs can be deleted by hand.

## 1.0.5 - 2026-10-19

### Changed

- `rig update --self` no longer uses the GitHub API, so it can't hit the
  60 requests/hour anonymous limit. It reads the latest tag from the
  release page redirect and checks the archive against its `.sha256` file.

### Added

- GitHub release lookups send a token from `GITHUB_TOKEN`, `GH_TOKEN`, or
  `gh auth token`, raising the API limit from 60 to 5000 requests/hour.
- `rig update --self` and the `rig.zsh` bootstrap support aarch64 Linux and
  x86_64/aarch64 macOS, not just x86_64 Linux.

## 1.0.2 - 2026-10-18

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

### Added

- `-v` / `--verbose` on `rig install` and `rig update`: stream every step
  and child output live, as before. A single tool, or non-terminal output,
  streams by default.
