//! Host-side VM lifecycle. The guest checkout owns the build state.
mod cleanup;
mod collect;
use std::{
    collections::BTreeSet,
    env, fs,
    io::{ErrorKind, Write},
    num::NonZeroU32,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand};
use opsx_build::{git::repository_root, process::shell_quote, ui::TerminalUi};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Parser)]
#[command(
    bin_name = "opsx-build lima",
    about = "Run a campaign in a prepared Lima builder"
)]
struct Cli {
    /// Host campaign checkout (defaults to the current repository).
    #[arg(long, global = true, default_value = ".")]
    repo: PathBuf,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Create a campaign clone and stage the checkout for guest sign-in.
    Prepare {
        /// Name of an existing, stopped Lima base instance.
        #[arg(long)]
        base: String,
        /// Persist this host variable in the guest user's login environment. Repeat for more names.
        #[arg(long, value_name = "NAME")]
        env: Vec<String>,
        #[command(flatten)]
        resources: Resources,
    },
    /// Run opsx-build in the campaign clone, starting the VM if needed.
    Run {
        /// Guest opsx-build arguments, such as execute or --resume.
        #[arg(last = true, required = true)]
        args: Vec<String>,
    },
    /// Open an interactive shell in the guest campaign checkout, starting the VM if needed.
    Shell,
    /// Change the campaign clone's CPU or RAM allocation after pausing the campaign.
    Resize {
        /// Number of virtual CPUs.
        #[arg(long, required_unless_present = "memory")]
        cpus: Option<NonZeroU32>,
        /// Guest RAM in GiB.
        #[arg(long)]
        memory: Option<f32>,
    },
    /// Collect results in the original campaign checkout, or export a separate copy.
    Export {
        destination: Option<PathBuf>,
        /// Stop and delete this clone after verifying the export.
        #[arg(long)]
        cleanup: bool,
    },
    /// Stop and delete this campaign's clone after verifying its last successful export.
    Cleanup,
    /// Send maintainer edits from this checkout to its paused campaign clone.
    Sync {
        /// Accept the corrected inputs and resume the existing checkpoint after syncing.
        #[arg(long)]
        resume: bool,
    },
}

#[derive(Args, Default, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Resources {
    /// Number of virtual CPUs.
    #[arg(long)]
    cpus: Option<NonZeroU32>,
    /// Guest RAM in GiB.
    #[arg(long)]
    memory: Option<f32>,
    /// Virtual disk size in GiB.
    #[arg(long)]
    disk: Option<f32>,
    /// Install the host's Claude subscription token and connection profiles for interactive use in the clone.
    #[arg(long)]
    #[serde(default)]
    claude_subscription: bool,
}

