use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use serde::Deserialize;

use crate::stream::StreamFilter;

const DEFAULT_MAX_VERIFY_RETRIES: u32 = 3;
const DEFAULT_PERMISSION_MODE: &str = "auto";
const DEFAULT_CLAUDE_COMMAND: &str = "claude";

#[derive(Debug, Clone)]
pub struct Cli {
    pub request: String,
    pub repo: PathBuf,
    pub interactive: bool,
    pub resume: bool,
    pub forget: bool,
    pub continue_existing: bool,
    pub change: Option<String>,
    pub direction: Option<String>,
    pub interactive_args: Vec<String>,
    pub max_verify_retries: u32,
    pub verbose: bool,
    pub debug: bool,
    pub stream_claude: Option<StreamFilter>,
    pub dry_run: bool,
    pub permission_mode: String,
    pub claude_command: String,
    pub claude_model: Option<String>,
    pub explore_command: Option<String>,
    pub propose_command: Option<String>,
    pub apply_command: Option<String>,
    pub verify_command: Option<String>,
    pub archive_command: Option<String>,
    pub config_path: Option<PathBuf>,
}

impl Cli {
    pub fn load() -> Result<Self> {
        Self::resolve(CliArgs::parse())
    }

    fn resolve(args: CliArgs) -> Result<Self> {
        let (config, config_path) = load_config(&args)?;
        Ok(resolve_values(args, config, config_path))
    }
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "ospx-build",
    version,
    about = "Build an OpenSpec change through synchronous Claude stages"
)]
struct CliArgs {
    /// Change request, or an optional initial prompt in interactive mode.
    #[arg(required_unless_present_any = ["interactive", "resume", "forget", "continue_existing"])]
    request: Option<String>,

    /// Repository containing .git, openspec/, and Claude skills.
    #[arg(long, default_value = ".", value_name = "PATH")]
    repo: PathBuf,

    /// Open an interactive Claude session instead of running the OpenSpec workflow.
    #[arg(long)]
    interactive: bool,

    /// Resume the last durable ospx-build run from its first incomplete phase.
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

    /// Select the OpenSpec change used by --continue-existing when several are active.
    #[arg(
        long,
        value_name = "NAME",
        requires = "continue_existing",
        conflicts_with_all = ["interactive", "resume", "forget"]
    )]
    change: Option<String>,

    /// One-shot guidance for the next code-changing stage of a resumed run.
    #[arg(long, requires = "resume", value_name = "TEXT")]
    direction: Option<String>,

    /// Additional arguments passed directly to Claude after `--`.
    #[arg(last = true, value_name = "CLAUDE_ARGS", requires = "interactive")]
    interactive_args: Vec<String>,

    /// Load defaults from this TOML file.
    #[arg(long, env = "OSPX_BUILD_CONFIG", value_name = "PATH")]
    config: Option<PathBuf>,

    /// Do not load the default or environment-selected config file.
    #[arg(long)]
    no_config: bool,

    /// Maximum repair/verify cycles after the first verification attempt.
    #[arg(long, env = "OSPX_BUILD_MAX_VERIFY_RETRIES", value_name = "N")]
    max_verify_retries: Option<u32>,

    /// Print commands and captured subprocess output.
    #[arg(long, short)]
    verbose: bool,

    /// Show resolved commands, Claude session details, and complete prompts.
    #[arg(long)]
    debug: bool,

    /// Stream Claude activity; optionally select activity, full, or raw filtering.
    #[arg(
        long,
        env = "OSPX_BUILD_STREAM_CLAUDE",
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
    #[arg(long, env = "OSPX_BUILD_PERMISSION_MODE", value_name = "MODE")]
    permission_mode: Option<String>,

    /// Command prefix used to launch Claude (for example, "omlx launch claude").
    #[arg(long, env = "OSPX_BUILD_CLAUDE_COMMAND", value_name = "COMMAND")]
    claude_command: Option<String>,

    /// Model passed to the configured Claude launcher as `--model MODEL`.
    #[arg(long, env = "OSPX_BUILD_CLAUDE_MODEL", value_name = "MODEL")]
    claude_model: Option<String>,

    /// Override the exploration slash command.
    #[arg(long, env = "OSPX_BUILD_EXPLORE_COMMAND", value_name = "COMMAND")]
    explore_command: Option<String>,

    /// Override the proposal slash command.
    #[arg(long, env = "OSPX_BUILD_PROPOSE_COMMAND", value_name = "COMMAND")]
    propose_command: Option<String>,

    /// Override the OpenSpec apply slash command.
    #[arg(long, env = "OSPX_BUILD_APPLY_COMMAND", value_name = "COMMAND")]
    apply_command: Option<String>,

    /// Override the OpenSpec verify slash command.
    #[arg(long, env = "OSPX_BUILD_VERIFY_COMMAND", value_name = "COMMAND")]
    verify_command: Option<String>,

    /// Override the OpenSpec archive slash command.
    #[arg(long, env = "OSPX_BUILD_ARCHIVE_COMMAND", value_name = "COMMAND")]
    archive_command: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    max_verify_retries: Option<u32>,
    permission_mode: Option<String>,
    claude_command: Option<String>,
    claude_model: Option<String>,
    explore_command: Option<String>,
    propose_command: Option<String>,
    apply_command: Option<String>,
    verify_command: Option<String>,
    archive_command: Option<String>,
    stream_claude: Option<StreamFilter>,
}

