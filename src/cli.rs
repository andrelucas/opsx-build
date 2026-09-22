use std::{collections::BTreeMap, env, fmt::Display, fs, path::PathBuf, str::FromStr};

use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Deserializer};

use crate::stream::StreamFilter;

const DEFAULT_MAX_VERIFY_RETRIES: u32 = 3;
const DEFAULT_MAX_OUTPUT_RETRIES: u32 = 3;
const DEFAULT_MAX_PROVIDER_RETRIES: u32 = 3;
const DEFAULT_LOCAL_WORKER_TIMEOUT_MINUTES: u32 = 60;
const DEFAULT_PERMISSION_MODE: &str = "auto";
const DEFAULT_CLAUDE_COMMAND: &str = "claude";
const DEFAULT_FRONTIER_COMMAND: &str = "claude";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    #[default]
    Claude,
    #[serde(rename = "opencode")]
    OpenCode,
    Codex,
}

impl BackendKind {
    fn default_command(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenCode => "opencode",
            Self::Codex => "codex",
        }
    }
}

impl Display for BackendKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Claude => "claude",
            Self::OpenCode => "opencode",
            Self::Codex => "codex",
        })
    }
}

#[derive(Debug, Clone)]
pub struct Cli {
    pub request: String,
    pub execute: bool,
    pub rewind_target: Option<String>,
    pub yes: bool,
    pub bootstrap_context: Option<PathBuf>,
    pub bootstrap_defines: BTreeMap<String, String>,
    pub repo: PathBuf,
    pub sidecar: bool,
    pub sidecar_root: Option<PathBuf>,
    pub local_only: bool,
    pub yolo: bool,
    pub interactive: bool,
    pub test_connection: Option<String>,
    pub basic_connection_test: bool,
    pub update_skills: bool,
    pub resume: bool,
    pub forget: bool,
    pub continue_existing: bool,
    pub loop_workflow: bool,
    pub no_loop: bool,
    pub max_iterations: Option<u32>,
    pub change: Option<String>,
    pub direction: Option<String>,
    pub interactive_args: Vec<String>,
    pub max_verify_retries: u32,
    pub max_output_retries: u32,
    pub max_provider_retries: u32,
    pub local_worker_timeout_minutes: u32,
    pub verbose: bool,
    pub debug: bool,
    pub stream_claude: Option<StreamFilter>,
    pub dry_run: bool,
    pub permission_mode: String,
    pub worker_connection: AgentConnection,
    pub frontier_connection: AgentConnection,
    pub explore_command: Option<String>,
    pub propose_command: Option<String>,
    pub apply_command: Option<String>,
    pub verify_command: Option<String>,
    pub archive_command: Option<String>,
    pub config_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConnection {
    pub name: Option<String>,
    pub backend: BackendKind,
    pub environment_name: Option<String>,
    pub command: String,
    pub model: Option<String>,
    pub permission_profile: Option<String>,
    pub context_window: Option<u64>,
    pub auto_compact_window: Option<u64>,
    pub auto_compact_percent: Option<u8>,
    pub max_output_tokens: Option<u64>,
    pub env: BTreeMap<String, ConnectionEnvironmentValue>,
    pub isolate: bool,
    pub unset_env: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ConnectionEnvironmentValue {
    Literal(String),
    FromEnvironment(EnvironmentReference),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentReference {
    pub from_env: String,
}

impl Cli {
    pub fn load() -> Result<Self> {
        Self::resolve(CliArgs::parse())
    }

    fn resolve(args: CliArgs) -> Result<Self> {
        let request_was_supplied = args.request.is_some();
        let command_line_max_iterations = args.max_iterations.is_some();
        let (mut config, config_path) = load_config(&args)?;
        apply_legacy_environment(&mut config)?;
        let cli = resolve_values(args, config, config_path)?;
        if cli.rewind_target.is_some() && cli.request != "rewind" {
            anyhow::bail!("a rewind target is only valid with `opsx-build rewind [REF]`");
        }
        if cli.yes && cli.request != "rewind" {
            anyhow::bail!("--yes is only valid with `opsx-build rewind [REF]`");
        }
        if cli.request == "rewind"
            && (cli.bootstrap_context.is_some()
                || !cli.bootstrap_defines.is_empty()
                || cli.interactive
                || cli.test_connection.is_some()
                || cli.update_skills
                || cli.resume
                || cli.forget
                || cli.continue_existing
                || cli.change.is_some()
                || cli.direction.is_some())
        {
            anyhow::bail!("`opsx-build rewind` cannot be combined with another workflow mode");
        }
        if command_line_max_iterations && !cli.loop_workflow {
            anyhow::bail!("--max-iterations requires --loop (or `loop = true` in config)");
        }
        if cli.loop_workflow && cli.continue_existing {
            anyhow::bail!("--loop cannot be combined with --continue-existing");
        }
        if cli.bootstrap_context.is_some()
            && cli.request != "bootstrap"
            && !cli.execute
            && !cli.sidecar
        {
            anyhow::bail!(
                "--context is only valid with `opsx-build bootstrap`, `opsx-build execute`, or --sidecar"
            );
        }
        if !cli.bootstrap_defines.is_empty() && cli.request != "bootstrap" && !cli.execute {
            anyhow::bail!(
                "--define is only valid with `opsx-build bootstrap` or `opsx-build execute`"
            );
        }
        if cli.change.is_some() && !cli.continue_existing {
            if !request_was_supplied || cli.request == "advance" {
                anyhow::bail!(
                    "--change requires an explicit free-form request or --continue-existing"
                );
            }
            if cli.loop_workflow {
                anyhow::bail!("--change cannot prescribe one name for a multi-change campaign");
            }
        }
        if cli.request == "bootstrap"
            && (cli.interactive
                || cli.test_connection.is_some()
                || cli.update_skills
                || cli.resume
                || cli.forget
                || cli.continue_existing
                || cli.loop_workflow
                || cli.no_loop
                || cli.max_iterations.is_some()
                || cli.change.is_some()
                || cli.direction.is_some()
                || cli.sidecar
                || cli.local_only)
        {
            anyhow::bail!("`opsx-build bootstrap` cannot be combined with another workflow mode");
        }
        if cli.execute
            && (cli.interactive
                || cli.test_connection.is_some()
                || cli.update_skills
                || cli.resume
                || cli.forget
                || cli.continue_existing
                || cli.no_loop
                || cli.change.is_some()
                || cli.direction.is_some()
                || cli.sidecar
                || cli.local_only)
        {
            anyhow::bail!("`opsx-build execute` cannot be combined with another workflow mode");
        }
        if cli.sidecar
            && !cli.resume
            && !cli.forget
            && !cli.update_skills
            && (cli.request == "advance" || cli.request.is_empty())
        {
            anyhow::bail!("--sidecar requires an explicit bounded change request");
        }
        if cli.sidecar && cli.loop_workflow {
            anyhow::bail!("the initial sidecar release supports one bounded change, not --loop");
        }
        if cli.sidecar && cli.continue_existing {
            anyhow::bail!("the initial sidecar release does not support --continue-existing");
        }
        if cli.sidecar && !cli.resume && !cli.forget && !cli.update_skills && cli.change.is_none() {
            anyhow::bail!("--sidecar requires --change NAME for a bounded one-off change");
        }
        Ok(cli)
    }
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "opsx-build",
    version,
    about = "Build an OpenSpec change through synchronous Claude stages"
)]
struct CliArgs {
    /// Change request, `advance`, `bootstrap`, `execute`, or `rewind`. Defaults to `advance`.
    request: Option<String>,

    /// Git revision used by `opsx-build rewind` (defaults to `post-bootstrap`).
    rewind_target: Option<String>,

    /// Confirm a destructive rewind without prompting.
    #[arg(long)]
    yes: bool,

    /// Markdown project context; bootstrap/execute default to context.md in the repository root.
    #[arg(long, value_name = "PATH")]
    context: Option<PathBuf>,

    /// Substitute one {{name}} placeholder in bootstrap context (repeatable).
    #[arg(long, value_name = "NAME=VALUE")]
    define: Vec<String>,

    /// Repository containing .git, openspec/, and Claude skills.
    #[arg(long, default_value = ".", value_name = "PATH")]
    repo: PathBuf,

    /// Keep OpenSpec and its history in an external native OpenSpec store.
    #[arg(long, env = "OPSX_BUILD_SIDECAR")]
    sidecar: bool,

    /// Parent directory for automatically named sidecar stores.
    #[arg(long, env = "OPSX_BUILD_SIDECAR_ROOT", value_name = "PATH")]
    sidecar_root: Option<PathBuf>,

    /// Use only the worker connection and never invoke frontier fallback.
    #[arg(long, env = "OPSX_BUILD_LOCAL_ONLY")]
    local_only: bool,

    /// Skip proposal milestone commits, retaining only the final completion commit.
    #[arg(long, env = "OPSX_BUILD_YOLO", conflicts_with = "no_yolo")]
    yolo: bool,

    /// Restore proposal milestone commits when yolo is enabled in configuration.
    #[arg(long)]
    no_yolo: bool,

    /// Open an interactive session with the selected worker backend.
    #[arg(long)]
    interactive: bool,

    /// Exercise a named agent connection, including one tool-result round trip, and exit.
    #[arg(
        long,
        value_name = "NAME",
        conflicts_with_all = [
            "request",
            "interactive",
            "update_skills",
            "resume",
            "forget",
            "continue_existing",
            "loop_workflow",
            "no_loop",
            "max_iterations",
            "change",
            "direction",
            "worker_connection"
        ]
    )]
    test_connection: Option<String>,