impl Resources {
    fn resolve(self, repo: &Path) -> Result<Self> {
        let path = repo.join("opsx-build.lima.toml");
        let defaults: Self = match fs::read_to_string(&path) {
            Ok(content) => {
                toml::from_str(&content).with_context(|| format!("reading {}", path.display()))?
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let resources = Self {
            cpus: self.cpus.or(defaults.cpus),
            memory: self.memory.or(defaults.memory),
            disk: self.disk.or(defaults.disk),
            claude_subscription: self.claude_subscription || defaults.claude_subscription,
        };
        resources.validate()?;
        Ok(resources)
    }

    fn validate(&self) -> Result<()> {
        for (key, size) in [("memory", self.memory), ("disk", self.disk)] {
            ensure!(
                size.is_none_or(|size| size.is_finite() && size > 0.0),
                "Lima {key} must be a positive size in GiB"
            );
        }
        Ok(())
    }

    fn add_args(&self, command: &mut Command) {
        if let Some(cpus) = self.cpus {
            command.arg(format!("--cpus={cpus}"));
        }
        if let Some(memory) = self.memory {
            command.arg(format!("--memory={memory}"));
        }
        if let Some(disk) = self.disk {
            command.arg(format!("--disk={disk}"));
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Campaign {
    instance: String,
    base: String,
    resources: Resources,
    #[serde(default)]
    env: BTreeSet<String>,
    guest_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exported: Option<cleanup::Receipt>,
}

const CLAUDE_TOKEN: &str = "CLAUDE_CODE_OAUTH_TOKEN";

pub fn run_cli() -> Result<Option<i32>> {
    let mut args = env::args_os();
    let program = args.next();
    if !args.next().is_some_and(|arg| arg == "lima") {
        return Ok(None);
    }
    let Cli { repo, action } = Cli::parse_from(program.into_iter().chain(args));
    let ui = TerminalUi::new(false, false, false);
    let repo = repository_root(&repo, &ui)?.canonicalize()?;
    let state = state_path(&repo)?;
    // Shells can coexist with a run, but must have exited before deleting the VM.
    let _shell_guard = if matches!(
        action,
        Action::Shell | Action::Cleanup | Action::Export { cleanup: true, .. }
    ) {
        fs::create_dir_all(state.parent().unwrap())?;
        let guard = fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(state.with_extension("shell-lock"))?;
        if matches!(action, Action::Shell) {
            guard
                .try_lock_shared()
                .context("cleanup is active for this campaign")?;
        } else {
            guard
                .try_lock()
                .context("close the campaign shell before cleanup")?;
        }
        Some(guard)
    } else {
        None
    };
    // A shell only reads the atomically saved location and can coexist with a run.
    let _lock = if matches!(action, Action::Shell) {
        None
    } else {
        fs::create_dir_all(state.parent().unwrap())?;
        let lock = fs::File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state.with_extension("lock"))?;
        lock.try_lock()
            .context(if matches!(action, Action::Resize { .. }) {
                "another Lima command is active for this campaign; pause the run before resizing"
            } else {
                "another Lima command is active for this campaign"
            })?;
        Some(lock)
    };

    let campaign = match fs::read(&state) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).context("invalid Lima campaign record")?),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("reading {}", state.display())),
    };
    if let Action::Prepare {
        base,
        resources,
        mut env,
    } = action
    {
        // The CLI flag requests a login token. TOML alone keeps forwarding it per run.
        if resources.claude_subscription {
            env.push(CLAUDE_TOKEN.to_owned());
        }
        prepare(
            &repo,
            &state,
            campaign,
            base,
            resources.resolve(&repo)?,
            env.into_iter().collect(),
        )?;
        return Ok(Some(0));
    }

    if matches!(action, Action::Cleanup) && campaign.is_none() {
        println!("No campaign clone is registered here.");
        return Ok(Some(0));
    }
    let mut campaign: Campaign =
        campaign.context("run 'opsx-build lima prepare --base NAME' first")?;
    let guest_dir = campaign
        .guest_dir
        .as_deref()
        .context("campaign import is incomplete; retry 'lima prepare' with the same options")?;
    eprintln!("Lima campaign: {}:{guest_dir}", campaign.instance);
    match action {
        Action::Resize { cpus, memory } => {
            resize(
                &campaign.instance,
                Resources {
                    cpus,
                    memory,
                    ..Resources::default()
                },
            )?;
            Ok(Some(0))
        }
        Action::Shell => {
            let status = shell(&campaign.instance, guest_dir)
                .status()
                .context("opening the guest campaign shell")?;
            Ok(Some(status.code().unwrap_or(1)))
        }
        Action::Run { args } => run_guest(&campaign, guest_dir, args).map(Some),
        Action::Sync { resume } => {
            collect::sync(&campaign.instance, guest_dir, &repo, &state)?;
            if resume {
                let code = run_guest(&campaign, guest_dir, vec!["accept-changes".into()])?;
                if code != 0 {
                    return Ok(Some(code));
                }
                return run_guest(&campaign, guest_dir, vec!["--resume".into()]).map(Some);
            }
            Ok(Some(0))
        }
        Action::Cleanup => {
            cleanup::remove(&campaign, &state)?;
            Ok(Some(0))
        }
        Action::Export {
            destination,
            cleanup: remove,
        } => {
            if campaign.exported.take().is_some() {
                save(&state, &campaign)?;
            }
            let target = destination.as_deref().unwrap_or(&repo);
            let full = destination.is_some();
            let signatures = if full {
                if remove {
                    ensure_guest_idle(&campaign.instance, guest_dir)?;
                }
                export(&campaign.instance, guest_dir, target)?;
                collect::signature(target, true).map(|s| Some((s.clone(), s)))
            } else {
                Ok(collect::export(
                    &campaign.instance,
                    guest_dir,
                    &repo,
                    &state,
                )?)
            };
            let receipt = signatures.and_then(|signature| {
                cleanup::Receipt::new(
                    &campaign.instance,
                    target,
                    full,
                    signature.context(
                        "the pending export was staged by an older launcher; export once more",
                    )?,
                )
            });
            match receipt {
                Ok(receipt) => {
                    campaign.exported = Some(receipt);
                    save(&state, &campaign)
                        .context("export succeeded but could not record it for cleanup")?;
                }
                Err(error) if remove => {
                    return Err(error).context("export succeeded but cleanup could not verify it");
                }
                Err(error) => eprintln!("Export succeeded without a cleanup record: {error}"),
            }
            if remove {
                cleanup::remove(&campaign, &state)?;
            }
            Ok(Some(0))
        }
        Action::Prepare { .. } => unreachable!(),
    }
}

