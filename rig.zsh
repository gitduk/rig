# rig bootstrap — https://github.com/gitduk/rig
#
# .zshrc only needs:
#   [[ -f ~/.local/share/rig/rig.zsh ]] || curl -fsSL -o ~/.local/share/rig/rig.zsh \
#     https://github.com/gitduk/rig/releases/latest/download/rig.zsh
#   source ~/.local/share/rig/rig.zsh

RIG_BIN=~/.local/bin/rig
if [[ ! -x $RIG_BIN ]]; then
  echo "rig not found, downloading latest release..."
  mkdir -p ${RIG_BIN:h}
  curl -fsSL -o $RIG_BIN \
    https://github.com/gitduk/rig/releases/latest/download/rig-x86_64-unknown-linux-gnu \
    && chmod +x $RIG_BIN
fi

_rig=~/.local/share/rig/init.zsh
[[ ! -f $_rig || ~/.config/rig/config.toml -nt $_rig || $commands[rig] -nt $_rig ]] && rig sync &>/dev/null
source $_rig
