#!/usr/bin/env bash
# Run on Ludwig. Argument: a source archive made with `git archive HEAD`.
set -euo pipefail
export PATH="$HOME/.local/bin:$PATH"
builder_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
opsx_archive=${1:?Usage: bash create-base.sh /path/to/opsx-build.tar}
shift
limactl create --yes --name=campaign-base "$@" "$builder_dir/lima.yaml"
limactl start --yes campaign-base
limactl copy --backend=scp -r "$builder_dir/guest" campaign-base:/tmp/
limactl copy --backend=scp "$opsx_archive" campaign-base:/tmp/opsx-build.tar
limactl shell --workdir=/tmp/guest campaign-base bash bootstrap.sh
# A fresh login after the restart picks up Docker group membership.
limactl stop campaign-base
limactl start --yes campaign-base
limactl shell --workdir=/tmp campaign-base bash -lc 'mise exec -- builder-check'
limactl stop campaign-base
limactl snapshot create campaign-base --tag prepared