fn run_guest(campaign: &Campaign, dir: &str, args: Vec<String>) -> Result<i32> {
    let status = if campaign.resources.claude_subscription && !campaign.env.contains(CLAUDE_TOKEN) {
        run_with_claude_token(&campaign.instance, dir, args)?
    } else {
        shell(&campaign.instance, dir)
            .args(["mise", "exec", "--", "opsx-campaign"])
            .args(args)
            .status()
            .context("running the campaign with limactl shell")?
    };
    Ok(status.code().unwrap_or(1))
}

fn host_config_file(name: &str) -> Result<PathBuf> {
    let root = env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("set HOME or XDG_CONFIG_HOME to locate the Claude subscription configuration")?;
    Ok(root.join("opsx-build").join(name))
}

fn claude_token() -> Result<String> {
    let path = host_config_file("claude-token")?;
    let token = fs::read_to_string(&path)
        .with_context(|| format!("reading Claude setup token from {}", path.display()))?;
    let token = token.trim();
    ensure!(
        !token.is_empty() && !token.contains(char::is_whitespace),
        "expected one Claude setup token in {}",
        path.display()
    );
    Ok(token.to_owned())
}

fn run_with_claude_token(instance: &str, guest_dir: &str, args: Vec<String>) -> Result<ExitStatus> {
    let token = claude_token()?;
    let mut command = limactl();
    command
        .args([
            "shell",
            "--tty=false",
            "--start",
            "--workdir",
            guest_dir,
            instance,
        ])
        .args([
            "bash",
            "-c",
            r#"IFS= read -r CLAUDE_CODE_OAUTH_TOKEN || exit 1
export CLAUDE_CODE_OAUTH_TOKEN
export OPSX_BUILD_CONFIG="$HOME/.local/share/campaign-builder/lima-claude.toml"
exec mise exec -- opsx-campaign "$@""#,
            "opsx-build",
        ])
        .args(args)
        .env_remove(CLAUDE_TOKEN);
    with_input(&mut command, &format!("{token}\n"))
        .context("sending the Claude setup token to the guest")
}

fn with_input(command: &mut Command, input: &str) -> Result<ExitStatus> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    let sent = child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes());
    let status = child
        .wait()
        .with_context(|| format!("waiting for {program}"))?;
    sent.with_context(|| format!("writing to {program}"))?;
    Ok(status)
}

fn login_environment(names: &BTreeSet<String>, subscription: bool) -> Result<String> {
    let mut exports = String::new();
    if subscription && names.contains(CLAUDE_TOKEN) && !names.contains("OPSX_BUILD_CONFIG") {
        exports.push_str(
            "export OPSX_BUILD_CONFIG=\"$HOME/.local/share/campaign-builder/lima-claude.toml\"\n",
        );
    }
    for name in names {
        let mut chars = name.chars();
        ensure!(
            chars
                .next()
                .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
                && chars.all(|c| c == '_' || c.is_ascii_alphanumeric()),
            "--env expects a shell variable name"
        );
        let value = if subscription && name == CLAUDE_TOKEN {
            claude_token()?
        } else {
            env::var(name).map_err(|_| {
                anyhow::anyhow!("--env {name} needs a UTF-8 value in the host environment")
            })?
        };
        exports.push_str(&format!("export {name}={}\n", shell_quote(&value)));
    }
    Ok(exports)
}

