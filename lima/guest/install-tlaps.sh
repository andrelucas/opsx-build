#!/usr/bin/env bash
set -euo pipefail
brew=/home/linuxbrew/.linuxbrew/bin/brew
formula=andrelucas/tlapm/tlapm
export HOMEBREW_NO_ANALYTICS=1 HOMEBREW_NO_AUTO_UPDATE=1
if [ ! -x "$brew" ]; then
  installer=$(mktemp)
  trap 'rm -f "$installer"' EXIT
  curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh -o "$installer"
  NONINTERACTIVE=1 bash "$installer"
fi

if [ ! -x /home/linuxbrew/.linuxbrew/bin/tlapm ]; then
  "$brew" tap andrelucas/tlapm
  "$brew" trust --formula "$formula"
  bottle=$("$brew" --cache --force-bottle "$formula")
  case "$bottle" in
    *.bottle*.tar.gz) ;;
    *) printf 'No TLAPS bottle available for this guest\n' >&2; exit 1 ;;
  esac
  "$brew" fetch --force-bottle "$formula"
  "$brew" install "$bottle"
fi
ln -sf /home/linuxbrew/.linuxbrew/bin/tlapm "$HOME/.local/bin/tlapm"
