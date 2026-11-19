//! Host-side VM lifecycle. The guest checkout owns the build state.
use std::{
    env, fs,
    io::{ErrorKind, Write},
    num::NonZeroU32,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand};
use opsx_build::{git::repository_root, ui::TerminalUi};
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
        #[command(flatten)]
        resources: Resources,
    },
    /// Run opsx-build in the campaign clone, starting the VM if needed.
    Run {
        /// Guest opsx-build arguments, such as execute or --resume.
        #[arg(last = true, required = true)]
        args: Vec<String>,
    },
    /// Copy the complete guest checkout to a new host directory.
    Export { destination: PathBuf },
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
    #[arg(skip)]
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
            claude_subscription: defaults.claude_subscription,
        };
        for (key, size) in [("memory", resources.memory), ("disk", resources.disk)] {
            ensure!(
                size.is_none_or(|size| size.is_finite() && size > 0.0),
                "Lima {key} must be a positive size in GiB"
            );
        }
        Ok(resources)
    }

    fn clone_args(&self, command: &mut Command) {
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
    guest_dir: Option<String>,
}

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
    fs::create_dir_all(state.parent().unwrap())?;
    let lock = fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state.with_extension("lock"))?;
    lock.try_lock()
        .context("another Lima command is active for this campaign")?;

    let campaign = match fs::read(&state) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).context("invalid Lima campaign record")?),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("reading {}", state.display())),
    };
    if let Action::Prepare { base, resources } = action {
        prepare(&repo, &state, campaign, base, resources.resolve(&repo)?)?;
        return Ok(Some(0));
    }

    let campaign: Campaign = campaign.context("run 'opsx-build lima prepare --base NAME' first")?;
    let guest_dir = campaign
        .guest_dir
        .as_deref()
        .context("campaign import is incomplete; retry 'lima prepare' with the same options")?;
    eprintln!("Lima campaign: {}:{guest_dir}", campaign.instance);
    match action {
        Action::Run { args } => {
            let status = if campaign.resources.claude_subscription {
                run_with_claude_token(&campaign.instance, guest_dir, args)?
            } else {
                shell(&campaign.instance, guest_dir)
                    .args(["mise", "exec", "--", "opsx-campaign"])
                    .args(args)
                    .status()
                    .context("running the campaign with limactl shell")?
            };
            Ok(Some(status.code().unwrap_or(1)))
        }
        Action::Export { destination } => {
            export(&campaign.instance, guest_dir, &destination)?;
            Ok(Some(0))
        }
        Action::Prepare { .. } => unreachable!(),
    }
}

fn host_config_file(name: &str) -> Result<PathBuf> {
    let root = env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("set HOME or XDG_CONFIG_HOME to locate the Claude subscription configuration")?;
    Ok(root.join("opsx-build").join(name))
}

fn run_with_claude_token(
    instance: &str,
    guest_dir: &str,
    args: Vec<String>,
) -> Result<std::process::ExitStatus> {
    let path = host_config_file("claude-token")?;
    let token = fs::read_to_string(&path)
        .with_context(|| format!("reading Claude setup token from {}", path.display()))?;
    let token = token.trim();
    ensure!(
        !token.is_empty() && !token.contains(char::is_whitespace),
        "expected one Claude setup token in {}",
        path.display()
    );
    let mut child = limactl()
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
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .stdin(Stdio::piped())
        .spawn()
        .context("starting the campaign with limactl shell")?;
    // Feed SSH via a pipe so the token stays out of subprocess arguments.
    let mut input = child.stdin.take().expect("piped campaign stdin");
    let sent = writeln!(input, "{token}");
    drop(input);
    let status = child.wait().context("waiting for limactl shell")?;
    sent.context("sending the Claude setup token to the guest")?;
    Ok(status)
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

fn prepare(
    repo: &Path,
    state: &Path,
    saved: Option<Campaign>,
    base: String,
    resources: Resources,
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
    let instances = output(
        limactl().args(["list", "--format", "{{.Name}} {{.Status}}"]),
        "listing Lima instances",
    )?;
    let status = |name: &str| {
        instances.lines().find_map(|line| {
            let (instance, status) = line.split_once(' ')?;
            (instance == name).then_some(status.trim())
        })
    };
    let mut campaign = if let Some(saved) = saved.filter(|saved| status(&saved.instance).is_some())
    {
        ensure!(
            saved.guest_dir.is_none(),
            "campaign already prepared in {}; use 'lima run' or 'lima export'",
            saved.instance
        );
        ensure!(
            saved.base == base && saved.resources == resources,
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
            guest_dir: None,
        };
        save(state, &campaign)?;
        campaign
    };
    println!("Preparing Lima campaign in {}", campaign.instance);
    if status(&campaign.instance).is_none() {
        let mut clone = limactl();
        clone.args(["clone", "--tty=false", &campaign.base, &campaign.instance]);
        campaign.resources.clone_args(&mut clone);
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
    campaign.guest_dir = Some(guest_dir.clone());
    save(state, &campaign)?;
    let instance = &campaign.instance;
    println!("Campaign staged in {instance}:{guest_dir}");
    if !campaign.resources.claude_subscription {
        println!("Open 'limactl shell {instance}' to set up guest agent sign-in and connections.");
    }
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
    fn claude_subscription_is_opt_in() {
        let old: Resources = serde_json::from_str(r#"{"cpus":4,"memory":8,"disk":null}"#).unwrap();
        assert!(!old.claude_subscription);

        let enabled: Resources = toml::from_str("cpus = 4\nclaude_subscription = true").unwrap();
        assert_eq!(enabled.cpus, NonZeroU32::new(4));
        assert!(enabled.claude_subscription);
    }
}