fn state_path(repo: &Path) -> Result<PathBuf> {
    let root = env::var_os("XDG_STATE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .context("set HOME or XDG_STATE_HOME to save the Lima campaign location")?;
    let key = Sha256::digest(repo.as_os_str().as_encoded_bytes());
    Ok(root.join("opsx-build/lima").join(format!("{key:x}.json")))
}

fn save(path: &Path, campaign: &Campaign) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(campaign)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn list_instances() -> Result<String> {
    output(
        limactl().args(["list", "--format", "{{.Name}} {{.Status}}"]),
        "listing Lima instances",
    )
}

fn instance_status<'a>(list: &'a str, name: &str) -> Option<&'a str> {
    list.lines().find_map(|line| {
        let (instance, status) = line.split_once(' ')?;
        (instance == name).then_some(status.trim())
    })
}

fn prepare(
    repo: &Path,
    state: &Path,
    saved: Option<Campaign>,
    base: String,
    resources: Resources,
    env: BTreeSet<String>,
) -> Result<()> {
    ensure!(
        fs::symlink_metadata(repo.join(".git")).is_ok_and(|entry| entry.is_dir())
            && !repo.join(".git/objects/info/alternates").exists(),
        "Lima import needs a standalone Git checkout with its own .git directory and objects"
    );
    let claude_config = if resources.claude_subscription {
        let path = host_config_file("lima-claude.toml")?;
        ensure!(
            path.is_file(),
            "missing Claude connection profiles at {}",
            path.display()
        );
        Some(path)
    } else {
        None
    };
    let exports = login_environment(&env, resources.claude_subscription)?;
    let instances = list_instances()?;
    let status = |name: &str| instance_status(&instances, name);
    let mut campaign = if let Some(saved) = saved.filter(|saved| status(&saved.instance).is_some())
    {
        ensure!(
            saved.guest_dir.is_none(),
            "campaign already prepared in {}; use 'lima run' or 'lima export'",
            saved.instance
        );
        ensure!(
            saved.base == base && saved.resources == resources && saved.env == env,
            "retry the incomplete Lima import with its original base and resources"
        );
        saved
    } else {
        ensure!(
            status(&base) == Some("Stopped"),
            "Lima base '{base}' must exist and be stopped"
        );
        let id = Uuid::new_v4().simple().to_string();
        let campaign = Campaign {
            instance: format!("opsx-{}", &id[..16]),
            base,
            resources,
            env,
            guest_dir: None,
            exported: None,
        };
        save(state, &campaign)?;
        campaign
    };
    println!("Preparing Lima campaign in {}", campaign.instance);
    let baseline = collect::baseline(&state, &campaign.instance);
    if let Err(error) = collect::remember(&repo, &baseline) {
        if baseline.try_exists()? {
            fs::remove_file(&baseline)?;
        }
        eprintln!(
            "Could not record a file baseline for in-place export ({error}); export will check against the committed inputs."
        );
    }
    if status(&campaign.instance).is_none() {
        let mut clone = limactl();
        clone.args(["clone", "--tty=false", &campaign.base, &campaign.instance]);
        campaign.resources.add_args(&mut clone);
        checked(&mut clone, "cloning the prepared Lima base")?;
    }
    // A retry gets an empty staging directory even if an earlier transfer was partial.
    let guest_dir = output(
        shell(&campaign.instance, "/tmp").args([
            "bash",
            "-c",
            r#"mkdir -p "$HOME/campaigns" && mktemp -d "$HOME/campaigns/opsx.XXXXXXXX""#,
        ]),
        "creating the guest campaign directory",
    )?;
    ensure!(
        Path::new(&guest_dir).is_absolute(),
        "Lima did not return a guest campaign directory"
    );
    checked(
        limactl().current_dir(repo).args([
            "copy",
            "--backend=rsync",
            "./",
            &format!("{}:{guest_dir}/", campaign.instance),
        ]),
        "copying the campaign to the guest; retry 'lima prepare' if the transfer fails",
    )?;
    if let Some(path) = claude_config {
        checked(
            limactl()
                .args(["copy", "--backend=rsync"])
                .arg(path)
                .arg(format!(
                    "{}:~/.local/share/campaign-builder/lima-claude.toml",
                    campaign.instance
                )),
            "copying the Claude connection profiles to the guest",
        )?;
    }
    if !exports.is_empty() {
        let mut command = shell(&campaign.instance, "/tmp");
        command
            .args(["bash", "-c", include_str!("../lima/install-login-env.sh")])
            .stdout(Stdio::null());
        ensure!(
            with_input(&mut command, &exports)?.success(),
            "installing the guest login environment failed"
        );
    }
    campaign.guest_dir = Some(guest_dir.clone());
    save(state, &campaign)?;
    let instance = &campaign.instance;
    println!("Campaign staged in {instance}:{guest_dir}");
    println!("Open the campaign shell: opsx-build lima shell");
    if !campaign.resources.claude_subscription {
        println!("Use the guest shell to set up agent sign-in and connections.");
    }
    Ok(())
}

