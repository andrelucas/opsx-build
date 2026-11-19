#!/usr/bin/env bash
set -euo pipefail
sudo apt-get update
sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates curl
if [ ! -x "$HOME/.local/bin/mise" ]; then
  curl -fsSL https://mise.run -o /tmp/install-mise.sh
  sh /tmp/install-mise.sh
fi
export PATH="$HOME/.local/bin:$PATH"
builder_dir="$HOME/.local/share/campaign-builder"
mkdir -p "$builder_dir" "$HOME/.config/mise" "$HOME/.local/bin"
cp -R . "$builder_dir/"
cp "$builder_dir/mise.toml" "$HOME/.config/mise/config.toml"
mise trust "$HOME/.config/mise/config.toml"
mise bootstrap --yes
rm -rf /tmp/guest /tmp/install-mise.sh
