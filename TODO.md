# TODO

## 1. `[[curl]]` 不固定安装目录，direnv 被装进 fnm 的 node 安装目录（核心 bug）

- `[[curl]]` 让 install.sh 自己决定装哪：它扫 PATH，挑**第一个可写目录**（`install.sh` 的 `for path in $(echo "$PATH" ...); if [[ -w $path ]]` 循环）
- fnm 的 multishell 目录（`~/.local/state/fnm_multishells/<pid>_<ts>` 与 `/run/user/1000/fnm_multishells/<pid>_<ts>`）是**符号链接** → node 安装树（`~/.local/share/fnm/node-versions/<v>/installation`），且位于 `fnm env` 求值后的 PATH[1]，可写 → 被选中
- 后果：
  - 二进制落在 node 安装树里，fnm 重装/清理 node 版本即被连带删除
  - rig 记录的 bin 是**终端 PID 相关的 multishell 路径**，终端一关链接就没了，记录悬空
- 现有缓解：direnv 条目加 `env = { bin_path = "~/.local/bin" }`，install.sh 认 `bin_path`，跳过 PATH 扫描
- 待办：让 `[[curl]]` 固定到 prefix 的 bin 目录（或拒绝 node 安装树这类外来位置），而不是"装哪算哪 + 事后 `command -v` 记录"
- 已修复：`src/sources/curl.rs` 装脚本前把 `prefix_bin_dir` 塞进子进程 `PATH` 最前面（`pinned_path`），解析 bin 时先查该目录（`resolve_declared_bins_pinned`），查不到才退回 `command -v` 并打印警告

## 2. eval cache 之后如何刷新 cache

- 触发场景：`direnv hook zsh` 的输出**内嵌了 direnv 被解析到的绝对路径**；rig 把捕获输出原样存进 `eval_cached_output`，`init.zsh` 原样内联（`src/initzsh.rs`）
- 二进制被删除/移动后，缓存里的绝对路径悬空 → 每次提示符报 `_direnv_hook:2: no such file or directory: .../direnv`
- 注意：`rig sync` **不能**修复——它只是把 lock 里旧的 `eval_cached_output` 重新内联，必须重装（re-capture）才会重跑 eval 命令、重新嵌入当前路径
- 待办：提供显式刷新 eval cache 的命令/机制（`rig sync --refresh-eval`？），或对"hook 内嵌绝对路径"的工具走 live eval（不缓存）
- 已修复：`rig sync --force`（`src/update.rs::refresh_eval_cache`,经 `update::update_all(..., refresh_eval=true, filter=None)` 对所有工具批量跑）——只重跑 `resolve_eval_cache` 原地覆盖 `eval_cacheable`/`eval_cached_output`/`eval_evidence`，不碰 version/bins；`doctor.rs` 的 drift 提示也改成指向这条命令。最初实现是 `rig update <tool> --refresh-eval`，后来按用户要求改成复用 `sync` 现有的 `--force`,不额外加新参数