    /// Test only a single text response, without exercising tool-result compatibility.
    #[arg(long, requires = "test_connection")]
    basic_connection_test: bool,

    /// Install or refresh the bundled unattended skills, then exit.
    #[arg(
        long,
        conflicts_with_all = [
            "request",
            "interactive",
            "test_connection",
            "resume",
            "forget",
            "continue_existing",
            "loop_workflow",
            "no_loop",
            "max_iterations",
            "change",
            "direction"
        ]
    )]
    update_skills: bool,

    /// Resume the last durable opsx-build run from its first incomplete phase.
    #[arg(long, conflicts_with = "interactive")]
    resume: bool,

    /// Forget the saved workflow checkpoint without changing repository files or Git history.
    #[arg(long, conflicts_with_all = ["interactive", "resume", "direction"])]
    forget: bool,

    /// Continue an active OpenSpec change, committing planning work first if necessary.
    #[arg(
        long,
        conflicts_with_all = ["interactive", "resume", "forget"]
    )]
    continue_existing: bool,

    /// Repeat complete OpenSpec changes until the agenda or objective is complete.
    #[arg(
        long = "loop",
        env = "OPSX_BUILD_LOOP",
        conflicts_with_all = ["interactive", "forget", "continue_existing"]
    )]
    loop_workflow: bool,

    /// Disable a campaign loop enabled by the config file.
    #[arg(long, conflicts_with = "loop_workflow")]
    no_loop: bool,

    /// Stop with an incomplete-campaign error after this many completed changes.
    #[arg(long, env = "OPSX_BUILD_MAX_ITERATIONS", value_name = "N")]
    max_iterations: Option<std::num::NonZeroU32>,

    /// Prescribe a new change name, or select one used by --continue-existing.
    #[arg(
        long,
        value_name = "NAME",
        conflicts_with_all = ["interactive", "resume", "forget"]
    )]
    change: Option<String>,

    /// One-shot guidance for the next code-changing stage of a resumed run.
    #[arg(long, requires = "resume", value_name = "TEXT")]
    direction: Option<String>,

    /// Additional arguments passed directly to the selected agent after `--`.
    #[arg(last = true, value_name = "AGENT_ARGS", requires = "interactive")]
    interactive_args: Vec<String>,

    /// Load defaults from this TOML file.
    #[arg(long, env = "OPSX_BUILD_CONFIG", value_name = "PATH")]
    config: Option<PathBuf>,

    /// Do not load the default or environment-selected config file.
    #[arg(long)]
    no_config: bool,

    /// Maximum repair/verify cycles after the first verification attempt.
    #[arg(long, env = "OPSX_BUILD_MAX_VERIFY_RETRIES", value_name = "N")]
    max_verify_retries: Option<u32>,

    /// Maximum same-session continuations after Claude reaches its output token limit.
    #[arg(long, env = "OPSX_BUILD_MAX_OUTPUT_RETRIES", value_name = "N")]
    max_output_retries: Option<u32>,

    /// Maximum same-session retries after a transient provider or network failure.
    #[arg(long, env = "OPSX_BUILD_MAX_PROVIDER_RETRIES", value_name = "N")]
    max_provider_retries: Option<u32>,

    /// Maximum minutes for one local Explore, Propose, Apply, Verify, or Repair stage.
    #[arg(
        long,
        env = "OPSX_BUILD_LOCAL_WORKER_TIMEOUT_MINUTES",
        value_name = "MINUTES"
    )]
    local_worker_timeout_minutes: Option<std::num::NonZeroU32>,

    /// Print commands and captured subprocess output.
    #[arg(long, short)]
    verbose: bool,

    /// Show resolved commands, agent session details, and complete prompts.
    #[arg(long)]
    debug: bool,

    /// Stream agent activity; optionally select activity, full, or raw filtering.
    #[arg(
        long,
        visible_alias = "stream-agent",
        env = "OPSX_BUILD_STREAM_CLAUDE",
        value_enum,
        value_name = "FILTER",
        num_args = 0..=1,
        default_missing_value = "activity",
        require_equals = true
    )]
    stream_claude: Option<StreamFilter>,

    /// Validate the repository and print the workflow without changing it.
    #[arg(long)]
    dry_run: bool,

    /// Claude permission mode used for unattended subprocesses.
    #[arg(long, env = "OPSX_BUILD_PERMISSION_MODE", value_name = "MODE")]
    permission_mode: Option<String>,

    /// Command prefix used to launch Claude (for example, "omlx launch claude").
    #[arg(long, env = "OPSX_BUILD_CLAUDE_COMMAND", value_name = "COMMAND")]
    claude_command: Option<String>,

    /// Model passed to the configured Claude launcher as `--model MODEL`.
    #[arg(long, env = "OPSX_BUILD_CLAUDE_MODEL", value_name = "MODEL")]
    claude_model: Option<String>,

    /// Named connection profile used for ordinary worker stages.
    #[arg(long, env = "OPSX_BUILD_WORKER_CONNECTION", value_name = "NAME")]
    worker_connection: Option<String>,

    /// Command prefix used for bootstrap, frontier planning, and local escalation.
    #[arg(long, env = "OPSX_BUILD_FRONTIER_COMMAND", value_name = "COMMAND")]
    frontier_command: Option<String>,

    /// Optional frontier model override; omitted uses the frontier harness default.
    #[arg(long, env = "OPSX_BUILD_FRONTIER_MODEL", value_name = "MODEL")]
    frontier_model: Option<String>,

    /// Named connection profile used for bootstrap and frontier replanning.
    #[arg(long, env = "OPSX_BUILD_FRONTIER_CONNECTION", value_name = "NAME")]
    frontier_connection: Option<String>,

    /// Context capacity used for Claude auto-compaction (supports binary k/m suffixes).
    #[arg(long, env = "OPSX_BUILD_AUTO_COMPACT_WINDOW", value_name = "TOKENS")]
    auto_compact_window: Option<TokenCount>,

    /// Percentage of the effective context capacity at which Claude auto-compacts.
    #[arg(long, env = "OPSX_BUILD_AUTO_COMPACT_PERCENT", value_name = "PERCENT")]
    auto_compact_percent: Option<Percentage>,

    /// Maximum Claude output tokens per request (supports binary k/m suffixes).
    #[arg(long, env = "OPSX_BUILD_MAX_OUTPUT_TOKENS", value_name = "TOKENS")]
    max_output_tokens: Option<TokenCount>,

    /// Override the exploration slash command.
    #[arg(long, env = "OPSX_BUILD_EXPLORE_COMMAND", value_name = "COMMAND")]
    explore_command: Option<String>,

    /// Override the proposal slash command.
    #[arg(long, env = "OPSX_BUILD_PROPOSE_COMMAND", value_name = "COMMAND")]
    propose_command: Option<String>,

    /// Override the OpenSpec apply slash command.
    #[arg(long, env = "OPSX_BUILD_APPLY_COMMAND", value_name = "COMMAND")]
    apply_command: Option<String>,

    /// Override the OpenSpec verify slash command.
    #[arg(long, env = "OPSX_BUILD_VERIFY_COMMAND", value_name = "COMMAND")]
    verify_command: Option<String>,

    /// Override the OpenSpec archive slash command.
    #[arg(long, env = "OPSX_BUILD_ARCHIVE_COMMAND", value_name = "COMMAND")]
    archive_command: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    max_verify_retries: Option<u32>,
    max_output_retries: Option<u32>,
    max_provider_retries: Option<u32>,
    local_worker_timeout_minutes: Option<std::num::NonZeroU32>,
    sidecar: Option<bool>,
    sidecar_root: Option<PathBuf>,
    local_only: Option<bool>,
    yolo: Option<bool>,
    #[serde(rename = "loop")]
    loop_workflow: Option<bool>,
    max_iterations: Option<std::num::NonZeroU32>,
    permission_mode: Option<String>,
    claude_command: Option<String>,
    claude_model: Option<String>,
    frontier_command: Option<String>,
    frontier_model: Option<String>,
    worker_connection: Option<String>,
    frontier_connection: Option<String>,
    connections: BTreeMap<String, FileConnection>,
    environments: BTreeMap<String, FileEnvironment>,
    auto_compact_window: Option<TokenCount>,
    auto_compact_percent: Option<Percentage>,
    max_output_tokens: Option<TokenCount>,
    explore_command: Option<String>,
    propose_command: Option<String>,
    apply_command: Option<String>,
    verify_command: Option<String>,
    archive_command: Option<String>,
    stream_claude: Option<StreamFilter>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConnection {
    backend: BackendKind,
    command: Option<String>,
    model: Option<String>,
    permission_profile: Option<String>,
    environment: Option<String>,
    context_window: Option<TokenCount>,
    auto_compact_window: Option<TokenCount>,
    auto_compact_percent: Option<Percentage>,
    max_output_tokens: Option<TokenCount>,
    env: BTreeMap<String, ConnectionEnvironmentValue>,
    isolate: Option<bool>,
    unset_env: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileEnvironment {
    env: BTreeMap<String, ConnectionEnvironmentValue>,
    isolate: Option<bool>,
    unset_env: Vec<String>,
}

fn load_config(args: &CliArgs) -> Result<(FileConfig, Option<PathBuf>)> {
    if args.no_config {
        return Ok((FileConfig::default(), None));
    }

    if let Some(path) = &args.config {
        return read_config(path).map(|config| (config, Some(path.clone())));
    }

    if let Some(path) = legacy_env::<PathBuf>("OSPX_BUILD_CONFIG")? {
        return read_config(&path).map(|config| (config, Some(path)));
    }

    let Some(path) = default_config_path() else {
        return Ok((FileConfig::default(), None));
    };
    if !path.exists() {
        return Ok((FileConfig::default(), None));
    }
    read_config(&path).map(|config| (config, Some(path)))
}

fn read_config(path: &PathBuf) -> Result<FileConfig> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("could not read config file `{}`", path.display()))?;
    toml::from_str(&text).with_context(|| format!("invalid config file `{}`", path.display()))
}

