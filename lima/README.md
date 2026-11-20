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

## Run a campaign with opsx-build

Install the [opsx-build host binary](../README.md#installation) on the machine
that runs Lima, and make sure `limactl` is on `PATH`. From a standalone campaign
checkout on that host, select a stopped, prepared base.
Many campaigns can share the same base and each gets its own writable clone.
The launcher inherits the base's architecture and uses its installed `mise`
and `opsx-campaign` wrapper. The scripts above prepare the x86_64 base on
Ludwig. For Apple Silicon, the launcher can use a separately prepared arm64
Ubuntu base with the tools that campaign needs.

```sh
opsx-build lima prepare --base campaign-base
```

To set resources per campaign, put this optional file in the campaign root
as `opsx-build.lima.toml`. Memory and disk are in GiB.

```toml
cpus = 8
memory = 16
disk = 100
```

Options `--cpus`, `--memory`, and `--disk` on `lima prepare` override the file.
Omitted values inherit the base settings. Use `--repo PATH` with any Lima
command to select a campaign from elsewhere.

Prepare copies the campaign, including its Git history and any bundled
dependency inputs. For Claude, configure [host token forwarding](#use-a-claude-subscription-on-the-builder)
before preparing the clone. For Codex, [sign in to the prepared clone](#use-chatgpt-pro-with-codex).
From the host campaign checkout, run the prerequisite check and start delivery.

```sh
opsx-build lima run -- preflight
opsx-build lima run -- execute
```

If you ran `bootstrap` separately and it completed, start delivery with
`opsx-build lima run -- --loop advance`. Pass the same supplied-contract
options to both commands if they are absent from `opsx-build.md`. To continue
an interrupted run, use `opsx-build lima run -- --resume`.

`lima run` and `lima export` start the recorded clone if needed. The guest
retains its checkout, agent sign-in and checkpoint between runs. The host
records which clone to use under `$XDG_STATE_HOME/opsx-build/lima` (by default
`~/.local/state/opsx-build/lima`), keyed to the host checkout's absolute path.
Use that same host checkout to resume or export.

When a guest command finishes, copy back a reviewable checkpoint with
`opsx-build lima export /path/to/new-directory`. The export includes Git
metadata, checkpoints and ignored artifacts. For a completed run with an
independent acceptance gate, the exported `.git/opsx-build/last-run.json` has
`stage` set to `done`. The passing `result.json` and logs are under
`.git/opsx-build/acceptance/`. If the transfer fails, retry with a fresh
destination. To release VM resources, run `limactl stop <clone-name>` after
export or between runs.
Lima keeps the guest disk for the next run.

### Use a Claude subscription on the builder

Run `claude setup-token` on the Lima host and save the token as a single line
in `~/.config/opsx-build/claude-token` with permissions `0600`. Use
`$XDG_CONFIG_HOME/opsx-build/` if the host sets `XDG_CONFIG_HOME`.

In the same host directory, put the guest's opsx-build configuration in
`lima-claude.toml`. Put the token reference on each connection so existing
campaigns can keep their recorded shared environment setting. For example,
these two profiles use Sonnet for delivery and Opus for frontier work.

```toml
[connections.anthropic]
model = "sonnet"

[connections.anthropic.env]
CLAUDE_CODE_OAUTH_TOKEN = { from_env = "CLAUDE_CODE_OAUTH_TOKEN" }

[connections.anthropic-frontier]
model = "opus"

[connections.anthropic-frontier.env]
CLAUDE_CODE_OAUTH_TOKEN = { from_env = "CLAUDE_CODE_OAUTH_TOKEN" }
```

Select those connection names in the campaign and set `claude_subscription = true`
in its `opsx-build.lima.toml` before `lima prepare`. Preparation copies the
connection configuration into the guest's builder directory. On each unattended
`lima run`, including `--resume`, the launcher reads the current host token and
sends it to the guest over SSH stdin. The token stays out of the source archive,
clone configuration and campaign export. Use a fresh clone to pick up changes
to `lima-claude.toml`. On Ludwig, both connections used the forwarded token
on a fresh clone, including after a stop and resume, without a guest sign-in.

### Use ChatGPT Pro with Codex

Each fresh clone currently needs one device login. The worker and frontier
share it, and stopping and restarting that clone preserves it. Enable device
code login in your ChatGPT security settings. After `lima prepare`, open a
guest shell on Ludwig, replacing `opsx-EXAMPLE` with the printed clone name.

```sh
limactl shell --workdir=/tmp opsx-EXAMPLE
```

Inside the guest, set `cli_auth_credentials_store = "file"` at the top level
of `~/.codex/config.toml`. Start the login, open the printed URL on your
workstation, and enter the code. Then check the status.

```sh
mise exec -- codex login --device-auth
mise exec -- codex login status
```

The status command should report `Logged in using ChatGPT`. The unattended
runner links to the guest's `~/.codex/auth.json`. The login stays on the
clone's disk, outside the campaign checkout that `lima export` copies.

Create `~/.config/opsx-build/` in the guest if needed. Add this connection to
its `config.toml` and select `codex` for both worker and frontier in the
campaign. Campaigns with an explicit Claude command, backend or model also
need those settings updated for Codex.

```toml
[connections.codex]
backend = "codex"
```

The Codex subscription connection needs HTTPS access to `chatgpt.com`.
In the guest's `~/.local/share/campaign-builder/network.json`, add
`"chatgpt.com"` to the existing `network.allow_domain` array. Leave the
existing domains in place. A successful `codex login status` checks the
saved login. Campaign preflight exercises the connection under nono.

Back in the campaign checkout on Ludwig, run `opsx-build lima run -- preflight`
for a supplied-contract campaign. For a campaign with a saved checkpoint,
`opsx-build lima run -- --resume` restarts the same clone after `limactl stop`
and reuses its login.

On 20 November 2026 the stop-and-resume test on Ludwig completed a second
delivery slice without another sign-in. The Sirocco campaign also passed
preflight and final acceptance with the same login. That clone had a guest
opsx-build binary that honors `--no-harness-sandbox` during Codex preflight.
As of that test, fresh clones from Ludwig's base need the `chatgpt.com` edit
and a [rebuilt guest opsx-build](#iterate-on-the-base) for Codex preflight.

## Manage a campaign clone manually

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
`proxy.golang.org` and `sum.golang.org` for Go module downloads and checksum
verification, and provides private loopback for local tests. The guest is
configured to install OpenSpec Propose, Apply, Verify and Archive. With
OpenSpec 2.1, opsx-build can discover workflows installed under `~/.agents/skills`
when the campaign has no matching workflow. The profile grants read access to
that directory. A small AppArmor allowance enables nono's namespaces on Ubuntu.
Adjust `~/.local/share/campaign-builder/network.json` for additional model
endpoints, registries or a campaign's Kubernetes API. Provisioning runs before
the campaign policy takes effect. Cluster credentials and cluster creation are
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

## Real Sirocco campaign checks

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
`~/campaign-builder/sirocco-environment-preflight.log` on Ludwig.

The separate Protocols 0.10.0 campaign exercised delivery and export on a
fresh 4-CPU, 8 GiB clone. After guest agent sign-in and bootstrap, the clone
was stopped. This command started the clone and completed delivery and
independent final acceptance without operator fixes.

```sh
opsx-build lima run -- --loop --obligation-inventory=obligations.json advance
```

The gate checked generated bindings and the packaged Go consumer with a fresh
module cache. `opsx-build lima export` produced a clean checkout at the accepted
commit, with the passing report and command logs intact. The clone was stopped
after export.
