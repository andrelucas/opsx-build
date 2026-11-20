#!/usr/bin/env bash
set -euo pipefail
export PATH="$HOME/.local/bin:$PATH"
usage='Usage: bash prepare-profile.sh STOPPED_BASE PROFILE.toml BUILDER_NAME'
base=${1:?"$usage"}
profile=${2:?"$usage"}
builder=${3:?"$usage"}
test -f "$profile" || { printf 'Profile not found: %s\n' "$profile" >&2; exit 1; }

limactl clone --tty=false "$base" "$builder"
trap 'limactl stop "$builder" || true' EXIT
limactl start --tty=false "$builder"
limactl copy --backend=scp "$profile" "$builder:/tmp/builder-profile.toml"
limactl shell --workdir=/tmp "$builder" bash -c '
  set -euo pipefail
  export PATH="$HOME/.local/bin:$PATH"
  mv /tmp/builder-profile.toml "$HOME/.config/mise/config.local.toml"
  mise bootstrap --yes --only packages,tools
  mise exec -- builder-check
'
limactl stop "$builder"
trap - EXIT
limactl snapshot create "$builder" --tag prepared