fn default_config_path() -> Option<PathBuf> {
    let root = if let Some(root) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty())
    {
        PathBuf::from(root)
    } else {
        env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(|home| PathBuf::from(home).join(".config"))?
    };
    Some(config_path_under(&root))
}

fn config_path_under(root: &std::path::Path) -> PathBuf {
    let canonical = root.join("opsx-build/config.toml");
    let legacy = root.join("ospx-build/config.toml");
    if !canonical.exists() && legacy.exists() {
        legacy
    } else {
        canonical
    }
}

fn apply_legacy_environment(config: &mut FileConfig) -> Result<()> {
    macro_rules! override_from_legacy {
        ($field:ident, $name:literal, $kind:ty) => {
            if let Some(value) = legacy_env::<$kind>($name)? {
                config.$field = Some(value);
            }
        };
    }

    override_from_legacy!(max_verify_retries, "OSPX_BUILD_MAX_VERIFY_RETRIES", u32);
    override_from_legacy!(max_output_retries, "OSPX_BUILD_MAX_OUTPUT_RETRIES", u32);
    override_from_legacy!(max_provider_retries, "OSPX_BUILD_MAX_PROVIDER_RETRIES", u32);
    override_from_legacy!(
        local_worker_timeout_minutes,
        "OSPX_BUILD_LOCAL_WORKER_TIMEOUT_MINUTES",
        std::num::NonZeroU32
    );
    override_from_legacy!(loop_workflow, "OSPX_BUILD_LOOP", bool);
    override_from_legacy!(yolo, "OSPX_BUILD_YOLO", bool);
    override_from_legacy!(
        max_iterations,
        "OSPX_BUILD_MAX_ITERATIONS",
        std::num::NonZeroU32
    );
    override_from_legacy!(permission_mode, "OSPX_BUILD_PERMISSION_MODE", String);
    override_from_legacy!(claude_command, "OSPX_BUILD_CLAUDE_COMMAND", String);
    override_from_legacy!(claude_model, "OSPX_BUILD_CLAUDE_MODEL", String);
    override_from_legacy!(frontier_command, "OSPX_BUILD_FRONTIER_COMMAND", String);
    override_from_legacy!(frontier_model, "OSPX_BUILD_FRONTIER_MODEL", String);
    override_from_legacy!(worker_connection, "OSPX_BUILD_WORKER_CONNECTION", String);
    override_from_legacy!(
        frontier_connection,
        "OSPX_BUILD_FRONTIER_CONNECTION",
        String
    );
    override_from_legacy!(
        auto_compact_window,
        "OSPX_BUILD_AUTO_COMPACT_WINDOW",
        TokenCount
    );
    override_from_legacy!(
        auto_compact_percent,
        "OSPX_BUILD_AUTO_COMPACT_PERCENT",
        Percentage
    );
    override_from_legacy!(
        max_output_tokens,
        "OSPX_BUILD_MAX_OUTPUT_TOKENS",
        TokenCount
    );
    override_from_legacy!(explore_command, "OSPX_BUILD_EXPLORE_COMMAND", String);
    override_from_legacy!(propose_command, "OSPX_BUILD_PROPOSE_COMMAND", String);
    override_from_legacy!(apply_command, "OSPX_BUILD_APPLY_COMMAND", String);
    override_from_legacy!(verify_command, "OSPX_BUILD_VERIFY_COMMAND", String);
    override_from_legacy!(archive_command, "OSPX_BUILD_ARCHIVE_COMMAND", String);

    if let Some(value) = env::var_os("OSPX_BUILD_STREAM_CLAUDE") {
        let value = value.into_string().map_err(|_| {
            anyhow::anyhow!("legacy environment variable OSPX_BUILD_STREAM_CLAUDE is not UTF-8")
        })?;
        config.stream_claude = Some(match value.as_str() {
            "activity" => StreamFilter::Activity,
            "full" => StreamFilter::Full,
            "raw" => StreamFilter::Raw,
            _ => anyhow::bail!(
                "invalid legacy environment variable OSPX_BUILD_STREAM_CLAUDE={value:?}; expected activity, full, or raw"
            ),
        });
    }
    Ok(())
}

fn legacy_env<T>(name: &str) -> Result<Option<T>>
where
    T: FromStr,
    T::Err: Display,
{
    let Some(value) = env::var_os(name) else {
        return Ok(None);
    };
    let value = value
        .into_string()
        .map_err(|_| anyhow::anyhow!("legacy environment variable {name} is not UTF-8"))?;
    value.parse().map(Some).map_err(|error| {
        anyhow::anyhow!("invalid legacy environment variable {name}={value:?}: {error}")
    })
}