fn resize(instance: &str, resources: Resources) -> Result<()> {
    #[derive(Deserialize)]
    struct Instance {
        status: String,
        cpus: u32,
        memory: u64,
    }
    resources.validate()?;
    let info = output(
        limactl().args(["list", "--json", instance]),
        "reading the clone's allocation",
    )?;
    let current: Instance =
        serde_json::from_str(&info).context("could not read the clone's CPU and RAM allocation")?;
    ensure!(
        matches!(current.status.as_str(), "Running" | "Stopped"),
        "cannot resize a clone in state {}",
        current.status
    );
    if resources.cpus.is_none_or(|n| n.get() == current.cpus)
        && resources
            .memory
            .is_none_or(|gib| gib == current.memory as f32 / (1_u32 << 30) as f32)
    {
        println!("The clone already has the requested allocation.");
        return Ok(());
    }
    if current.status == "Running" {
        ensure_guest_idle(instance, "/")?;
        checked(
            limactl().args(["stop", instance]),
            "stopping the clone for resizing",
        )?;
    }
    let mut edit = limactl();
    edit.args(["edit", "--tty=false", instance]);
    resources.add_args(&mut edit);
    checked(&mut edit, "updating the stopped clone's allocation")?;
    println!("Clone resized. Resume a paused campaign with 'opsx-build lima run -- --resume'.");
    Ok(())
}

// A campaign started directly in a guest shell does not hold the host lock.
fn ensure_guest_idle(instance: &str, dir: &str) -> Result<()> {
    let processes = output(
        shell(instance, dir).args(["ps", "-e", "-o", "comm="]),
        "checking for an active campaign in the guest",
    )?;
    ensure!(
        !processes.lines().any(|name| name.trim() == "opsx-build"),
        "pause the active guest campaign before continuing"
    );
    Ok(())
}

fn export(instance: &str, guest_dir: &str, destination: &Path) -> Result<()> {
    fs::create_dir(destination)
        .with_context(|| format!("choose a new export directory: {}", destination.display()))?;
    let result = (|| {
        checked(
            shell(instance, guest_dir).arg("true"),
            "opening the guest checkout",
        )?;
        checked(
            limactl().current_dir(destination).args([
                "copy",
                "--backend=rsync",
                &format!("{instance}:{guest_dir}/"),
                "./",
            ]),
            "copying campaign results from the guest",
        )
    })();
    result.with_context(|| {
        format!(
            "export incomplete in {}; retry with a new destination",
            destination.display()
        )
    })?;
    println!("Exported campaign to {}", destination.display());
    Ok(())
}

fn limactl() -> Command {
    Command::new("limactl")
}

fn shell(instance: &str, dir: &str) -> Command {
    let mut command = limactl();
    command.args(["shell", "--start", "--workdir", dir, instance]);
    command
}

