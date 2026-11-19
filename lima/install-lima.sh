#!/usr/bin/env bash
# One-time user-local installation on Ludwig; QEMU/KVM is already installed.
set -euo pipefail
export PATH="$HOME/.local/bin:$PATH"
if ! command -v limactl >/dev/null; then
  scratch=$(mktemp -d)
  trap 'rm -rf "$scratch"' EXIT
  url=$(curl -fsSL https://api.github.com/repos/lima-vm/lima/releases/latest |
    python3 -c 'import json,sys; print(next(a["browser_download_url"] for a in json.load(sys.stdin)["assets"] if a["name"].startswith("lima-") and a["name"].endswith("-Linux-x86_64.tar.gz")))')
  curl -fsSL "$url" -o "$scratch/lima.tar.gz"
  mkdir -p "$HOME/.local"
  tar -xzf "$scratch/lima.tar.gz" -C "$HOME/.local"
fi
if ! command -v qemu-system-x86_64 >/dev/null && [ -x /usr/libexec/qemu-kvm ]; then
  ln -s /usr/libexec/qemu-kvm "$HOME/.local/bin/qemu-system-x86_64"
fi
limactl --version