fn resolve_values(args: CliArgs, config: FileConfig, config_path: Option<PathBuf>) -> Result<Cli> {
    let bootstrap_defines = parse_bootstrap_defines(&args.define)?;
    let worker_profile = selected_connection(
        args.test_connection.as_deref().or(args
            .worker_connection
            .as_deref()
            .or(config.worker_connection.as_deref())),
        &config.connections,
    )?;
    let frontier_profile = selected_connection(
        args.frontier_connection
            .as_deref()
            .or(config.frontier_connection.as_deref()),
        &config.connections,
    )?;
    let worker_connection = resolve_connection(
        worker_profile,
        &config.environments,
        args.claude_command.clone(),
        args.claude_model.clone(),
        config.claude_command.clone(),
        config.claude_model.clone(),
        args.auto_compact_window,
        args.auto_compact_percent,
        args.max_output_tokens,
        config.auto_compact_window,
        config.auto_compact_percent,
        config.max_output_tokens,
        DEFAULT_CLAUDE_COMMAND,
    )?;
    let frontier_connection = resolve_connection(
        frontier_profile,
        &config.environments,
        args.frontier_command.clone(),
        args.frontier_model.clone(),
        config.frontier_command.clone(),
        config.frontier_model.clone(),
        None,
        None,
        None,
        None,
        None,
        None,
        DEFAULT_FRONTIER_COMMAND,
    )?;
    let request = match args.request.as_deref() {
        Some(value) if value.trim().eq_ignore_ascii_case("next slice") => "advance".to_owned(),
        Some(value) => value.to_owned(),
        None if args.interactive
            || args.test_connection.is_some()
            || args.update_skills
            || args.resume
            || args.forget
            || args.continue_existing =>
        {
            String::new()
        }
        None => "advance".to_owned(),
    };
    let execute = request == "execute";
    let request = if execute {
        "advance".to_owned()
    } else {
        request
    };
    let rewind_target = if request == "rewind" {
        Some(
            args.rewind_target
                .clone()
                .unwrap_or_else(|| "post-bootstrap".to_owned()),
        )
    } else {
        args.rewind_target.clone()
    };
    Ok(Cli {
        request,
        execute,
        rewind_target,
        yes: args.yes,
        bootstrap_context: args.context,
        bootstrap_defines,
        repo: args.repo,
        sidecar: args.sidecar || config.sidecar.unwrap_or(false),
        sidecar_root: args.sidecar_root.or(config.sidecar_root),
        local_only: args.local_only || config.local_only.unwrap_or(false),
        yolo: !args.no_yolo && (args.yolo || config.yolo.unwrap_or(false)),
        interactive: args.interactive,
        test_connection: args.test_connection,
        basic_connection_test: args.basic_connection_test,
        update_skills: args.update_skills,
        resume: args.resume,
        forget: args.forget,
        continue_existing: args.continue_existing,
        loop_workflow: !args.no_loop
            && (execute || args.loop_workflow || config.loop_workflow.unwrap_or(false)),
        no_loop: args.no_loop,
        max_iterations: args
            .max_iterations
            .or(config.max_iterations)
            .map(std::num::NonZeroU32::get),
        change: args.change,
        direction: args.direction,
        interactive_args: args.interactive_args,
        max_verify_retries: args
            .max_verify_retries
            .or(config.max_verify_retries)
            .unwrap_or(DEFAULT_MAX_VERIFY_RETRIES),
        max_output_retries: args
            .max_output_retries
            .or(config.max_output_retries)
            .unwrap_or(DEFAULT_MAX_OUTPUT_RETRIES),
        max_provider_retries: args
            .max_provider_retries
            .or(config.max_provider_retries)
            .unwrap_or(DEFAULT_MAX_PROVIDER_RETRIES),
        local_worker_timeout_minutes: args
            .local_worker_timeout_minutes
            .or(config.local_worker_timeout_minutes)
            .map(std::num::NonZeroU32::get)
            .unwrap_or(DEFAULT_LOCAL_WORKER_TIMEOUT_MINUTES),
        verbose: args.verbose,
        debug: args.debug,
        stream_claude: args.stream_claude.or(config.stream_claude),
        dry_run: args.dry_run,
        permission_mode: args
            .permission_mode
            .or(config.permission_mode)
            .unwrap_or_else(|| DEFAULT_PERMISSION_MODE.to_owned()),
        worker_connection,
        frontier_connection,
        explore_command: args.explore_command.or(config.explore_command),
        propose_command: args.propose_command.or(config.propose_command),
        apply_command: args.apply_command.or(config.apply_command),
        verify_command: args.verify_command.or(config.verify_command),
        archive_command: args.archive_command.or(config.archive_command),
        config_path,
    })
}

fn parse_bootstrap_defines(values: &[String]) -> Result<BTreeMap<String, String>> {
    let mut definitions = BTreeMap::new();
    for definition in values {
        let (name, value) = definition
            .split_once('=')
            .with_context(|| format!("invalid --define {definition:?}; expected NAME=VALUE"))?;
        if !valid_template_name(name) {
            anyhow::bail!(
                "invalid bootstrap variable name `{name}`; use letters, digits, and underscores, beginning with a letter or underscore"
            );
        }
        if definitions
            .insert(name.to_owned(), value.to_owned())
            .is_some()
        {
            anyhow::bail!("bootstrap variable `{name}` was defined more than once");
        }
    }
    Ok(definitions)
}

fn valid_template_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn selected_connection(
    name: Option<&str>,
    connections: &BTreeMap<String, FileConnection>,
) -> Result<Option<(String, FileConnection)>> {
    let Some(name) = name else {
        return Ok(None);
    };
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("connection profile name cannot be empty");
    }
    let profile = connections.get(name).cloned().with_context(|| {
        let available = if connections.is_empty() {
            "none are configured".to_owned()
        } else {
            format!(
                "available profiles: {}",
                connections.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        };
        format!("unknown agent connection profile `{name}`; {available}")
    })?;
    Ok(Some((name.to_owned(), profile)))
}

#[allow(clippy::too_many_arguments)]
fn resolve_connection(
    profile: Option<(String, FileConnection)>,
    environments: &BTreeMap<String, FileEnvironment>,
    command_line_command: Option<String>,
    command_line_model: Option<String>,
    legacy_command: Option<String>,
    legacy_model: Option<String>,
    command_line_window: Option<TokenCount>,
    command_line_percentage: Option<Percentage>,
    command_line_output_tokens: Option<TokenCount>,
    legacy_window: Option<TokenCount>,
    legacy_percentage: Option<Percentage>,
    legacy_output_tokens: Option<TokenCount>,
    default_command: &str,
) -> Result<AgentConnection> {
    let (name, profile) = profile
        .map(|(name, profile)| (Some(name), profile))
        .unwrap_or_default();
    let (environment_name, shared_environment) = selected_environment(
        profile.environment.as_deref(),
        environments,
        name.as_deref(),
    )?;
    let mut environment = shared_environment.env;
    environment.extend(profile.env);
    let isolate = name.is_some()
        && profile
            .isolate
            .or(shared_environment.isolate)
            .unwrap_or(true);
    let mut unset_env = shared_environment.unset_env;
    for variable in profile.unset_env {
        if !unset_env.contains(&variable) {
            unset_env.push(variable);
        }
    }
    let backend = profile.backend;
    if profile.permission_profile.is_some() && backend != BackendKind::Codex {
        anyhow::bail!(
            "connection profile `{}` sets permission_profile, which is supported only by the Codex backend",
            name.as_deref().unwrap_or("unnamed")
        );
    }
    Ok(AgentConnection {
        name,
        backend,
        environment_name,
        command: command_line_command
            .or(profile.command)
            .or(legacy_command)
            .unwrap_or_else(|| {
                if backend == BackendKind::Claude {
                    default_command.to_owned()
                } else {
                    backend.default_command().to_owned()
                }
            }),
        model: command_line_model.or(profile.model).or(legacy_model),
        permission_profile: profile.permission_profile,
        context_window: profile.context_window.map(|count| count.0),
        auto_compact_window: command_line_window
            .or(profile.auto_compact_window)
            .or(legacy_window)
            .map(|count| count.0),
        auto_compact_percent: command_line_percentage
            .or(profile.auto_compact_percent)
            .or(legacy_percentage)
            .map(|percentage| percentage.0),
        max_output_tokens: command_line_output_tokens
            .or(profile.max_output_tokens)
            .or(legacy_output_tokens)
            .map(|count| count.0),
        env: environment,
        isolate,
        unset_env,
    })
}

fn selected_environment(
    name: Option<&str>,
    environments: &BTreeMap<String, FileEnvironment>,
    connection_name: Option<&str>,
) -> Result<(Option<String>, FileEnvironment)> {
    let Some(name) = name else {
        return Ok((None, FileEnvironment::default()));
    };
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("environment profile name cannot be empty");
    }
    let environment = environments.get(name).cloned().with_context(|| {
        let available = if environments.is_empty() {
            "none are configured".to_owned()
        } else {
            format!(
                "available environments: {}",
                environments.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        };
        format!(
            "Agent connection profile `{}` references unknown environment profile `{name}`; {available}",
            connection_name.unwrap_or("unnamed")
        )
    })?;
    Ok((Some(name.to_owned()), environment))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TokenCount(u64);

impl std::str::FromStr for TokenCount {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_token_count(value).map(Self)
    }
}

impl<'de> Deserialize<'de> for TokenCount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Value {
            Integer(u64),
            Text(String),
        }

        match Value::deserialize(deserializer)? {
            Value::Integer(value) if value > 0 => Ok(Self(value)),
            Value::Integer(_) => Err(serde::de::Error::custom(
                "token count must be greater than zero",
            )),
            Value::Text(value) => value.parse().map_err(serde::de::Error::custom),
        }
    }
}