fn checked(command: &mut Command, action: &str) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("{action} with limactl"))?;
    ensure!(status.success(), "{action} failed ({status})");
    Ok(())
}

fn output(command: &mut Command, action: &str) -> Result<String> {
    let result = command
        .output()
        .with_context(|| format!("{action} with limactl"))?;
    if !result.status.success() {
        bail!(
            "{action} failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_accepts_only_cpu_and_memory_changes() {
        let parse = |flags: &[&str]| {
            Cli::try_parse_from(
                ["opsx-build lima", "resize"]
                    .into_iter()
                    .chain(flags.iter().copied()),
            )
        };
        for flags in [
            &["--cpus", "4"][..],
            &["--memory", "6.5"],
            &["--cpus", "2", "--memory", "8"],
        ] {
            assert!(parse(flags).is_ok());
        }
        for flags in [&[][..], &["--disk", "80"], &["--cpus", "0"]] {
            assert!(parse(flags).is_err());
        }
        for size in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(
                Resources {
                    memory: Some(size),
                    ..Resources::default()
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn claude_subscription_is_opt_in() {
        let old: Resources = serde_json::from_str(r#"{"cpus":4,"memory":8,"disk":null}"#).unwrap();
        assert!(!old.claude_subscription);

        let enabled: Resources = toml::from_str("cpus = 4\nclaude_subscription = true").unwrap();
        assert_eq!(enabled.cpus, NonZeroU32::new(4));
        assert!(enabled.claude_subscription);
    }

    #[test]
    fn prepare_accepts_a_claude_subscription_without_campaign_config() {
        let cli = Cli::try_parse_from([
            "opsx-build lima",
            "prepare",
            "--base",
            "test-base",
            "--claude-subscription",
        ])
        .unwrap();
        let Action::Prepare { resources, .. } = cli.action else {
            unreachable!()
        };
        let unconfigured = env::temp_dir().join(Uuid::new_v4().to_string());
        assert!(
            resources
                .resolve(&unconfigured)
                .unwrap()
                .claude_subscription
        );
    }

    #[test]
    fn older_campaign_records_have_no_persisted_environment() {
        let campaign: Campaign = serde_json::from_str(
            r#"{"instance":"old","base":"base","resources":{"claude_subscription":true},"guest_dir":"/tmp/checkout"}"#,
        ).unwrap();
        assert!(campaign.env.is_empty());
        assert!(campaign.resources.claude_subscription);
    }

    #[test]
    fn rejects_invalid_or_missing_host_variables() {
        for name in ["X=secret", "0NAME", "OPSX_LIMA_TEST_UNSET_9F7D"] {
            let error = login_environment(&BTreeSet::from([name.to_owned()]), false).unwrap_err();
            assert!(!error.to_string().contains("secret"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn guest_login_file_is_private_and_retry_preserves_the_profile() {
        use std::os::unix::fs::PermissionsExt;

        let home = env::temp_dir().join(format!("opsx-lima-login-{}", Uuid::new_v4()));
        fs::create_dir(&home).unwrap();
        let profile = home.join(".bash_profile");
        fs::write(&profile, "export EXISTING_SETTING=yes\n").unwrap();
        let value = "spaces ' \" $(false)\nsecond line\n";
        let mut setup = Command::new("bash");
        setup
            .args(["-c", include_str!("../lima/install-login-env.sh")])
            .env("HOME", &home);
        for setting in ["previous", value] {
            assert!(
                with_input(
                    &mut setup,
                    &format!("export EXAMPLE={}\n", shell_quote(setting))
                )
                .unwrap()
                .success()
            );
        }
        let mode = fs::metadata(home.join(".config/opsx-build/lima-env.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            fs::read_to_string(profile)
                .unwrap()
                .matches("lima-env.sh")
                .count(),
            1
        );
        assert!(Command::new("bash")
            .args(["-c", r#". "$HOME/.bash_profile" && [[ $EXAMPLE == "$EXPECTED" && $EXISTING_SETTING == yes ]]"#])
            .env("HOME", &home).env("EXPECTED", value).status().unwrap().success());
        fs::remove_dir_all(home).unwrap();
    }
}
