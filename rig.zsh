# rig bootstrap — https://github.com/gitduk/rig
#
# .zshrc only needs:
#   _rig_zsh=$HOME/.local/share/rig/rig.zsh
#   [[ -f $_rig_zsh ]] || {
#     mkdir -p ${_rig_zsh:h}
#     curl -fsSL -o $_rig_zsh \
#       https://raw.githubusercontent.com/gitduk/rig/master/rig.zsh
#   }
#   source $_rig_zsh

RIG_BIN=~/.local/bin/rig
if [[ ! -x $RIG_BIN ]]; then
  case $(uname -sm) in
    'Linux x86_64')  _rig_target=x86_64-unknown-linux-gnu ;;
    'Linux aarch64') _rig_target=aarch64-unknown-linux-gnu ;;
    'Darwin x86_64') _rig_target=x86_64-apple-darwin ;;
    'Darwin arm64')  _rig_target=aarch64-apple-darwin ;;
    *) _rig_target= ;;
  esac
  if [[ -z $_rig_target ]]; then
    echo "rig: no release build for $(uname -sm); install it with cargo" >&2
  else
    echo "rig not found, downloading latest release..."
    mkdir -p ${RIG_BIN:h}
    curl -fsSL -o ${RIG_BIN}.tar.gz \
      https://github.com/gitduk/rig/releases/latest/download/rig-${_rig_target}.tar.gz
    tar -xzf ${RIG_BIN}.tar.gz -C ${RIG_BIN:h} --strip-components=1
    rm -f ${RIG_BIN}.tar.gz
    chmod +x $RIG_BIN
  fi
  unset _rig_target
fi

# Unconditional sync: the bootstrap stays a thin shell that never needs
# updating, so every version-dependent decision (config path, staleness)
# lives in the rig binary. `atomic_write` is idempotent, so a no-op sync
# costs a few ms of fork, not an init.zsh rewrite.
[[ -x $RIG_BIN ]] && rig sync &>/dev/null
[[ -f ~/.local/share/rig/init.zsh ]] && source ~/.local/share/rig/init.zsh