fn parse_token_count(value: &str) -> Result<u64, String> {
    let normalized = value.trim().replace('_', "").to_ascii_lowercase();
    if normalized.ends_with('%') {
        return Err("use --auto-compact-percent for percentage thresholds".to_owned());
    }
    let (digits, multiplier) = if let Some(digits) = normalized.strip_suffix('k') {
        (digits, 1_024_u64)
    } else if let Some(digits) = normalized.strip_suffix('m') {
        (digits, 1_048_576_u64)
    } else {
        (normalized.as_str(), 1_u64)
    };
    let count = digits
        .parse::<u64>()
        .map_err(|_| format!("invalid token count `{value}`"))?;
    let count = count
        .checked_mul(multiplier)
        .ok_or_else(|| format!("token count `{value}` is too large"))?;
    if count == 0 {
        return Err("token count must be greater than zero".to_owned());
    }
    Ok(count)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Percentage(u8);

impl std::str::FromStr for Percentage {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_percentage(value).map(Self)
    }
}

impl<'de> Deserialize<'de> for Percentage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Value {
            Integer(u8),
            Text(String),
        }

        match Value::deserialize(deserializer)? {
            Value::Integer(value) => parse_percentage(&value.to_string())
                .map(Self)
                .map_err(serde::de::Error::custom),
            Value::Text(value) => value.parse().map_err(serde::de::Error::custom),
        }
    }
}

