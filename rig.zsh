# rig bootstrap — https://github.com/gitduk/rig
#
# .zshrc only needs:
#   _rig_zsh=$HOME/.local/share/rig/rig.zsh
#   [[ -f $_rig_zsh ]] || {
#     mkdir -p ${_rig_zsh:h}
#     curl -fsSL -o $_rig_zsh \
#       https://github.com/gitduk/rig/releases/latest/download/rig.zsh
#   }
#   source $_rig_zsh

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
