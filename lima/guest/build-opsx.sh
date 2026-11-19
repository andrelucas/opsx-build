#!/usr/bin/env bash
# Only rebuild when the host supplies a fresh source archive.
set -euo pipefail
if [ ! -f /tmp/opsx-build.tar ]; then exit 0; fi
source_dir=$(mktemp -d)
trap 'rm -rf "$source_dir"' EXIT
tar -xf /tmp/opsx-build.tar -C "$source_dir"
cargo install --locked --path "$source_dir" --root "$HOME/.local"
rm /tmp/opsx-build.tar