fn parse_percentage(value: &str) -> Result<u8, String> {
    let normalized = value.trim().strip_suffix('%').unwrap_or(value.trim());
    let percentage = normalized
        .parse::<u8>()
        .map_err(|_| format!("invalid percentage `{value}`"))?;
    if !(1..=100).contains(&percentage) {
        return Err("percentage must be between 1 and 100".to_owned());
    }
    Ok(percentage)
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs};

    use super::*;
    use clap::CommandFactory;
    use uuid::Uuid;

    fn args(values: impl IntoIterator<Item = impl Into<OsString> + Clone>) -> CliArgs {
        CliArgs::try_parse_from(values).unwrap()
    }

    #[test]
    fn uses_the_corrected_program_name() {
        assert_eq!(CliArgs::command().get_name(), "opsx-build");
    }

    #[test]
    fn resolves_a_bounded_sidecar_request() {
        let cli = Cli::resolve(args([
            "opsx-build",
            "--no-config",
            "--sidecar",
            "--local-only",
            "--sidecar-root",
            "/tmp/opsx-sidecars",
            "--context",
            "/tmp/ceph-change.md",
            "--change",
            "ceph-bounded-fix",
            "fix the bounded Ceph behaviour",
        ]))
        .unwrap();

        assert!(cli.sidecar);
        assert!(cli.local_only);
        assert_eq!(cli.sidecar_root, Some(PathBuf::from("/tmp/opsx-sidecars")));
        assert_eq!(
            cli.bootstrap_context,
            Some(PathBuf::from("/tmp/ceph-change.md"))
        );
        assert_eq!(cli.change.as_deref(), Some("ceph-bounded-fix"));
        assert_eq!(cli.request, "fix the bounded Ceph behaviour");
    }

    #[test]
    fn sidecar_requires_a_named_bounded_change() {
        let error = Cli::resolve(args([
            "opsx-build",
            "--no-config",
            "--sidecar",
            "--context",
            "/tmp/context.md",
            "--dry-run",
        ]))
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("explicit bounded change request")
        );

        let error = Cli::resolve(args([
            "opsx-build",
            "--no-config",
            "--sidecar",
            "--context",
            "/tmp/context.md",
            "bounded request",
        ]))
        .unwrap_err();
        assert!(error.to_string().contains("requires --change NAME"));
    }

    #[test]
    fn reads_sidecar_defaults_from_config() {
        let directory = env::temp_dir().join(format!("opsx-config-test-{}", Uuid::new_v4()));
        let config = directory.join("config.toml");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            &config,
            "sidecar = true\nsidecar_root = '/tmp/stores'\nlocal_only = true\n",
        )
        .unwrap();

        let cli = Cli::resolve(args([
            OsString::from("opsx-build"),
            OsString::from("--config"),
            config.as_os_str().to_owned(),
            OsString::from("--context"),
            OsString::from("/tmp/context.md"),
            OsString::from("--change"),
            OsString::from("ceph-fix"),
            OsString::from("bounded request"),
        ]))
        .unwrap();

        assert!(cli.sidecar);
        assert!(cli.local_only);
        assert_eq!(cli.sidecar_root, Some(PathBuf::from("/tmp/stores")));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn canonical_config_path_falls_back_to_the_legacy_location() {
        let directory = env::temp_dir().join(format!("opsx-config-test-{}", Uuid::new_v4()));
        let legacy = directory.join("ospx-build/config.toml");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "").unwrap();
        assert_eq!(config_path_under(&directory), legacy);

        let canonical = directory.join("opsx-build/config.toml");
        fs::create_dir_all(canonical.parent().unwrap()).unwrap();
        fs::write(&canonical, "").unwrap();
        assert_eq!(config_path_under(&directory), canonical);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn parses_toml_defaults() {
        let config: FileConfig = toml::from_str(
            r#"
                max_verify_retries = 5
                max_output_retries = 7
                max_provider_retries = 4
                local_worker_timeout_minutes = 45
                loop = true
                yolo = true
                max_iterations = 12
                permission_mode = "dontAsk"
                claude_command = "omlx launch claude"
                claude_model = "local-model"
                frontier_command = "frontier-wrapper claude"
                frontier_model = "frontier-model"
                auto_compact_window = "192k"
                auto_compact_percent = "75%"
                max_output_tokens = "8k"
                verify_command = "/opsx:verify"
                stream_claude = "full"
            "#,
        )
        .unwrap();
        let cli = resolve_values(
            args(["opsx-build", "build something"]),
            config,
            Some(PathBuf::from("config.toml")),
        )
        .unwrap();

        assert_eq!(cli.max_verify_retries, 5);
        assert_eq!(cli.max_output_retries, 7);
        assert_eq!(cli.max_provider_retries, 4);
        assert_eq!(cli.local_worker_timeout_minutes, 45);
        assert!(cli.loop_workflow);
        assert!(cli.yolo);
        assert_eq!(cli.max_iterations, Some(12));
        assert_eq!(cli.permission_mode, "dontAsk");
        assert_eq!(cli.worker_connection.command, "omlx launch claude");
        assert_eq!(cli.worker_connection.model.as_deref(), Some("local-model"));
        assert_eq!(cli.frontier_connection.command, "frontier-wrapper claude");
        assert_eq!(
            cli.frontier_connection.model.as_deref(),
            Some("frontier-model")
        );
        assert_eq!(cli.worker_connection.auto_compact_window, Some(196_608));
        assert_eq!(cli.worker_connection.auto_compact_percent, Some(75));
        assert_eq!(cli.worker_connection.max_output_tokens, Some(8_192));
        assert_eq!(cli.verify_command.as_deref(), Some("/opsx:verify"));
        assert_eq!(cli.stream_claude, Some(StreamFilter::Full));
    }

    #[test]
    fn resolves_named_worker_and_frontier_connections() {
        let config: FileConfig = toml::from_str(
            r#"
                worker_connection = "local"
                frontier_connection = "kimi"

                [connections.local]
                command = "omlx launch claude"
                model = "qwen-local"
                context_window = "256k"
                auto_compact_window = "192k"
                max_output_tokens = "8k"

                [connections.local.env]
                OMLX_HOST = "http://localhost:8000"

                [connections.kimi]
                command = "claude"
                model = "kimi-k3"
                context_window = "128k"
                environment = "openrouter"

                [environments.openrouter]
                unset_env = ["HTTP_PROXY"]

                [environments.openrouter.env]
                ANTHROPIC_BASE_URL = "https://provider.example/anthropic"
                ANTHROPIC_AUTH_TOKEN = { from_env = "KIMI_API_KEY" }
            "#,
        )
        .unwrap();
        let cli = resolve_values(
            args(["opsx-build", "build something"]),
            config,
            Some(PathBuf::from("config.toml")),
        )
        .unwrap();

        assert_eq!(cli.worker_connection.name.as_deref(), Some("local"));
        assert_eq!(cli.worker_connection.command, "omlx launch claude");
        assert_eq!(cli.worker_connection.model.as_deref(), Some("qwen-local"));
        assert_eq!(cli.worker_connection.context_window, Some(262_144));
        assert_eq!(cli.worker_connection.auto_compact_window, Some(196_608));
        assert_eq!(cli.worker_connection.max_output_tokens, Some(8_192));
        assert!(cli.worker_connection.isolate);
        assert_eq!(
            cli.worker_connection.env.get("OMLX_HOST"),
            Some(&ConnectionEnvironmentValue::Literal(
                "http://localhost:8000".to_owned()
            ))
        );

        assert_eq!(cli.frontier_connection.name.as_deref(), Some("kimi"));
        assert_eq!(
            cli.frontier_connection.environment_name.as_deref(),
            Some("openrouter")
        );
        assert_eq!(cli.frontier_connection.model.as_deref(), Some("kimi-k3"));
        assert_eq!(cli.frontier_connection.context_window, Some(131_072));
        assert_eq!(cli.frontier_connection.unset_env, ["HTTP_PROXY"]);
        assert!(matches!(
            cli.frontier_connection.env.get("ANTHROPIC_AUTH_TOKEN"),
            Some(ConnectionEnvironmentValue::FromEnvironment(reference))
                if reference.from_env == "KIMI_API_KEY"
        ));
    }

    #[test]
    fn connection_profiles_select_an_explicit_backend() {
        let config: FileConfig = toml::from_str(
            r#"
                worker_connection = "open"
                frontier_connection = "frontier"

                [connections.open]
                backend = "opencode"
                model = "provider/worker"

                [connections.frontier]
                command = "claude-wrapper"
            "#,
        )
        .unwrap();
        let cli = resolve_values(
            args(["opsx-build", "build something"]),
            config,
            Some(PathBuf::from("config.toml")),
        )
        .unwrap();

        assert_eq!(cli.worker_connection.backend, BackendKind::OpenCode);
        assert_eq!(cli.worker_connection.command, "opencode");
        assert_eq!(cli.frontier_connection.backend, BackendKind::Claude);
        assert_eq!(cli.frontier_connection.command, "claude-wrapper");
    }

    #[test]
    fn codex_connection_uses_the_codex_default_command() {
        let config: FileConfig = toml::from_str(
            r#"
                worker_connection = "codex-worker"
                frontier_connection = "codex-frontier"

                [connections.codex-worker]
                backend = "codex"
                model = "gpt-5.6-codex"
                permission_profile = "opsx-build"
                context_window = "200k"
                auto_compact_percent = 75

                [connections.codex-frontier]
                backend = "codex"
                command = "codex --profile frontier"
                model = "gpt-frontier"
            "#,
        )
        .unwrap();
        let cli = resolve_values(
            args(["opsx-build", "build something"]),
            config,
            Some(PathBuf::from("config.toml")),
        )
        .unwrap();

        assert_eq!(cli.worker_connection.backend, BackendKind::Codex);
        assert_eq!(cli.worker_connection.command, "codex");
        assert_eq!(
            cli.worker_connection.permission_profile.as_deref(),
            Some("opsx-build")
        );
        assert_eq!(
            cli.worker_connection.model.as_deref(),
            Some("gpt-5.6-codex")
        );
        assert_eq!(cli.worker_connection.context_window, Some(204_800));
        assert_eq!(cli.worker_connection.auto_compact_percent, Some(75));
        assert_eq!(cli.frontier_connection.backend, BackendKind::Codex);
        assert_eq!(cli.frontier_connection.command, "codex --profile frontier");
        assert_eq!(
            cli.frontier_connection.model.as_deref(),
            Some("gpt-frontier")
        );
    }

    #[test]
    fn permission_profiles_are_codex_only() {
        let config: FileConfig = toml::from_str(
            r#"
                worker_connection = "claude-worker"

                [connections.claude-worker]
                backend = "claude"
                permission_profile = "opsx-build"
            "#,
        )
        .unwrap();
        let error = resolve_values(
            args(["opsx-build", "build something"]),
            config,
            Some(PathBuf::from("config.toml")),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("supported only by the Codex backend")
        );
    }

    #[test]
    fn multiple_connections_share_one_environment_profile() {
        let config: FileConfig = toml::from_str(
            r#"
                worker_connection = "openrouter-kimi"
                frontier_connection = "openrouter-gemini"

                [environments.openrouter.env]
                ANTHROPIC_BASE_URL = "https://openrouter.ai/api"
                ANTHROPIC_AUTH_TOKEN = { from_env = "OPENROUTER_API_KEY" }
                ANTHROPIC_API_KEY = ""

                [connections.openrouter-kimi]
                command = "claude"
                model = "moonshotai/kimi-k3"
                environment = "openrouter"

                [connections.openrouter-gemini]
                command = "claude"
                model = "google/gemini"
                environment = "openrouter"
            "#,
        )
        .unwrap();
        let cli = resolve_values(args(["opsx-build", "build something"]), config, None).unwrap();

        for connection in [&cli.worker_connection, &cli.frontier_connection] {
            assert_eq!(connection.environment_name.as_deref(), Some("openrouter"));
            assert_eq!(
                connection.env.get("ANTHROPIC_BASE_URL"),
                Some(&ConnectionEnvironmentValue::Literal(
                    "https://openrouter.ai/api".to_owned()
                ))
            );
            assert_eq!(
                connection.env.get("ANTHROPIC_API_KEY"),
                Some(&ConnectionEnvironmentValue::Literal(String::new()))
            );
        }
        assert_eq!(
            cli.worker_connection.model.as_deref(),
            Some("moonshotai/kimi-k3")
        );
        assert_eq!(
            cli.frontier_connection.model.as_deref(),
            Some("google/gemini")
        );
    }

    #[test]
    fn connection_environment_overrides_shared_environment_values() {
        let config: FileConfig = toml::from_str(
            r#"
                frontier_connection = "custom"

                [environments.provider.env]
                ROUTING_HINT = "shared"

                [connections.custom]
                environment = "provider"

                [connections.custom.env]
                ROUTING_HINT = "connection-specific"
            "#,
        )
        .unwrap();
        let cli = resolve_values(args(["opsx-build", "build something"]), config, None).unwrap();

        assert_eq!(
            cli.frontier_connection.env.get("ROUTING_HINT"),
            Some(&ConnectionEnvironmentValue::Literal(
                "connection-specific".to_owned()
            ))
        );
    }

    #[test]
    fn rejects_an_unknown_shared_environment_profile() {
        let config: FileConfig = toml::from_str(
            r#"
                frontier_connection = "hosted"

                [connections.hosted]
                environment = "missing"

                [environments.openrouter.env]
                ANTHROPIC_BASE_URL = "https://openrouter.ai/api"
            "#,
        )
        .unwrap();
        let error =
            resolve_values(args(["opsx-build", "build something"]), config, None).unwrap_err();

        assert!(error.to_string().contains(
            "connection profile `hosted` references unknown environment profile `missing`"
        ));
        assert!(
            error
                .to_string()
                .contains("available environments: openrouter")
        );
    }

    #[test]
    fn command_line_connection_and_model_override_config_selection() {
        let config: FileConfig = toml::from_str(
            r#"
                frontier_connection = "hosted"

                [connections.hosted]
                command = "hosted-claude"
                model = "hosted-default"

                [connections.local-large]
                command = "omlx launch claude"
                model = "large-default"
            "#,
        )
        .unwrap();
        let cli = resolve_values(
            args([
                "opsx-build",
                "--frontier-connection",
                "local-large",
                "--frontier-model",
                "specific-large-model",
                "build something",
            ]),
            config,
            None,
        )
        .unwrap();

        assert_eq!(cli.frontier_connection.name.as_deref(), Some("local-large"));
        assert_eq!(cli.frontier_connection.command, "omlx launch claude");
        assert_eq!(
            cli.frontier_connection.model.as_deref(),
            Some("specific-large-model")
        );
    }

    #[test]
    fn rejects_an_unknown_connection_profile_with_available_names() {
        let config: FileConfig = toml::from_str(
            r#"
                worker_connection = "missing"

                [connections.local]
                command = "claude"
            "#,
        )
        .unwrap();
        let error =
            resolve_values(args(["opsx-build", "build something"]), config, None).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unknown agent connection profile `missing`")
        );
        assert!(error.to_string().contains("available profiles: local"));
    }

    #[test]
    fn command_line_values_override_file_values() {
        let config: FileConfig = toml::from_str(
            r#"
                max_verify_retries = 5
                max_output_retries = 6
                max_provider_retries = 7
                local_worker_timeout_minutes = 90
                claude_model = "config-model"
                frontier_command = "configured-frontier"
                frontier_model = "configured-frontier-model"
                auto_compact_window = 131072
                auto_compact_percent = 80
                max_output_tokens = 16384
            "#,
        )
        .unwrap();
        let cli = resolve_values(
            args([
                "opsx-build",
                "--max-verify-retries",
                "2",
                "--max-output-retries",
                "4",
                "--max-provider-retries",
                "2",
                "--local-worker-timeout-minutes",
                "30",
                "--claude-model",
                "cli-model",
                "--frontier-command",
                "cli-frontier",
                "--frontier-model",
                "cli-frontier-model",
                "--auto-compact-window",
                "200k",
                "--auto-compact-percent",
                "50",
                "--max-output-tokens",
                "12k",
                "build something",
            ]),
            config,
            None,
        )
        .unwrap();

        assert_eq!(cli.max_verify_retries, 2);
        assert_eq!(cli.max_output_retries, 4);
        assert_eq!(cli.max_provider_retries, 2);
        assert_eq!(cli.local_worker_timeout_minutes, 30);
        assert_eq!(cli.worker_connection.model.as_deref(), Some("cli-model"));
        assert_eq!(cli.frontier_connection.command, "cli-frontier");
        assert_eq!(
            cli.frontier_connection.model.as_deref(),
            Some("cli-frontier-model")
        );
        assert_eq!(cli.worker_connection.auto_compact_window, Some(204_800));
        assert_eq!(cli.worker_connection.auto_compact_percent, Some(50));
        assert_eq!(cli.worker_connection.max_output_tokens, Some(12_288));
    }

    #[test]
    fn rejects_unknown_config_keys() {
        let error = toml::from_str::<FileConfig>("claude_modle = 'typo'").unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn uses_builtin_defaults_without_a_file() {
        let cli = resolve_values(
            args(["opsx-build", "build something"]),
            FileConfig::default(),
            None,
        )
        .unwrap();
        assert_eq!(cli.max_verify_retries, DEFAULT_MAX_VERIFY_RETRIES);
        assert_eq!(cli.max_output_retries, DEFAULT_MAX_OUTPUT_RETRIES);
        assert_eq!(cli.max_provider_retries, DEFAULT_MAX_PROVIDER_RETRIES);
        assert_eq!(
            cli.local_worker_timeout_minutes,
            DEFAULT_LOCAL_WORKER_TIMEOUT_MINUTES
        );
        assert!(!cli.loop_workflow);
        assert!(!cli.yolo);
        assert_eq!(cli.max_iterations, None);
        assert_eq!(cli.permission_mode, DEFAULT_PERMISSION_MODE);
        assert_eq!(cli.worker_connection.command, DEFAULT_CLAUDE_COMMAND);
        assert_eq!(cli.frontier_connection.command, DEFAULT_FRONTIER_COMMAND);
        assert_eq!(cli.frontier_connection.model, None);
        assert_eq!(cli.worker_connection.context_window, None);
        assert_eq!(cli.worker_connection.auto_compact_window, None);
        assert_eq!(cli.worker_connection.auto_compact_percent, None);
        assert_eq!(cli.worker_connection.max_output_tokens, None);
    }

    #[test]
    fn parses_auto_compact_windows_and_percentages() {
        assert_eq!(parse_token_count("196_608").unwrap(), 196_608);
        assert_eq!(parse_token_count("192k").unwrap(), 196_608);
        assert_eq!(parse_token_count("1M").unwrap(), 1_048_576);

        let percentage = parse_token_count("75%").unwrap_err();
        assert!(percentage.contains("--auto-compact-percent"));
        assert_eq!(parse_percentage("75").unwrap(), 75);
        assert_eq!(parse_percentage("50%").unwrap(), 50);
        assert!(parse_percentage("0").is_err());
        assert!(parse_percentage("101").is_err());
        assert!(
            CliArgs::try_parse_from([
                "opsx-build",
                "--auto-compact-window",
                "0",
                "build something"
            ])
            .is_err()
        );
        assert!(
            CliArgs::try_parse_from(["opsx-build", "--max-output-tokens", "0", "build something"])
                .is_err()
        );
    }

    #[test]
    fn parses_campaign_loop_and_positive_iteration_limit() {
        let cli = Cli::resolve(args([
            "opsx-build",
            "--loop",
            "--max-iterations",
            "8",
            "finish the compiler",
        ]))
        .unwrap();
        assert!(cli.loop_workflow);
        assert_eq!(cli.max_iterations, Some(8));

        assert!(
            CliArgs::try_parse_from([
                "opsx-build",
                "--loop",
                "--max-iterations",
                "0",
                "finish the compiler",
            ])
            .is_err()
        );
        assert!(
            Cli::resolve(args([
                "opsx-build",
                "--max-iterations",
                "2",
                "finish the compiler",
            ]))
            .is_err()
        );
    }

    #[test]
    fn enables_yolo_from_the_command_line() {
        let cli = Cli::resolve(args(["opsx-build", "--yolo", "advance"])).unwrap();
        assert!(cli.yolo);
    }

    #[test]
    fn rewind_defaults_to_the_post_bootstrap_tag_and_accepts_an_override() {
        let cli = Cli::resolve(args(["opsx-build", "rewind"])).unwrap();
        assert_eq!(cli.request, "rewind");
        assert_eq!(cli.rewind_target.as_deref(), Some("post-bootstrap"));
        assert!(!cli.yes);

        let cli = Cli::resolve(args([
            "opsx-build",
            "--yes",
            "rewind",
            "my-bootstrap-baseline",
        ]))
        .unwrap();
        assert_eq!(cli.rewind_target.as_deref(), Some("my-bootstrap-baseline"));
        assert!(cli.yes);
    }

    #[test]
    fn rewind_only_options_are_rejected_for_other_workflows() {
        assert!(Cli::resolve(args(["opsx-build", "build it", "some-ref"])).is_err());
        assert!(Cli::resolve(args(["opsx-build", "--yes", "advance"])).is_err());
    }

    #[test]
    fn no_yolo_restores_safe_commits_over_a_configured_default() {
        let config: FileConfig = toml::from_str("yolo = true").unwrap();
        let cli =
            resolve_values(args(["opsx-build", "--no-yolo", "advance"]), config, None).unwrap();
        assert!(!cli.yolo);
    }

    #[test]
    fn no_loop_overrides_a_configured_campaign_without_losing_other_defaults() {
        let config: FileConfig = toml::from_str(
            r#"
                loop = true
                max_iterations = 12
                claude_model = "configured-model"
            "#,
        )
        .unwrap();
        let cli = resolve_values(
            args(["opsx-build", "--no-loop", "one change"]),
            config,
            None,
        )
        .unwrap();
        assert!(!cli.loop_workflow);
        assert_eq!(cli.max_iterations, Some(12));
        assert_eq!(
            cli.worker_connection.model.as_deref(),
            Some("configured-model")
        );
    }

    #[test]
    fn loads_an_explicit_file_and_records_its_path() {
        let directory = env::temp_dir().join(format!("opsx-build-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.toml");
        fs::write(
            &path,
            "claude_command = 'omlx launch claude'\nclaude_model = 'configured-model'\n",
        )
        .unwrap();
        let cli = Cli::resolve(args(vec![
            OsString::from("opsx-build"),
            OsString::from("--config"),
            path.clone().into_os_string(),
            OsString::from("build something"),
        ]))
        .unwrap();

        assert_eq!(cli.config_path.as_ref(), Some(&path));
        assert_eq!(cli.worker_connection.command, "omlx launch claude");
        assert_eq!(
            cli.worker_connection.model.as_deref(),
            Some("configured-model")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn no_config_skips_even_an_explicit_missing_file() {
        let cli = Cli::resolve(args([
            "opsx-build",
            "--config",
            "/definitely/missing/config.toml",
            "--no-config",
            "build something",
        ]))
        .unwrap();
        assert!(cli.config_path.is_none());
        assert_eq!(cli.worker_connection.command, DEFAULT_CLAUDE_COMMAND);
    }

    #[test]
    fn interactive_mode_allows_no_initial_prompt_and_passthrough_args() {
        let cli = Cli::resolve(args([
            "opsx-build",
            "--interactive",
            "--",
            "--effort",
            "xhigh",
        ]))
        .unwrap();
        assert!(cli.interactive);
        assert!(cli.request.is_empty());
        assert_eq!(cli.interactive_args, ["--effort", "xhigh"]);
    }

    #[test]
    fn connection_test_selects_one_profile_without_a_build_request() {
        let config: FileConfig = toml::from_str(
            r#"
                worker_connection = "local"

                [connections.local]
                model = "local-model"

                [connections.openrouter-kimi]
                model = "moonshotai/kimi-k3"
            "#,
        )
        .unwrap();
        let cli = resolve_values(
            args(["opsx-build", "--test-connection", "openrouter-kimi"]),
            config,
            None,
        )
        .unwrap();

        assert_eq!(cli.test_connection.as_deref(), Some("openrouter-kimi"));
        assert!(!cli.basic_connection_test);
        assert!(cli.request.is_empty());
        assert_eq!(
            cli.worker_connection.name.as_deref(),
            Some("openrouter-kimi")
        );
        assert_eq!(
            cli.worker_connection.model.as_deref(),
            Some("moonshotai/kimi-k3")
        );
        assert!(
            CliArgs::try_parse_from([
                "opsx-build",
                "--test-connection",
                "openrouter-kimi",
                "build something",
            ])
            .is_err()
        );
        let basic = resolve_values(
            args([
                "opsx-build",
                "--test-connection",
                "openrouter-kimi",
                "--basic-connection-test",
            ]),
            toml::from_str(
                r#"
                    [connections.openrouter-kimi]
                    model = "moonshotai/kimi-k3"
                "#,
            )
            .unwrap(),
            None,
        )
        .unwrap();
        assert!(basic.basic_connection_test);
        assert!(CliArgs::try_parse_from(["opsx-build", "--basic-connection-test"]).is_err());
    }

    #[test]
    fn debug_is_an_explicit_transient_switch() {
        let cli = Cli::resolve(args(["opsx-build", "--debug", "build something"])).unwrap();
        assert!(cli.debug);
        assert!(!cli.verbose);
    }

    #[test]
    fn direction_is_one_shot_resume_input() {
        let cli = Cli::resolve(args([
            "opsx-build",
            "--resume",
            "--direction",
            "keep the AST unchanged",
        ]))
        .unwrap();
        assert_eq!(cli.direction.as_deref(), Some("keep the AST unchanged"));
        assert!(
            CliArgs::try_parse_from([
                "opsx-build",
                "--direction",
                "orphaned direction",
                "build something"
            ])
            .is_err()
        );
    }

    #[test]
    fn streaming_defaults_to_activity_and_accepts_profiles() {
        let activity =
            Cli::resolve(args(["opsx-build", "--stream-claude", "build something"])).unwrap();
        assert_eq!(activity.stream_claude, Some(StreamFilter::Activity));

        let raw = Cli::resolve(args([
            "opsx-build",
            "--stream-claude=raw",
            "build something",
        ]))
        .unwrap();
        assert_eq!(raw.stream_claude, Some(StreamFilter::Raw));

        let generic = Cli::resolve(args([
            "opsx-build",
            "--stream-agent=full",
            "build something",
        ]))
        .unwrap();
        assert_eq!(generic.stream_claude, Some(StreamFilter::Full));
    }

    #[test]
    fn workflow_defaults_to_advance_and_accepts_next_slice_alias() {
        let default = Cli::resolve(args(["opsx-build"])).unwrap();
        assert_eq!(default.request, "advance");

        let alias = Cli::resolve(args(["opsx-build", "next slice"])).unwrap();
        assert_eq!(alias.request, "advance");
    }

    #[test]
    fn bootstrap_and_execute_accept_an_omitted_context_for_repository_discovery() {
        for command in ["bootstrap", "execute"] {
            let cli = Cli::resolve(args(["opsx-build", "--no-config", command])).unwrap();
            assert!(cli.bootstrap_context.is_none());
            assert_eq!(cli.execute, command == "execute");
        }
    }

    #[test]
    fn bootstrap_accepts_explicit_markdown_context_and_validates_options() {
        let cli = Cli::resolve(args([
            "opsx-build",
            "--define",
            "language=Go",
            "bootstrap",
            "--context",
            "project.md",
        ]))
        .unwrap();
        assert_eq!(cli.request, "bootstrap");
        assert_eq!(
            cli.bootstrap_context.as_deref(),
            Some(std::path::Path::new("project.md"))
        );
        assert_eq!(
            cli.bootstrap_defines.get("language").map(String::as_str),
            Some("Go")
        );

        assert!(Cli::resolve(args(["opsx-build", "advance", "--context", "project.md"])).is_err());
        assert!(Cli::resolve(args(["opsx-build", "--define", "language=Go", "advance"])).is_err());
        assert!(
            Cli::resolve(args([
                "opsx-build",
                "--define",
                "language=Go",
                "--define",
                "language=Rust",
                "bootstrap",
                "--context",
                "project.md"
            ]))
            .is_err()
        );
        assert!(
            Cli::resolve(args([
                "opsx-build",
                "bootstrap",
                "--context",
                "project.md",
                "--loop"
            ]))
            .is_err()
        );
    }

    #[test]
    fn execute_composes_bootstrap_and_an_advance_campaign() {
        let cli = Cli::resolve(args([
            "opsx-build",
            "--define",
            "language=Go",
            "--max-iterations",
            "9",
            "execute",
            "--context",
            "project.md",
        ]))
        .unwrap();
        assert!(cli.execute);
        assert_eq!(cli.request, "advance");
        assert!(cli.loop_workflow);
        assert_eq!(cli.max_iterations, Some(9));
        assert_eq!(
            cli.bootstrap_context.as_deref(),
            Some(std::path::Path::new("project.md"))
        );
        assert!(
            Cli::resolve(args([
                "opsx-build",
                "--no-loop",
                "execute",
                "--context",
                "project.md"
            ]))
            .is_err()
        );
    }

    #[test]
    fn update_skills_is_a_standalone_mode() {
        let cli = Cli::resolve(args(["opsx-build", "--update-skills"])).unwrap();
        assert!(cli.update_skills);
        assert!(cli.request.is_empty());
        assert!(
            CliArgs::try_parse_from(["opsx-build", "--update-skills", "build something"]).is_err()
        );
        assert!(CliArgs::try_parse_from(["opsx-build", "--update-skills", "--resume"]).is_err());
    }

    #[test]
    fn resume_allows_no_request_and_conflicts_with_interactive() {
        let cli = Cli::resolve(args(["opsx-build", "--resume"])).unwrap();
        assert!(cli.resume);
        assert!(cli.request.is_empty());
        assert!(CliArgs::try_parse_from(["opsx-build", "--resume", "--interactive"]).is_err());
    }

    #[test]
    fn continues_existing_change_without_request() {
        let cli = Cli::resolve(args([
            "opsx-build",
            "--continue-existing",
            "--change",
            "fix-test-harness",
        ]))
        .unwrap();
        assert!(cli.continue_existing);
        assert_eq!(cli.change.as_deref(), Some("fix-test-harness"));
        assert!(cli.request.is_empty());
    }

    #[test]
    fn change_prescribes_a_free_form_request_or_selects_an_existing_change() {
        let cli = Cli::resolve(args([
            "opsx-build",
            "--change",
            "remote-path-prefix",
            "support remote URI path prefixes",
        ]))
        .unwrap();
        assert_eq!(cli.change.as_deref(), Some("remote-path-prefix"));
        assert_eq!(cli.request, "support remote URI path prefixes");

        assert!(Cli::resolve(args(["opsx-build", "--change", "fix-test-harness"])).is_err());
        assert!(
            Cli::resolve(args([
                "opsx-build",
                "--change",
                "fix-test-harness",
                "--loop",
                "build something",
            ]))
            .is_err()
        );
        assert!(
            CliArgs::try_parse_from([
                "opsx-build",
                "--resume",
                "--continue-existing",
                "--change",
                "fix-test-harness",
            ])
            .is_err()
        );
    }

    #[test]
    fn forget_allows_no_request_and_conflicts_with_resume() {
        let cli = Cli::resolve(args(["opsx-build", "--forget"])).unwrap();
        assert!(cli.forget);
        assert!(cli.request.is_empty());
        assert!(CliArgs::try_parse_from(["opsx-build", "--forget", "--resume"]).is_err());
    }

    #[test]
    fn passthrough_args_require_interactive_mode() {
        assert!(
            CliArgs::try_parse_from(["opsx-build", "build something", "--", "--effort", "xhigh",])
                .is_err()
        );
    }
}