fn load_config(args: &CliArgs) -> Result<(FileConfig, Option<PathBuf>)> {
    if args.no_config {
        return Ok((FileConfig::default(), None));
    }

    if let Some(path) = &args.config {
        return read_config(path).map(|config| (config, Some(path.clone())));
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
    if let Some(root) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(root).join("ospx-build/config.toml"));
    }
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(".config/ospx-build/config.toml"))
}

fn resolve_values(args: CliArgs, config: FileConfig, config_path: Option<PathBuf>) -> Cli {
    Cli {
        request: args.request.unwrap_or_default(),
        repo: args.repo,
        interactive: args.interactive,
        resume: args.resume,
        forget: args.forget,
        continue_existing: args.continue_existing,
        change: args.change,
        direction: args.direction,
        interactive_args: args.interactive_args,
        max_verify_retries: args
            .max_verify_retries
            .or(config.max_verify_retries)
            .unwrap_or(DEFAULT_MAX_VERIFY_RETRIES),
        verbose: args.verbose,
        debug: args.debug,
        stream_claude: args.stream_claude.or(config.stream_claude),
        dry_run: args.dry_run,
        permission_mode: args
            .permission_mode
            .or(config.permission_mode)
            .unwrap_or_else(|| DEFAULT_PERMISSION_MODE.to_owned()),
        claude_command: args
            .claude_command
            .or(config.claude_command)
            .unwrap_or_else(|| DEFAULT_CLAUDE_COMMAND.to_owned()),
        claude_model: args.claude_model.or(config.claude_model),
        explore_command: args.explore_command.or(config.explore_command),
        propose_command: args.propose_command.or(config.propose_command),
        apply_command: args.apply_command.or(config.apply_command),
        verify_command: args.verify_command.or(config.verify_command),
        archive_command: args.archive_command.or(config.archive_command),
        config_path,
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs};

    use super::*;
    use uuid::Uuid;

    fn args(values: impl IntoIterator<Item = impl Into<OsString> + Clone>) -> CliArgs {
        CliArgs::try_parse_from(values).unwrap()
    }

    #[test]
    fn parses_toml_defaults() {
        let config: FileConfig = toml::from_str(
            r#"
                max_verify_retries = 5
                permission_mode = "dontAsk"
                claude_command = "omlx launch claude"
                claude_model = "local-model"
                verify_command = "/opsx:verify"
                stream_claude = "full"
            "#,
        )
        .unwrap();
        let cli = resolve_values(
            args(["ospx-build", "build something"]),
            config,
            Some(PathBuf::from("config.toml")),
        );

        assert_eq!(cli.max_verify_retries, 5);
        assert_eq!(cli.permission_mode, "dontAsk");
        assert_eq!(cli.claude_command, "omlx launch claude");
        assert_eq!(cli.claude_model.as_deref(), Some("local-model"));
        assert_eq!(cli.verify_command.as_deref(), Some("/opsx:verify"));
        assert_eq!(cli.stream_claude, Some(StreamFilter::Full));
    }

    #[test]
    fn command_line_values_override_file_values() {
        let config: FileConfig = toml::from_str(
            r#"
                max_verify_retries = 5
                claude_model = "config-model"
            "#,
        )
        .unwrap();
        let cli = resolve_values(
            args([
                "ospx-build",
                "--max-verify-retries",
                "2",
                "--claude-model",
                "cli-model",
                "build something",
            ]),
            config,
            None,
        );

        assert_eq!(cli.max_verify_retries, 2);
        assert_eq!(cli.claude_model.as_deref(), Some("cli-model"));
    }

    #[test]
    fn rejects_unknown_config_keys() {
        let error = toml::from_str::<FileConfig>("claude_modle = 'typo'").unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn uses_builtin_defaults_without_a_file() {
        let cli = resolve_values(
            args(["ospx-build", "build something"]),
            FileConfig::default(),
            None,
        );
        assert_eq!(cli.max_verify_retries, DEFAULT_MAX_VERIFY_RETRIES);
        assert_eq!(cli.permission_mode, DEFAULT_PERMISSION_MODE);
        assert_eq!(cli.claude_command, DEFAULT_CLAUDE_COMMAND);
    }

    #[test]
    fn loads_an_explicit_file_and_records_its_path() {
        let directory = env::temp_dir().join(format!("ospx-build-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.toml");
        fs::write(
            &path,
            "claude_command = 'omlx launch claude'\nclaude_model = 'configured-model'\n",
        )
        .unwrap();
        let cli = Cli::resolve(args(vec![
            OsString::from("ospx-build"),
            OsString::from("--config"),
            path.clone().into_os_string(),
            OsString::from("build something"),
        ]))
        .unwrap();

        assert_eq!(cli.config_path.as_ref(), Some(&path));
        assert_eq!(cli.claude_command, "omlx launch claude");
        assert_eq!(cli.claude_model.as_deref(), Some("configured-model"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn no_config_skips_even_an_explicit_missing_file() {
        let cli = Cli::resolve(args([
            "ospx-build",
            "--config",
            "/definitely/missing/config.toml",
            "--no-config",
            "build something",
        ]))
        .unwrap();
        assert!(cli.config_path.is_none());
        assert_eq!(cli.claude_command, DEFAULT_CLAUDE_COMMAND);
    }

    #[test]
    fn interactive_mode_allows_no_initial_prompt_and_passthrough_args() {
        let cli = Cli::resolve(args([
            "ospx-build",
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
    fn debug_is_an_explicit_transient_switch() {
        let cli = Cli::resolve(args(["ospx-build", "--debug", "build something"])).unwrap();
        assert!(cli.debug);
        assert!(!cli.verbose);
    }

    #[test]
    fn direction_is_one_shot_resume_input() {
        let cli = Cli::resolve(args([
            "ospx-build",
            "--resume",
            "--direction",
            "keep the AST unchanged",
        ]))
        .unwrap();
        assert_eq!(cli.direction.as_deref(), Some("keep the AST unchanged"));
        assert!(
            CliArgs::try_parse_from([
                "ospx-build",
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
            Cli::resolve(args(["ospx-build", "--stream-claude", "build something"])).unwrap();
        assert_eq!(activity.stream_claude, Some(StreamFilter::Activity));

        let raw = Cli::resolve(args([
            "ospx-build",
            "--stream-claude=raw",
            "build something",
        ]))
        .unwrap();
        assert_eq!(raw.stream_claude, Some(StreamFilter::Raw));
    }

    #[test]
    fn workflow_still_requires_a_request() {
        assert!(CliArgs::try_parse_from(["ospx-build"]).is_err());
    }

    #[test]
    fn resume_allows_no_request_and_conflicts_with_interactive() {
        let cli = Cli::resolve(args(["ospx-build", "--resume"])).unwrap();
        assert!(cli.resume);
        assert!(cli.request.is_empty());
        assert!(CliArgs::try_parse_from(["ospx-build", "--resume", "--interactive"]).is_err());
    }

    #[test]
    fn continues_existing_change_without_request() {
        let cli = Cli::resolve(args([
            "ospx-build",
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
    fn change_selection_requires_continue_existing() {
        assert!(CliArgs::try_parse_from(["ospx-build", "--change", "fix-test-harness"]).is_err());
        assert!(
            CliArgs::try_parse_from([
                "ospx-build",
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
        let cli = Cli::resolve(args(["ospx-build", "--forget"])).unwrap();
        assert!(cli.forget);
        assert!(cli.request.is_empty());
        assert!(CliArgs::try_parse_from(["ospx-build", "--forget", "--resume"]).is_err());
    }

    #[test]
    fn passthrough_args_require_interactive_mode() {
        assert!(
            CliArgs::try_parse_from(["ospx-build", "build something", "--", "--effort", "xhigh",])
                .is_err()
        );
    }
}
