# Lima + mise campaign builder

An Ubuntu 24.04 LTS x86_64 builder on Ludwig, with a prepared base VM that can
be cloned for Sirocco and other opsx-build campaigns. The base has no campaign
checkout, agent credentials or host filesystem mounts. Linux files and Docker
storage live on the VM disk.

`lima.yaml` describes the VM. `guest/mise.toml` describes its tools and Ubuntu
packages. The short scripts install Lima, prepare the base, and build opsx-build
from a supplied source archive. Most tools use latest/stable releases. The exact
Go, Buf, generator and TLC versions are required by existing Sirocco checks;
PD and TiKV use 8.5.8. Installed tools are retained in the prepared image.
TLAPS is built once from upstream source inside Ubuntu, using its supported
opam/Make build. This avoids the newer glibc required by the current upstream
binary. The installed proof system is then reused by snapshots and clones.

## Prepare the base on Ludwig

From the opsx-build checkout on Maria, copy the builder scripts and a source
archive to Ludwig.

```sh
rsync -a lima/ ludwig:~/campaign-builder/
git archive --output=/tmp/opsx-build.tar HEAD
scp /tmp/opsx-build.tar ludwig:~/campaign-builder/opsx-build.tar
```

On Ludwig:

```sh
cd ~/campaign-builder
bash install-lima.sh
bash create-base.sh "$PWD/opsx-build.tar" --cpus=8 --memory=16 --disk=100
```

Memory and disk are in GiB. The defaults are 8 CPUs, 16 GiB RAM and a 100 GiB
sparse virtual disk, declared in `lima.yaml`. The script forwards any resource
flags to Lima. It refuses an existing `campaign-base`; it leaves
the prepared base stopped and creates a `prepared` snapshot. Resource flags
also work when cloning, so each campaign can have different allocations.

Ludwig's existing QEMU/KVM is reused. Lima is installed under `~/.local`; its
instances and caches stay in the user's home filesystem. Existing host Docker,
Podman and Kubernetes stores are not used by the guest.

## Start a campaign clone

On Ludwig:

```sh
export PATH="$HOME/.local/bin:$PATH"
limactl clone campaign-base campaign-demo --cpus=8 --memory=16 --start
limactl shell campaign-demo
```

Inside the VM, install agent authentication and campaign-specific opsx-build
connection settings. Do that in the clone so the base remains reusable. Copy
an already prepared campaign with `limactl copy` from Ludwig; keep its Git
metadata and the relative locations of any declared dependency inputs.

For a prepared directory named `my-campaign` on Ludwig:

```sh
limactl copy --backend=rsync -r /path/to/my-campaign 'campaign-demo:~/campaigns/'
limactl shell --workdir=/home/andre.guest/campaigns/my-campaign campaign-demo
```

Use the rsync backend for campaigns: it preserves Git-tracked symlinks, including
OpenSpec's skill links. SCP recursively follows those links and changes the copy.

Run from the campaign directory:

```sh
opsx-campaign preflight
opsx-campaign execute
```

`opsx-campaign` invokes nono inside the guest and turns off the inner agent
harness sandbox, as the Amygdala launcher does. Its profile grants broad access
to the campaign, tools and guest OS, while keeping nono's own state private.
The shared Lima builder profile uses nono's `developer` network profile, adds
`proxy.golang.org` for public Go modules, and provides private loopback for
local tests. A small AppArmor allowance enables this on Ubuntu. Adjust
`~/.local/share/campaign-builder/network.json` for additional model endpoints,
registries or a campaign's Kubernetes API. Provisioning is done before this
campaign policy is applied. Cluster credentials and cluster creation are
separate from installing Docker, kind and kubectl; the base creates no cluster.
The nono policy applies to the campaign process tree; container networking is
managed by Docker.

Stop a clone with `limactl stop campaign-demo`. Start it again with
`limactl start campaign-demo`; the installed tools and campaign work remain.

## Iterate on the base

Start `campaign-base`, copy changed files from `guest/` into
`~/.local/share/campaign-builder/`, and copy the updated `mise.toml` to
`~/.config/mise/config.toml`. Run `mise bootstrap` inside the guest to install
what is missing. Use `mise bootstrap --upgrade` when you deliberately want to
refresh tools that follow latest/stable. Stop the base before cloning it.

