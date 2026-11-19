#!/usr/bin/env bash
# Build once in Ubuntu; subsequent bootstraps and clones reuse the installation.
set -euo pipefail
prefix="$HOME/.local/share/tlaps"
if [ -f "$prefix/.built" ]; then exit 0; fi
source_dir="$HOME/.cache/tlaps-build"
if [ ! -d "$source_dir/.git" ]; then
  git clone --depth=1 https://github.com/tlaplus/tlapm.git "$source_dir"
fi
export OPAMYES=1 OPAMJOBS
OPAMJOBS=$(nproc)
if [ ! -f "$HOME/.opam/config" ]; then
  opam init --bare --disable-sandboxing --no-setup --yes
fi
if ! opam switch list --short | rg -qx tlaps; then
  opam switch create tlaps ocaml-base-compiler --yes
fi
eval "$(opam env --switch=tlaps --set-switch)"
cd "$source_dir"
make opam-deps opam-deps-opt
make
dune install --relocatable --prefix "$prefix"
for tool in tlapm tlapm_lsp; do ln -sf "$prefix/bin/$tool" "$HOME/.local/bin/$tool"; done
git rev-parse HEAD >"$prefix/.built"
