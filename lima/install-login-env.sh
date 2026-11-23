#!/usr/bin/env bash
set -eu
umask 077
dir="$HOME/.config/opsx-build"
mkdir -p "$dir"
tmp=$(mktemp "$dir/.lima-env.XXXXXXXX")
trap 'rm -f "$tmp"' EXIT
cat > "$tmp"
mv -f "$tmp" "$dir/lima-env.sh"

# Bash reads only the first existing login profile.
profile="$HOME/.profile"
for candidate in "$HOME/.bash_profile" "$HOME/.bash_login"; do
  if [[ -r "$candidate" ]]; then
    profile=$candidate
    break
  fi
done
source_line='. "$HOME/.config/opsx-build/lima-env.sh"'
grep -Fxq "$source_line" "$profile" 2>/dev/null || printf '\n%s\n' "$source_line" >> "$profile"