To rebuild opsx-build, copy a fresh `git archive` to `/tmp/opsx-build.tar` in
the guest and run:

```sh
mise exec -- bash ~/.local/share/campaign-builder/build-opsx.sh
```

The helper removes the supplied source and temporary compilation directory
after installation. It installs the executable under `~/.local/bin`.

TLAPS has a separate short `build-tlaps.sh` helper. Its upstream source checkout
and opam environment are retained for incremental builds; the installed proof
system lives under `~/.local/share/tlaps`. The helper skips an existing
installation, so refreshing TLAPS source is an explicit base-image update.
Opam installs the OCaml compiler and libraries during this initial build;
clones reuse the compiled proof system.

## Software included

The campaign tools were inventoried on Amygdala on 19 November 2026:

- Claude Code, Codex, OpenCode, OpenSpec, opsx-build and nono.
- Rust/Cargo, Node/npm, Python with Boto3, PyYAML, psutil and requests, Java.
- Go, Buf, protoc, both Go generators, gopls, gotestsum and Dapper.
- TLC, SANY, PlusCal, TLA+ XML/LaTeX tools and TLAPS with its backend provers.
- PD/TiKV 8.5.8, Docker, kind and kubectl.
- OCaml/opam, C/C++ build tools, Clang/LLD, CMake, Ninja, ccache, native development
  libraries, Git, rsync, jq, ripgrep, tmux and debugging/network utilities.

Ubuntu supplies the native packages. Mise manages the language runtimes and
downloaded CLIs. The manifest is the maintained software list; this is a first
iteration for refinement with actual campaigns.

## First validation on Ludwig

On 19 November 2026, the prepared base was stopped, snapshotted as `prepared`,
and cloned as `campaign-check` with 4 CPUs and 8 GiB RAM. The clone retained
its installed tools and passed `builder-check`. Further checks passed:

- TLC model checking.
- Source-built TLAPS proofs using SMT, Zenon, Isabelle and PTL under nono,
  plus strict rejection of an invalid theorem.
- PD/TiKV 8.5.8 startup and store registration under the campaign nono policy.
- Local TCP, an allowed GitHub request, a blocked unlisted domain, and blocked
  direct external TCP under the same nono policy.
- Docker container execution, Docker API access under nono, and a temporary
  kind cluster whose node became Ready. The temporary cluster was removed.
- Campaign-directory copying into the clone with `limactl copy`.
- Normal NTP synchronization with chrony.

Agent authentication and campaign-specific connections are still supplied in
each clone. No campaign was launched as part of these installation checks.

## Real Sirocco campaign trial

On 19 November 2026, `sirocco` was cloned on Ludwig with 8 CPUs, 16 GiB RAM
and a 100 GiB virtual disk. It contains a copy of
`kv-lifetime-owner-continuity-20261118` at commit `4fc4f2c`, under
`/home/andre.guest/campaigns/`. Amygdala's named agent connections and Claude
sign-in were copied into this clone. The base remains stopped and unchanged.

Standalone `opsx-campaign preflight` returned READY under the normal nono
policy, running from 16:54:09 to 16:54:26 GMT. The campaign working tree and HEAD
remained unchanged. The saved report is `.git/opsx-build/contract-preflight.json`
inside the campaign.

The maintained opsx-build `nono/preflight.py` environment checks also passed,
from 16:55:10 to 16:55:22 GMT: Go and generators, real gopls diagnostics, Git,
OpenSpec discovery, resolved configuration, module-selected compiler/cache
access, and agentic tool round trips with both Sonnet and Opus. The checker's
outer launcher uses `nono wrap`, which cannot create this profile's network
namespaces. Its unchanged checks were run through `nono run` instead:

```sh
# From the campaign directory in the guest; the staged script is in /tmp.
mise exec -- nono run \
  --profile "$HOME/.local/share/campaign-builder/network.json" --allow-cwd -- \
  python /tmp/environment-preflight.py --inside --campaign "$PWD" \
  --connection anthropic --connection anthropic-frontier --timeout 120
```

Host logs are `~/campaign-builder/sirocco-preflight.log` and
`~/campaign-builder/sirocco-environment-preflight.log` on Ludwig. The clone is
left running. No delivery campaign or final acceptance suite was launched.
