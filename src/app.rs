use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    agenda::{
        AgendaAssignment, AgendaSelection, discover as discover_agenda, has_subdivision,
        is_terminal_assignment,
    },
    backend::{AgentBackend, SessionId, SessionMode, StageProtocol, StageResult, StageSignal},
    bootstrap::{
        BOOTSTRAP_CHANGE, BOOTSTRAP_PATH, BootstrapScaffold, ensure_codex_guidance, init_command,
        instructions as bootstrap_instructions, validate_agenda as validate_bootstrap_agenda,
    },
    claude::{
        CONNECTION_TEST_MARKER, ClaudeBackend, ClaudeLauncher, ClaudeOutputFormat,
        ConnectionTestMode, SkillCommands, build_claude_command, build_connection_test_command,
        build_interactive_claude_command, is_missing_terminal_result, parse_connection_test_output,
        stage_prompt,
    },
    cli::{AgentConnection, BackendKind, Cli},
    codex::{CodexBackend, CodexLauncher, build_interactive_codex_command},
    codex_server::CodexServer,
    git::{
        SliceBaseline, baseline_matches_except, capture_slice_baseline, committed_paths_since,
        current_head, hard_reset, head_descends_from, legacy_metadata_dir, metadata_dir,
        release_slice_baseline, remove_metadata, repository_root, reset_to_slice_baseline,
        resolve_commit, untracked_paths,
    },
    model_confusions::ensure_model_confusion_plugin,
    opencode::{
        OpenCodeBackend, OpenCodeLauncher,
        build_connection_test_command as build_opencode_connection_test_command,
        build_interactive_opencode_command, build_opencode_command,
        parse_connection_test_output as parse_opencode_connection_test_output,
    },
    opencode_server::OpenCodeServer,
    openspec::{
        ChangeSnapshot, identify_assigned_change, identify_change,
        numeric_prefix_reconciliation_candidate, planning_status, rename_change_directory,
        select_existing_change, snapshot as openspec_snapshot,
    },
    process::{
        CommandSpec, PauseRequested, ProcessRunner, WorkerEscalationRequested, prerequisite_exists,
        shell_quote,
    },
    sidecar::{SidecarAssociation, SidecarPlan, default_sidecar_root, load_association},
    skills::{SkillInstallAction, ensure_unattended_skills},
    state::Stage,
    ui::{CampaignIterationView, CampaignView, Ui},
};

const BOOTSTRAP_PROPOSAL_CONTEXT: &str = "This is the one-time planning-only bootstrap change. Its purpose is to decompose the complete project goal into bounded implementation slices; do not implement product code and do not report TOO_LARGE merely because the overall project spans many slices. Use the exact assigned change name and create only this one OpenSpec change. During this Propose stage, create or update only that change's normal OpenSpec artifacts. Do not create or modify `automation/slices/README.md` or any implementation slice under `automation/slices/`; the subsequent Apply stage exclusively owns those deliverables. Record the intended agenda structure, slice files, acceptance criteria, and required tests in the OpenSpec design and tasks so a fresh Apply session can materialize them.";
const BOOTSTRAP_APPLY_CONTEXT: &str = "Apply this planning-only bootstrap change completely. Create the ordered implementation agenda and README required by `automation/bootstrap.md`, using `openspec/config.yaml` as the project authority. Do not implement product functionality and do not create OpenSpec changes for the planned implementation slices. Before reporting READY, inspect the completed agenda against every structural requirement in `automation/bootstrap.md` and correct any omission within this Apply stage. Do not archive the bootstrap change.";

const TOTAL_STAGES: usize = 7;
const AGENDA_TOTAL_STAGES: usize = 6;
const STATE_SCHEMA_VERSION: u32 = 5;
const MAX_FRONTIER_REPLANS: u32 = 3;
const MAX_TERMINAL_REMEDIATIONS: u32 = 3;

fn stage_after_propose(yolo: bool) -> Stage {
    if yolo {
        Stage::Apply
    } else {
        Stage::ProposalCommit
    }
}

fn displayed_stage_position(
    stage: Stage,
    has_agenda: bool,
    terminal_frontier: bool,
    yolo: bool,
) -> (usize, usize) {
    let (mut number, mut total) = if terminal_frontier {
        (stage.number(), TOTAL_STAGES)
    } else if has_agenda {
        (stage.number() - 1, AGENDA_TOTAL_STAGES)
    } else {
        (stage.number(), TOTAL_STAGES)
    };
    if yolo {
        total -= 1;
        if stage.number() > Stage::ProposalCommit.number() {
            number -= 1;
        }
    }
    (number, total)
}

pub struct App<U: Ui> {
    cli: Cli,
    ui: U,
}

#[derive(Debug, Clone)]
enum AgentLauncher {
    Claude(ClaudeLauncher),
    OpenCode(OpenCodeLauncher),
    Codex(CodexLauncher),
}

impl AgentLauncher {
    fn from_connection(connection: &AgentConnection) -> Result<Self> {
        match connection.backend {
            BackendKind::Claude => ClaudeLauncher::from_connection(connection).map(Self::Claude),
            BackendKind::OpenCode => {
                OpenCodeLauncher::from_connection(connection).map(Self::OpenCode)
            }
            BackendKind::Codex => CodexLauncher::from_connection(connection).map(Self::Codex),
        }
    }

    fn program(&self) -> &str {
        match self {
            Self::Claude(launcher) => &launcher.program,
            Self::OpenCode(launcher) => &launcher.program,
            Self::Codex(launcher) => &launcher.program,
        }
    }

    fn connection_name(&self) -> Option<&str> {
        match self {
            Self::Claude(launcher) => launcher.connection_name.as_deref(),
            Self::OpenCode(launcher) => launcher.connection_name.as_deref(),
            Self::Codex(launcher) => launcher.connection_name.as_deref(),
        }
    }

    fn model(&self) -> Option<&str> {
        match self {
            Self::Claude(launcher) => launcher.model.as_deref(),
            Self::OpenCode(launcher) => launcher.model.as_deref(),
            Self::Codex(launcher) => launcher.model.as_deref(),
        }
    }

    fn environment_name(&self) -> Option<&str> {
        match self {
            Self::Claude(launcher) => launcher.environment_name.as_deref(),
            Self::OpenCode(launcher) => launcher.environment_name.as_deref(),
            Self::Codex(launcher) => launcher.environment_name.as_deref(),
        }
    }

    fn backend_name(&self) -> &'static str {
        match self {
            Self::Claude(_) => "claude",
            Self::OpenCode(_) => "opencode",
            Self::Codex(_) => "codex",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunState {
    schema_version: u32,
    request: String,
    change: Option<String>,
    stage: Stage,
    verify_retries: u32,
    before_changes: ChangeSnapshot,
    planning_session: Option<SessionId>,
    #[serde(default)]
    planning_backend: Option<String>,
    pending_repair: Option<String>,
    pending_direction: Option<String>,
    proposal_head: Option<String>,
    final_head: Option<String>,
    #[serde(default)]
    terminal_summary: Option<String>,
    #[serde(default)]
    too_large: Option<TooLargeOutcome>,
    #[serde(default)]
    campaign: Option<CampaignState>,
    #[serde(default)]
    agenda: Option<AgendaAssignment>,
    #[serde(default)]
    slice_baseline: Option<SliceBaseline>,
    #[serde(default)]
    frontier_replans: u32,
    #[serde(default)]
    terminal_review_complete: bool,
    #[serde(default)]
    terminal_remediations: u32,
    #[serde(default)]
    bootstrap: bool,
    #[serde(default)]
    product_repo: Option<PathBuf>,
    #[serde(default)]
    sidecar_store: Option<String>,
    #[serde(default)]
    planning_final_head: Option<String>,
    #[serde(default)]
    local_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TooLargeOutcome {
    stage: Stage,
    summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CampaignState {
    iteration: u32,
    max_iterations: Option<u32>,
    completed: Vec<CompletedIteration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompletedIteration {
    iteration: u32,
    change: String,
    proposal_head: Option<String>,
    final_head: Option<String>,
    elapsed_seconds: Option<u64>,
}

enum WorkflowOutcome {
    Complete(RunState),
    Done(RunState),
    TooLarge(RunState),
}

impl RunState {
    fn new(request: String, before_changes: ChangeSnapshot) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            request,
            change: None,
            stage: Stage::Explore,
            verify_retries: 0,
            before_changes,
            planning_session: None,
            planning_backend: None,
            pending_repair: None,
            pending_direction: None,
            proposal_head: None,
            final_head: None,
            terminal_summary: None,
            too_large: None,
            campaign: None,
            agenda: None,
            slice_baseline: None,
            frontier_replans: 0,
            terminal_review_complete: false,
            terminal_remediations: 0,
            bootstrap: false,
            product_repo: None,
            sidecar_store: None,
            planning_final_head: None,
            local_only: false,
        }
    }

    fn new_campaign(
        request: String,
        before_changes: ChangeSnapshot,
        max_iterations: Option<u32>,
    ) -> Self {
        let mut state = Self::new(request, before_changes);
        state.campaign = Some(CampaignState {
            iteration: 1,
            max_iterations,
            completed: Vec::new(),
        });
        state
    }

    fn continue_existing(change: String, active_changes: ChangeSnapshot) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            request: format!("Continue existing OpenSpec change `{change}`"),
            change: Some(change),
            stage: Stage::ProposalCommit,
            verify_retries: 0,
            before_changes: active_changes,
            planning_session: None,
            planning_backend: None,
            pending_repair: None,
            pending_direction: None,
            proposal_head: None,
            final_head: None,
            terminal_summary: None,
            too_large: None,
            campaign: None,
            agenda: None,
            slice_baseline: None,
            frontier_replans: 0,
            terminal_review_complete: false,
            terminal_remediations: 0,
            bootstrap: false,
            product_repo: None,
            sidecar_store: None,
            planning_final_head: None,
            local_only: false,
        }
    }

    fn bootstrap(before_changes: ChangeSnapshot) -> Self {
        let mut state = Self::new("bootstrap".to_owned(), before_changes);
        state.change = Some(BOOTSTRAP_CHANGE.to_owned());
        state.stage = Stage::Propose;
        state.agenda = Some(AgendaAssignment {
            path: BOOTSTRAP_PATH.to_owned(),
            change: BOOTSTRAP_CHANGE.to_owned(),
            title: "Bootstrap implementation agenda".to_owned(),
            content: bootstrap_instructions().to_owned(),
        });
        state.bootstrap = true;
        state
    }

    fn sidecar(
        request: String,
        change: String,
        before_changes: ChangeSnapshot,
        association: &SidecarAssociation,
        local_only: bool,
    ) -> Self {
        let mut state = Self::new(request, before_changes);
        state.change = Some(change);
        state.stage = Stage::Propose;
        state.product_repo = Some(association.product_root.clone());
        state.sidecar_store = Some(association.store_id.clone());
        state.local_only = local_only;
        state
    }

    fn consume_direction(&mut self) {
        self.pending_direction = None;
    }
}

impl<U: Ui> App<U> {
    pub fn new(cli: Cli, ui: U) -> Self {
        Self { cli, ui }
    }

    pub fn run(self) -> Result<()> {
        match self.run_inner() {
            Err(error) if error.downcast_ref::<PauseRequested>().is_some() => {
                let pause = error
                    .downcast_ref::<PauseRequested>()
                    .expect("pause error was checked above");
                self.ui.finish_dashboard();
                self.ui
                    .success("PAUSED — the current phase checkpoint was preserved");
                self.ui
                    .info(&format!("Resume with: {}", pause.resume_command()));
                Ok(())
            }
            result => result,
        }
    }

    fn run_inner(&self) -> Result<()> {
        let requested_repo = self.cli.repo.canonicalize().with_context(|| {
            format!(
                "repository path `{}` does not exist",
                self.cli.repo.display()
            )
        })?;

        if self.cli.test_connection.is_some() {
            self.ui.banner(&requested_repo.display().to_string());
            let launcher = AgentLauncher::from_connection(&self.cli.worker_connection)?;
            ensure_launcher_prerequisites(&launcher, &requested_repo, &self.ui)?;
            self.debug_configuration(&requested_repo, &launcher);
            if let Some(path) = &self.cli.config_path {
                self.ui.info(&format!("Using config `{}`", path.display()));
            }
            return match &launcher {
                AgentLauncher::Claude(launcher) => {
                    self.run_connection_test(&requested_repo, launcher)
                }
                AgentLauncher::OpenCode(launcher) => {
                    self.run_opencode_connection_test(&requested_repo, launcher)
                }
                AgentLauncher::Codex(launcher) => {
                    self.run_codex_connection_test(&requested_repo, launcher)
                }
            };
        }

        prerequisite_exists("git", &requested_repo, &self.ui)?;
        let product_repo = repository_root(&requested_repo, &self.ui)?;
        if self.cli.request == "rewind" {
            self.ui.banner(&product_repo.display().to_string());
            return self.rewind(&product_repo);
        }
        let mut sidecar = load_association(&product_repo, &self.ui)?;
        if (self.cli.sidecar || sidecar.is_some())
            && self.cli.worker_connection.backend == BackendKind::OpenCode
        {
            bail!(
                "the first OpenCode adapter does not yet support sidecar repositories; use an in-repository OpenSpec project or a Claude worker for this run"
            );
        }
        if self.cli.sidecar
            && sidecar.is_none()
            && (self.cli.resume || self.cli.forget || self.cli.update_skills)
        {
            bail!(
                "this checkout has no associated sidecar; start one bounded --sidecar change with --context and --change first"
            );
        }
        if self.cli.sidecar && sidecar.is_none() {
            let context_path = self.cli.bootstrap_context.as_deref().context(
                "the first --sidecar invocation requires --context PATH with the bounded project/change context",
            )?;
            let root = self
                .cli
                .sidecar_root
                .clone()
                .map(Ok)
                .unwrap_or_else(default_sidecar_root)?;
            let plan = SidecarPlan::new(&product_repo, context_path, &root)?;
            self.ui.banner(&product_repo.display().to_string());
            if self.cli.dry_run {
                self.ui
                    .warn("DRY RUN — the external OpenSpec store would not be created");
                self.ui.info(&format!(
                    "Planning sidecar: `{}`",
                    plan.association.planning_root.display()
                ));
                if plan.needs_setup() {
                    self.ui
                        .info(&format!("Create store: {}", plan.setup_command().display()));
                } else {
                    self.ui.info(&format!(
                        "Recover store: `{}`",
                        plan.association.planning_root.display()
                    ));
                }
                self.ui.info(&format!(
                    "Install Claude workflows: {}",
                    plan.init_command().display()
                ));
                self.ui.info(if self.cli.yolo {
                    "Run local Propose, Apply, Verify/repair, Archive, product commit, and sidecar archive commit (YOLO: no proposal commit)"
                } else {
                    "Run local Propose, proposal commit, Apply, Verify/repair, Archive, product commit, and sidecar archive commit"
                });
                return Ok(());
            }
            prerequisite_exists("openspec", &product_repo, &self.ui)?;
            let launcher = AgentLauncher::from_connection(&self.cli.worker_connection)?;
            ensure_launcher_prerequisites(&launcher, &product_repo, &self.ui)?;
            let runner = ProcessRunner::new(&self.ui);
            if plan.needs_setup() {
                let setup =
                    runner.checked(&plan.setup_command(), "Creating OpenSpec sidecar store")?;
                plan.validate_setup_output(&setup.stdout)?;
            } else {
                self.ui
                    .info("Recovering native sidecar store from an interrupted setup");
            }
            runner.checked(
                &plan.init_command(),
                "Installing OpenSpec Claude workflows in the sidecar",
            )?;
            plan.finish(&self.ui)?;
            self.ui.success("Created and associated OpenSpec sidecar");
            sidecar = load_association(&product_repo, &self.ui)?;
        }
        if sidecar.is_some() && self.cli.request == "bootstrap" {
            bail!("sidecar campaign bootstrap is not implemented yet; use one bounded change");
        }
        if sidecar.is_some() && self.cli.loop_workflow {
            bail!("the initial sidecar release supports one bounded change, not --loop");
        }
        if sidecar.is_some() && self.cli.continue_existing {
            bail!("the initial sidecar release does not support --continue-existing");
        }
        if sidecar.is_some() && self.cli.bootstrap_context.is_some() && !self.cli.sidecar {
            bail!(
                "--context is only used when creating a sidecar; this checkout is already associated"
            );
        }
        let repo = sidecar.as_ref().map_or_else(
            || product_repo.clone(),
            |sidecar| sidecar.planning_root.clone(),
        );
        self.ui.banner(&product_repo.display().to_string());
        if let Some(sidecar) = &sidecar {
            self.ui.info(&format!(
                "Planning sidecar `{}`: `{}`",
                sidecar.store_id,
                sidecar.planning_root.display()
            ));
        }
        let local_only = self.cli.local_only || sidecar.is_some();
        self.ui.frontier_enabled(!local_only);
        if local_only {
            self.ui.info("LOCAL-ONLY: frontier fallback is disabled");
        }

        if self.cli.forget {
            return self.forget_checkpoint(&repo);
        }
        if self.cli.update_skills {
            let changed = self.synchronize_skills(&repo)?;
            if self.cli.dry_run {
                self.ui.success(&format!(
                    "DRY RUN — {changed} bundled skill file(s) would change"
                ));
            } else if changed == 0 {
                self.ui
                    .success("Bundled unattended skills are already current");
            } else {
                self.ui.success(&format!(
                    "Updated {changed} bundled unattended skill file(s)"
                ));
            }
            return Ok(());
        }

        if self.cli.bootstrap_context.is_some() {
            let frontier_launcher = AgentLauncher::from_connection(&self.cli.frontier_connection)?;
            ensure_launcher_prerequisites(&frontier_launcher, &repo, &self.ui)?;
            let worker_launcher = self
                .cli
                .execute
                .then(|| AgentLauncher::from_connection(&self.cli.worker_connection))
                .transpose()?;
            if let Some(worker_launcher) = worker_launcher.as_ref() {
                ensure_launcher_prerequisites(worker_launcher, &repo, &self.ui)?;
            }
            prerequisite_exists("openspec", &repo, &self.ui)?;
            self.debug_configuration(&repo, &frontier_launcher);
            if let Some(worker_launcher) = worker_launcher.as_ref() {
                self.debug_configuration(&repo, worker_launcher);
            }
            if let Some(path) = &self.cli.config_path {
                self.ui.info(&format!("Using config `{}`", path.display()));
            }
            if let Some(worker_launcher) = worker_launcher.as_ref() {
                self.ui.info(&format!(
                    "Connections: bootstrap/frontier {}, campaign worker {}",
                    connection_description(&frontier_launcher),
                    connection_description(worker_launcher)
                ));
            } else {
                self.ui.info(&format!(
                    "Bootstrap planner: {}",
                    connection_description(&frontier_launcher)
                ));
            }
            if self.cli.yolo {
                self.ui
                    .warn("YOLO mode: proposal milestone commits are disabled");
            }
            return self.run_bootstrap(
                &repo,
                worker_launcher.as_ref().unwrap_or(&frontier_launcher),
                &frontier_launcher,
            );
        }

        let launcher = AgentLauncher::from_connection(&self.cli.worker_connection)?;
        ensure_launcher_prerequisites(&launcher, &repo, &self.ui)?;

        self.debug_configuration(&repo, &launcher);
        if let Some(path) = &self.cli.config_path {
            self.ui.info(&format!("Using config `{}`", path.display()));
        }

        if self.cli.interactive {
            return match &launcher {
                AgentLauncher::Claude(launcher) => self.run_interactive(&repo, launcher),
                AgentLauncher::OpenCode(launcher) => self.run_interactive_opencode(&repo, launcher),
                AgentLauncher::Codex(launcher) => self.run_interactive_codex(&repo, launcher),
            };
        }

        if self.cli.yolo {
            self.ui
                .warn("YOLO mode: proposal milestone commits are disabled");
        }

        let frontier_launcher = if local_only {
            None
        } else {
            let frontier = AgentLauncher::from_connection(&self.cli.frontier_connection)?;
            ensure_launcher_prerequisites(&frontier, &repo, &self.ui)?;
            self.debug_configuration(&repo, &frontier);
            Some(frontier)
        };
        if launcher.connection_name().is_some()
            || frontier_launcher
                .as_ref()
                .is_some_and(|frontier| frontier.connection_name().is_some())
        {
            let description = frontier_launcher.as_ref().map_or_else(
                || {
                    format!(
                        "worker {} (all model stages)",
                        connection_description(&launcher)
                    )
                },
                |frontier| {
                    format!(
                        "worker {}, frontier {}",
                        connection_description(&launcher),
                        connection_description(frontier)
                    )
                },
            );
            self.ui.info(&format!("Connections: {description}"));
        }

        prerequisite_exists("openspec", &repo, &self.ui)?;
        if !repo.join("openspec/config.yaml").is_file() {
            bail!(
                "no OpenSpec setup found at `{}`; expected `openspec/config.yaml`",
                repo.display()
            );
        }
        self.synchronize_skills(&repo)?;
        let commands = SkillCommands::discover(&repo, &self.cli)?;
        self.ui.debug(&format!(
            "workflow commands: explore=`{}`, propose=`{}`, apply=`{}`, verify=`{}`, archive=`{}`",
            commands.explore, commands.propose, commands.apply, commands.verify, commands.archive
        ));

        if self.cli.resume {
            return self.resume(&repo, &launcher, frontier_launcher.as_ref(), &commands);
        }

        if let Some(existing) = try_load_state(&repo, &self.ui)? {
            let requires_resume = !matches!(existing.stage, Stage::Complete | Stage::Done)
                || (existing.stage == Stage::Complete && existing.campaign.is_some());
            if requires_resume {
                bail!(
                    "an unfinished opsx-build run is recorded at {}; use `--resume` or `--forget`",
                    existing.stage.title()
                );
            }
        }

        if self.cli.continue_existing {
            return self.continue_existing(&repo, &launcher, frontier_launcher.as_ref(), &commands);
        }

        if self.cli.dry_run {
            return self.print_new_dry_run(&repo, &launcher, frontier_launcher.as_ref(), &commands);
        }

        let before_changes = openspec_snapshot(&repo, &self.ui)?;
        let mut state = if let Some(sidecar) = &sidecar {
            let change = self
                .cli
                .change
                .clone()
                .context("a new sidecar change requires --change NAME")?;
            validate_change_name(&change)?;
            RunState::sidecar(
                self.cli.request.clone(),
                change,
                before_changes,
                sidecar,
                true,
            )
        } else if self.cli.loop_workflow {
            RunState::new_campaign(
                self.cli.request.clone(),
                before_changes,
                self.cli.max_iterations,
            )
        } else {
            RunState::new(self.cli.request.clone(), before_changes)
        };
        state.local_only |= local_only;
        if sidecar.is_none() {
            self.assign_requested_change(&mut state)?;
            self.assign_agenda(&repo, &mut state)?;
        }
        arm_slice_baseline(&repo, &mut state, &self.ui)?;
        persist_state(&repo, &state, &self.ui)?;
        self.run_workflows(
            &repo,
            &launcher,
            frontier_launcher.as_ref(),
            &commands,
            state,
        )
    }

    fn assign_agenda(&self, repo: &Path, state: &mut RunState) -> Result<()> {
        if state.request != "advance" {
            return Ok(());
        }
        match discover_agenda(repo, &state.before_changes)? {
            AgendaSelection::Absent => {
                bail!(
                    "`advance` requires an ordered agenda under `automation/slices/<number>-slug.md`"
                )
            }
            AgendaSelection::Complete => {
                state.change = None;
                state.agenda = None;
                state.stage = Stage::Done;
                state.terminal_summary = Some("every ordered agenda slice is archived".to_owned());
                self.ui.info("Ordered agenda is complete");
            }
            AgendaSelection::Next(assignment) => {
                self.ui.info(&format!(
                    "Advancing agenda with `{}` — {}",
                    assignment.path, assignment.title
                ));
                state.change = Some(assignment.change.clone());
                state.stage = if state
                    .before_changes
                    .changes
                    .contains_key(&assignment.change)
                    && planning_status(repo, &assignment.change, &self.ui)?.is_complete
                {
                    self.ui.info(&format!(
                        "Assigned change `{}` already has complete planning; continuing at {}",
                        assignment.change,
                        if self.cli.yolo {
                            "Apply"
                        } else {
                            "proposal commit"
                        }
                    ));
                    stage_after_propose(self.cli.yolo)
                } else {
                    Stage::Propose
                };
                state.agenda = Some(assignment);
            }
        }
        Ok(())
    }

    fn assign_requested_change(&self, state: &mut RunState) -> Result<()> {
        let Some(change) = self.cli.change.as_deref() else {
            return Ok(());
        };
        validate_change_name(change)?;
        if state.before_changes.changes.contains_key(change) {
            bail!(
                "OpenSpec change `{change}` is already active; continue it with `--continue-existing --change {change}`"
            );
        }
        state.change = Some(change.to_owned());
        self.ui
            .info(&format!("Prescribing OpenSpec change `{change}`"));
        Ok(())
    }

    fn synchronize_skills(&self, repo: &Path) -> Result<usize> {
        let changes = ensure_unattended_skills(repo, self.cli.dry_run)?;
        for installed in &changes {
            let action = match installed.action {
                SkillInstallAction::Installed => "Installed",
                SkillInstallAction::Updated => "Updated",
                SkillInstallAction::WouldInstall => "Would install",
                SkillInstallAction::WouldUpdate => "Would update",
            };
            self.ui.info(&format!(
                "{action} bundled agent skill `{}` in target repository",
                installed.name
            ));
        }
        let mut changed = changes.len();
        if (self.cli.worker_connection.backend == BackendKind::Codex
            || self.cli.frontier_connection.backend == BackendKind::Codex)
            && ensure_codex_guidance(repo, self.cli.dry_run)?
        {
            self.ui.info(if self.cli.dry_run {
                "Would add the opsx-build managed guidance to AGENTS.md for Codex"
            } else {
                "Added the opsx-build managed guidance to AGENTS.md for Codex"
            });
            changed += 1;
        }
        Ok(changed)
    }

    fn run_bootstrap(
        &self,
        repo: &Path,
        worker_launcher: &AgentLauncher,
        frontier_launcher: &AgentLauncher,
    ) -> Result<()> {
        let context_path = self
            .cli
            .bootstrap_context
            .as_deref()
            .context("bootstrap context path was not resolved")?;
        let scaffold = BootstrapScaffold::plan(repo, context_path, &self.cli.bootstrap_defines)?;
        let init = init_command(repo);

        if self.cli.dry_run {
            self.ui
                .warn("DRY RUN — bootstrap files and subprocesses will not be created");
            self.ui
                .info(&format!("Project context: `{}`", context_path.display()));
            if !self.cli.bootstrap_defines.is_empty() {
                self.ui.info(&format!(
                    "Template variables: {}",
                    self.cli
                        .bootstrap_defines
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            self.ui
                .info(&format!("Initialize OpenSpec: {}", init.display()));
            for path in BootstrapScaffold::paths() {
                self.ui.info(&format!("Create or update `{path}`"));
            }
            let proposal_commit = if self.cli.yolo {
                ""
            } else {
                ", proposal commit"
            };
            self.ui.info(&format!(
                "Run `{BOOTSTRAP_CHANGE}` through Propose{proposal_commit}, Apply, Verify/repair, Archive, and completion commit on the frontier connection"
            ));
            self.ui.info(
                "Require an ordered agenda ending at `automation/slices/9999-project-acceptance.md`",
            );
            if self.cli.execute {
                self.ui.info(
                    "Then run the generated agenda as an advance campaign with the worker connection",
                );
            }
            return Ok(());
        }

        ProcessRunner::new(&self.ui).checked(&init, "Initializing OpenSpec")?;
        scaffold.write(repo)?;
        self.ui.success("Created bootstrap planning inputs");
        self.synchronize_skills(repo)?;
        let before_changes = openspec_snapshot(repo, &self.ui)?;
        let mut state = RunState::bootstrap(before_changes);
        if self.cli.execute {
            state.campaign = Some(CampaignState {
                iteration: 1,
                max_iterations: self.cli.max_iterations,
                completed: Vec::new(),
            });
        }
        persist_state(repo, &state, &self.ui)?;
        let commands = SkillCommands::discover(repo, &self.cli).with_context(|| {
            "OpenSpec initialization did not install the complete Propose/Apply/Verify/Archive workflow; enable the required OpenSpec actions and run `openspec update`"
        })?;
        self.ui.debug(&format!(
            "bootstrap workflow commands: propose=`{}`, apply=`{}`, verify=`{}`, archive=`{}`",
            commands.propose, commands.apply, commands.verify, commands.archive
        ));

        self.run_workflows(
            repo,
            worker_launcher,
            Some(frontier_launcher),
            &commands,
            state,
        )
    }

    fn continue_existing(
        &self,
        repo: &Path,
        launcher: &AgentLauncher,
        frontier_launcher: Option<&AgentLauncher>,
        commands: &SkillCommands,
    ) -> Result<()> {
        let active_changes = openspec_snapshot(repo, &self.ui)?;
        let change = select_existing_change(&active_changes, self.cli.change.as_deref())?;
        validate_change_name(&change)?;
        let mut state = RunState::continue_existing(change.clone(), active_changes);
        state.stage = stage_after_propose(self.cli.yolo);
        self.ui.info(&format!(
            "Continuing OpenSpec change `{change}`; {}",
            if self.cli.yolo {
                "YOLO mode skips the planning commit"
            } else {
                "planning work will be committed if necessary"
            }
        ));
        if self.cli.dry_run {
            return self.print_resume_dry_run(&state, commands);
        }
        arm_slice_baseline(repo, &mut state, &self.ui)?;
        persist_state(repo, &state, &self.ui)?;
        self.run_workflows(repo, launcher, frontier_launcher, commands, state)
    }

    fn forget_checkpoint(&self, repo: &Path) -> Result<()> {
        if self.cli.dry_run {
            self.ui.warn("DRY RUN — the checkpoint would be forgotten");
            return Ok(());
        }
        if let Ok(Some(mut state)) = try_load_state(repo, &self.ui) {
            release_state_baseline(repo, &mut state, &self.ui);
        }
        if remove_metadata(repo, &self.ui)? {
            self.ui.success(
                "Forgot opsx-build checkpoint; repository files and Git history were untouched",
            );
        } else {
            self.ui.info("No opsx-build checkpoint exists");
        }
        Ok(())
    }

    fn rewind(&self, repo: &Path) -> Result<()> {
        let revision = self
            .cli
            .rewind_target
            .as_deref()
            .context("rewind target was not resolved")?;
        let target = resolve_commit(repo, revision, &self.ui)?;
        let previous_head = current_head(repo, &self.ui)?;
        self.ui.info(&format!(
            "Rewind target `{revision}` resolves to {}",
            short_hash(&target)
        ));
        self.ui.warn(
            "Rewind discards tracked working-tree changes and moves the current branch; untracked files are preserved",
        );

        if self.cli.dry_run {
            self.ui
                .warn("DRY RUN — Git and opsx-build metadata will not be changed");
            self.ui.info(&format!(
                "Would run: git reset --hard {}",
                short_hash(&target)
            ));
            self.ui
                .info("Would forget the current opsx-build workflow checkpoint");
            return Ok(());
        }

        if !self.cli.yes {
            if !io::stdin().is_terminal() {
                bail!("rewind requires confirmation on a terminal; rerun with --yes");
            }
            print!(
                "Rewind this repository to `{revision}` ({})? [y/N] ",
                short_hash(&target)
            );
            io::stdout()
                .flush()
                .context("could not display rewind prompt")?;
            let mut response = String::new();
            io::stdin()
                .read_line(&mut response)
                .context("could not read rewind confirmation")?;
            if !matches!(response.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                self.ui.info("Rewind cancelled; nothing was changed");
                return Ok(());
            }
        }

        hard_reset(repo, &target, &self.ui)?;
        self.forget_checkpoint(repo)?;
        self.ui.success(&format!(
            "Rewound tracked repository state to `{revision}` ({})",
            short_hash(&target)
        ));
        if let Some(previous_head) = previous_head.filter(|head| head != &target) {
            self.ui.info(&format!(
                "Previous HEAD was {}; it remains recoverable through Git's reflog",
                short_hash(&previous_head)
            ));
        }
        let untracked = untracked_paths(repo, &self.ui)?;
        if !untracked.is_empty() {
            self.ui.warn(&format!(
                "Preserved {} untracked path(s); inspect them before restarting",
                untracked.len()
            ));
        }
        self.ui.info(&format!(
            "Restart with: opsx-build --repo {} --loop advance",
            shell_quote(&repo.to_string_lossy())
        ));
        Ok(())
    }

    fn resume(
        &self,
        repo: &Path,
        launcher: &AgentLauncher,
        frontier_launcher: Option<&AgentLauncher>,
        commands: &SkillCommands,
    ) -> Result<()> {
        let mut state = load_state(repo, &self.ui)?;
        if !self.cli.request.is_empty() && self.cli.request != state.request {
            bail!("the supplied request does not match the saved run request");
        }
        if state.stage == Stage::Complete && state.campaign.is_none() {
            bail!("the saved run is already complete; start a new run or use `--forget`");
        }
        if state.stage == Stage::Done && state.campaign.is_none() {
            bail!("the saved run is already done; start a new run or use `--forget`");
        }

        if let Some(outcome) = state.too_large.as_ref() {
            self.ui.warn(&format!(
                "Resuming frontier recovery after {} escalation: {}",
                outcome.stage.title(),
                outcome.summary.trim()
            ));
        }

        if self.cli.loop_workflow && state.campaign.is_none() {
            state.campaign = Some(CampaignState {
                iteration: 1,
                max_iterations: self.cli.max_iterations,
                completed: Vec::new(),
            });
        } else if let Some(campaign) = state.campaign.as_mut()
            && self.cli.max_iterations.is_some()
        {
            campaign.max_iterations = self.cli.max_iterations;
        }

        if state.request == "advance"
            && state.agenda.is_none()
            && state.stage == Stage::Explore
            && state.planning_session.is_none()
        {
            self.assign_agenda(repo, &mut state)?;
        }

        if !state.bootstrap
            && state.slice_baseline.is_none()
            && matches!(state.stage, Stage::Explore | Stage::Propose)
        {
            arm_slice_baseline(repo, &mut state, &self.ui)?;
        }

        if let Some(direction) = &self.cli.direction {
            queue_direction(&mut state, direction);
            self.ui
                .info("Queued one-shot direction for the next code-changing stage");
        }

        if state.stage == Stage::Archive {
            let changes = openspec_snapshot(repo, &self.ui)?;
            if state
                .change
                .as_ref()
                .is_some_and(|change| archive_is_absent(&changes, change))
            {
                self.ui
                    .warn("The active change is already absent; continuing at completion commit");
                state.stage = Stage::FinalCommit;
            }
        }

        self.ui.info(&format!(
            "Resuming `{}` at {}",
            state.change.as_deref().unwrap_or("planning run"),
            state.stage.title()
        ));
        if self.cli.dry_run {
            return self.print_resume_dry_run(&state, commands);
        }

        state.local_only |= self.cli.local_only;
        persist_state(repo, &state, &self.ui)?;
        self.run_workflows(repo, launcher, frontier_launcher, commands, state)
    }

    fn run_workflows(
        &self,
        repo: &Path,
        launcher: &AgentLauncher,
        frontier_launcher: Option<&AgentLauncher>,
        commands: &SkillCommands,
        mut state: RunState,
    ) -> Result<()> {
        loop {
            if needs_terminal_review(&state) {
                if state.local_only {
                    state.terminal_review_complete = true;
                    persist_state(repo, &state, &self.ui)?;
                    self.ui.warn(
                        "LOCAL-ONLY: skipping frontier review of the terminal acceptance gate",
                    );
                } else {
                    let frontier_launcher = frontier_launcher
                        .context("terminal project acceptance requires a frontier connection")?;
                    state = self.run_terminal_review(repo, frontier_launcher, state, None)?;
                    if !is_terminal_gate(&state) {
                        continue;
                    }
                }
            }
            self.present_campaign(&state);
            let iteration_started = (matches!(state.stage, Stage::Explore | Stage::Propose)
                && state.planning_session.is_none())
            .then(Instant::now);
            let stage_launcher = if state.bootstrap {
                frontier_launcher.context("bootstrap requires a frontier connection")?
            } else {
                launcher
            };
            match self.execute(repo, stage_launcher, frontier_launcher, commands, state)? {
                WorkflowOutcome::TooLarge(escalated) => {
                    if escalated.bootstrap {
                        let summary = escalated
                            .too_large
                            .as_ref()
                            .map(|outcome| outcome.summary.trim())
                            .unwrap_or(
                                "the frontier planner could not complete bootstrap planning",
                            );
                        bail!(
                            "bootstrap planning did not fit the configured frontier model: {summary}. The checkpoint and repository work were preserved; resume with a more capable frontier connection or refine the project context"
                        );
                    }
                    if escalated.local_only {
                        let outcome = escalated
                            .too_large
                            .as_ref()
                            .map(|outcome| outcome.summary.trim())
                            .unwrap_or("the local model could not complete the bounded change");
                        bail!(
                            "LOCAL-ONLY: no frontier model was invoked after the local worker reported TOO_LARGE: {outcome}"
                        );
                    }
                    if is_terminal_gate(&escalated) {
                        let frontier_launcher = frontier_launcher.context(
                            "terminal project acceptance remediation requires a frontier connection",
                        )?;
                        let outcome = escalated.too_large.clone().context(
                            "terminal acceptance escalation omitted its durable outcome",
                        )?;
                        state = self.run_terminal_review(
                            repo,
                            frontier_launcher,
                            escalated,
                            Some(outcome),
                        )?;
                        continue;
                    }
                    let frontier_launcher = frontier_launcher.with_context(|| {
                        let outcome = escalated
                            .too_large
                            .as_ref()
                            .map(|outcome| outcome.summary.trim())
                            .unwrap_or("the local model could not complete the bounded change");
                        format!(
                            "LOCAL-ONLY: no frontier model was invoked after the local worker reported TOO_LARGE: {outcome}"
                        )
                    })?;
                    state = self.run_frontier_replan(repo, frontier_launcher, escalated)?;
                    persist_state(repo, &state, &self.ui)?;
                }
                WorkflowOutcome::Done(state) => {
                    self.ui.finish_dashboard();
                    let summary = state
                        .terminal_summary
                        .as_deref()
                        .unwrap_or("the requested objective is already satisfied");
                    self.ui.success(&format!("DONE — {summary}"));
                    return Ok(());
                }
                WorkflowOutcome::Complete(mut completed_state) => {
                    if completed_state.bootstrap && completed_state.campaign.is_some() {
                        let campaign = completed_state
                            .campaign
                            .take()
                            .expect("bootstrap campaign was checked above");
                        let before_changes = openspec_snapshot(repo, &self.ui)?;
                        state = RunState::new("advance".to_owned(), before_changes);
                        state.campaign = Some(campaign);
                        self.assign_agenda(repo, &mut state)?;
                        arm_slice_baseline(repo, &mut state, &self.ui)?;
                        persist_state(repo, &state, &self.ui)?;
                        self.ui.success(
                            "Bootstrap complete — starting the generated implementation agenda",
                        );
                        continue;
                    }
                    let Some(campaign) = completed_state.campaign.as_ref() else {
                        let change = completed_state.change.as_deref().unwrap_or("change");
                        let head = completed_state
                            .final_head
                            .as_deref()
                            .unwrap_or("unknown HEAD");
                        self.ui.finish_dashboard();
                        self.ui
                            .success(&format!("COMPLETE `{change}` — final {}", short_hash(head)));
                        if let Some(planning_head) = completed_state.planning_final_head.as_deref()
                        {
                            self.ui.info(&format!(
                                "Sidecar archive commit: {}",
                                short_hash(planning_head)
                            ));
                        }
                        if let Some(command) =
                            single_advance_continuation_command(repo, &completed_state)
                        {
                            self.ui.info(
                                "More ordered agenda slices remain; this was a single iteration.",
                            );
                            self.ui.info(&format!("Continue as a campaign: {command}"));
                        }
                        return Ok(());
                    };

                    let iteration = campaign.iteration;
                    let max_iterations = campaign.max_iterations;
                    let change = require_change(&completed_state)?.to_owned();
                    if !campaign
                        .completed
                        .iter()
                        .any(|entry| entry.iteration == iteration)
                    {
                        completed_state
                            .campaign
                            .as_mut()
                            .expect("campaign was checked above")
                            .completed
                            .push(CompletedIteration {
                                iteration,
                                change: change.clone(),
                                proposal_head: completed_state.proposal_head.clone(),
                                final_head: completed_state.final_head.clone(),
                                elapsed_seconds: iteration_started
                                    .map(|started| started.elapsed().as_secs()),
                            });
                        persist_state(repo, &completed_state, &self.ui)?;
                    }
                    self.present_campaign(&completed_state);
                    self.ui.success(&format!(
                        "Campaign iteration {iteration} complete: `{change}`"
                    ));

                    if self.cli.no_loop || self.ui.stop_after_iteration_requested() {
                        self.ui.finish_dashboard();
                        self.ui.success(&format!(
                            "PAUSED after campaign iteration {iteration}: `{change}` is complete"
                        ));
                        return Ok(());
                    }
                    let request = completed_state.request.clone();
                    let pending_direction = completed_state.pending_direction.take();
                    let mut campaign = completed_state
                        .campaign
                        .take()
                        .expect("campaign was checked above");
                    campaign.iteration += 1;
                    let before_changes = openspec_snapshot(repo, &self.ui)?;
                    state = RunState::new(request, before_changes);
                    state.campaign = Some(campaign);
                    state.pending_direction = pending_direction;
                    state.terminal_remediations = completed_state.terminal_remediations;
                    self.assign_agenda(repo, &mut state)?;
                    arm_slice_baseline(repo, &mut state, &self.ui)?;
                    if state.stage != Stage::Done
                        && max_iterations.is_some_and(|maximum| iteration >= maximum)
                    {
                        let maximum = max_iterations.expect("campaign limit was checked above");
                        self.ui.finish_dashboard();
                        self.ui.success(&format!(
                            "PAUSED — cumulative campaign limit reached after {iteration} completed iteration(s); `{change}` is complete"
                        ));
                        self.ui.info(
                            "The campaign checkpoint and all completed iteration history were preserved.",
                        );
                        self.ui.info(&format!(
                            "Resume with a higher cumulative limit: {}",
                            campaign_limit_resume_command(repo, maximum)
                        ));
                        return Ok(());
                    }
                    persist_state(repo, &state, &self.ui)?;
                }
            }
        }
    }

    fn frontier_backend<'a>(
        &'a self,
        repo: &'a Path,
        launcher: &'a AgentLauncher,
        model_confusion_plugin: &Path,
    ) -> Box<dyn AgentBackend + 'a> {
        match launcher {
            AgentLauncher::Claude(launcher) => Box::new(
                ClaudeBackend::new(
                    repo,
                    launcher,
                    &self.cli.permission_mode,
                    self.ui.supports_stream_input(),
                    self.cli.stream_claude,
                    self.cli.max_output_retries,
                    &self.ui,
                )
                .with_provider_retries(self.cli.max_provider_retries)
                .with_plugin_dir(model_confusion_plugin),
            ),
            AgentLauncher::OpenCode(launcher) => Box::new(
                OpenCodeBackend::new(
                    repo,
                    launcher,
                    self.ui.supports_stream_input(),
                    self.cli.stream_claude,
                    &self.ui,
                )
                .with_retries(self.cli.max_output_retries, self.cli.max_provider_retries),
            ),
            AgentLauncher::Codex(launcher) => Box::new(
                CodexBackend::new(
                    repo,
                    launcher,
                    &self.cli.permission_mode,
                    self.cli.stream_claude,
                    &self.ui,
                )
                .with_retries(self.cli.max_output_retries, self.cli.max_provider_retries),
            ),
        }
    }

    fn run_frontier_replan(
        &self,
        repo: &Path,
        frontier_launcher: &AgentLauncher,
        mut state: RunState,
    ) -> Result<RunState> {
        let outcome = state
            .too_large
            .clone()
            .context("worker escalation omitted its durable outcome")?;
        let assignment = state
            .agenda
            .clone()
            .context("frontier subdivision currently requires an ordered agenda assignment")?;
        let baseline = state
            .slice_baseline
            .clone()
            .context("worker escalation has no recorded pre-Propose rollback baseline")?;
        if state.frontier_replans >= MAX_FRONTIER_REPLANS {
            reset_to_slice_baseline(repo, &baseline, &self.ui)?;
            bail!(
                "frontier subdivision reached its limit of {MAX_FRONTIER_REPLANS} successful replans for `{}` after restoring the pre-Propose baseline; the latest worker report was: {}",
                assignment.change,
                outcome.summary.trim()
            );
        }

        let rollback = reset_to_slice_baseline(repo, &baseline, &self.ui)?;
        self.ui.warn(&format!(
            "Abandoned the local {} attempt and restored pre-Propose HEAD {}",
            outcome.stage.title(),
            short_hash(&baseline.head)
        ));
        self.ui.info(&format!(
            "Failed-attempt diagnostic retained at `{}`",
            rollback.diagnostic_path.display()
        ));
        if let Some(reference) = &rollback.recovery_ref {
            self.ui
                .info(&format!("Failed-attempt commits retained at `{reference}`"));
        }
        let restored_changes = openspec_snapshot(repo, &self.ui)?;
        if restored_changes != state.before_changes {
            bail!(
                "pre-Propose rollback did not restore the recorded OpenSpec state; recovery data remains at `{}`",
                rollback.diagnostic_path.display()
            );
        }

        let model_confusion_plugin = ensure_model_confusion_plugin(repo, &self.ui)?;
        let frontier = self.frontier_backend(repo, frontier_launcher, &model_confusion_plugin);
        let session = SessionId::new(Uuid::new_v4().to_string());
        let stage_number = outcome.stage.number().saturating_sub(1).max(1);
        self.ui.stage(
            stage_number,
            AGENDA_TOTAL_STAGES,
            &model_stage_title("Frontier replan", "frontier", frontier_launcher),
        );
        let base_prompt = frontier_replan_prompt(&assignment, &outcome);
        let mut failure = None;
        for attempt in 0..2 {
            let prompt = match &failure {
                None => base_prompt.clone(),
                Some(failure) => format!(
                    "{base_prompt}\n\nPOSTCONDITION REPAIR: The preceding frontier attempt was rejected: {failure}\nThe repository has again been restored to the recorded pre-Propose baseline, including any pre-existing user edits. Perform and commit the required agenda-only subdivision now."
                ),
            };
            let result = match frontier.invoke(
                if attempt == 0 {
                    SessionMode::New {
                        id: session.clone(),
                        name: Some(format!("{}-frontier-replan", assignment.change)),
                    }
                } else {
                    SessionMode::Resume {
                        id: session.clone(),
                    }
                },
                &stage_prompt("", &prompt, StageProtocol::Frontier),
                if attempt == 0 {
                    "Frontier agent is subdividing the oversized agenda slice"
                } else {
                    "Frontier agent is correcting the agenda subdivision"
                },
                StageProtocol::Frontier,
            ) {
                Ok(result) => result,
                Err(error) => {
                    reset_to_slice_baseline(repo, &baseline, &self.ui)?;
                    return Err(error.context(
                        "frontier replan invocation failed; the pre-Propose baseline was restored",
                    ));
                }
            };
            match result.signal {
                StageSignal::Blocked => {
                    reset_to_slice_baseline(repo, &baseline, &self.ui)?;
                    return blocked("frontier replan", &result.text);
                }
                StageSignal::Replanned => {}
                other => {
                    reset_to_slice_baseline(repo, &baseline, &self.ui)?;
                    bail!("frontier replan returned unexpected terminal status {other:?}");
                }
            }

            match frontier_replan_postcondition(
                repo,
                &baseline,
                &assignment,
                &state.before_changes,
                &self.ui,
            ) {
                Ok(refreshed) => {
                    state.frontier_replans += 1;
                    state.too_large = None;
                    state.change = Some(refreshed.change.clone());
                    state.agenda = Some(refreshed);
                    state.stage = Stage::Propose;
                    state.planning_session = None;
                    state.planning_backend = None;
                    state.verify_retries = 0;
                    state.pending_repair = None;
                    state.proposal_head = None;
                    state.final_head = None;
                    state.before_changes = openspec_snapshot(repo, &self.ui)?;
                    arm_slice_baseline(repo, &mut state, &self.ui)?;
                    persist_state(repo, &state, &self.ui)?;
                    if let Err(error) = release_slice_baseline(repo, &baseline, &self.ui) {
                        self.ui.warn(&format!(
                            "Could not release superseded rollback metadata; workflow state is unaffected: {error}"
                        ));
                    }
                    self.ui.success(&format!(
                        "Frontier replanned `{}` into smaller ordered slices; restarting at Propose",
                        assignment.change
                    ));
                    return Ok(state);
                }
                Err(error) if attempt == 0 => {
                    failure = Some(error.to_string());
                    self.ui.warn(&format!(
                        "Frontier replan did not satisfy its repository postcondition: {}",
                        failure.as_deref().unwrap_or_default()
                    ));
                    reset_to_slice_baseline(repo, &baseline, &self.ui)?;
                }
                Err(error) => {
                    reset_to_slice_baseline(repo, &baseline, &self.ui)?;
                    return Err(error.context(
                        "frontier replan failed its repository postcondition twice; the pre-Propose baseline was restored",
                    ));
                }
            }
        }
        unreachable!("frontier retry loop always returns")
    }

    fn run_terminal_review(
        &self,
        repo: &Path,
        frontier_launcher: &AgentLauncher,
        mut state: RunState,
        escalation: Option<TooLargeOutcome>,
    ) -> Result<RunState> {
        let assignment = state
            .agenda
            .clone()
            .filter(is_terminal_assignment)
            .context("terminal review requires the canonical 9999 agenda assignment")?;
        let baseline = state
            .slice_baseline
            .clone()
            .context("terminal review has no recorded pre-Propose rollback baseline")?;

        if state.terminal_remediations >= MAX_TERMINAL_REMEDIATIONS {
            if escalation.is_some() {
                reset_to_slice_baseline(repo, &baseline, &self.ui)?;
            }
            bail!(
                "terminal acceptance reached its limit of {MAX_TERMINAL_REMEDIATIONS} frontier remediation rounds; `{}` remains the terminal gate and the pre-Propose state is preserved",
                assignment.change
            );
        }

        if let Some(outcome) = &escalation {
            let rollback = reset_to_slice_baseline(repo, &baseline, &self.ui)?;
            self.ui.warn(&format!(
                "Abandoned the terminal {} attempt and restored pre-Propose HEAD {}",
                outcome.stage.title(),
                short_hash(&baseline.head)
            ));
            self.ui.info(&format!(
                "Failed-attempt diagnostic retained at `{}`",
                rollback.diagnostic_path.display()
            ));
            if let Some(reference) = &rollback.recovery_ref {
                self.ui
                    .info(&format!("Failed-attempt commits retained at `{reference}`"));
            }
            if openspec_snapshot(repo, &self.ui)? != state.before_changes {
                bail!(
                    "terminal rollback did not restore the recorded OpenSpec state; recovery data remains at `{}`",
                    rollback.diagnostic_path.display()
                );
            }
        }

        let model_confusion_plugin = ensure_model_confusion_plugin(repo, &self.ui)?;
        let frontier = self.frontier_backend(repo, frontier_launcher, &model_confusion_plugin);
        let session = SessionId::new(Uuid::new_v4().to_string());
        self.present_campaign(&state);
        self.ui.stage(
            1,
            TOTAL_STAGES,
            &model_stage_title("Frontier acceptance review", "frontier", frontier_launcher),
        );
        let base_prompt = terminal_review_prompt(&assignment, escalation.as_ref());
        let mut postcondition_failure = None;

        for attempt in 0..2 {
            let prompt = match &postcondition_failure {
                None => base_prompt.clone(),
                Some(failure) => format!(
                    "{base_prompt}\n\nPOSTCONDITION REPAIR: The preceding frontier response was rejected: {failure}\nThe repository has again been restored to the exact pre-9999 baseline. Perform the review again and satisfy either the unchanged READY contract or the committed agenda-only REPLANNED contract."
                ),
            };
            let result = match frontier.invoke(
                if attempt == 0 {
                    SessionMode::New {
                        id: session.clone(),
                        name: Some("9999-frontier-acceptance-review".to_owned()),
                    }
                } else {
                    SessionMode::Resume {
                        id: session.clone(),
                    }
                },
                &stage_prompt("", &prompt, StageProtocol::TerminalReview),
                if attempt == 0 {
                    "Frontier agent is reviewing whole-project acceptance"
                } else {
                    "Frontier agent is correcting the acceptance review"
                },
                StageProtocol::TerminalReview,
            ) {
                Ok(result) => result,
                Err(error) => {
                    reset_to_slice_baseline(repo, &baseline, &self.ui)?;
                    return Err(error.context(
                        "frontier acceptance review failed; the pre-9999 baseline was restored",
                    ));
                }
            };

            let postcondition = match result.signal {
                StageSignal::Blocked => {
                    reset_to_slice_baseline(repo, &baseline, &self.ui)?;
                    return blocked("frontier acceptance review", &result.text);
                }
                StageSignal::Ready if escalation.is_some() => Err(anyhow::anyhow!(
                    "READY is not valid after a failed terminal attempt; insert bounded remediation slices"
                )),
                StageSignal::Ready => terminal_review_ready_postcondition(
                    repo,
                    &baseline,
                    &assignment,
                    &state.before_changes,
                    &self.ui,
                )
                .map(|_| None),
                StageSignal::Replanned => terminal_remediation_postcondition(
                    repo,
                    &baseline,
                    &assignment,
                    &state.before_changes,
                    &self.ui,
                )
                .map(Some),
                other => Err(anyhow::anyhow!(
                    "frontier acceptance review returned unexpected terminal status {other:?}"
                )),
            };

            match postcondition {
                Ok(None) => {
                    state.terminal_review_complete = true;
                    state.too_large = None;
                    persist_state(repo, &state, &self.ui)?;
                    self.ui.success(
                        "Frontier review confirmed readiness for whole-project acceptance",
                    );
                    return Ok(state);
                }
                Ok(Some(remediation)) => {
                    state.terminal_remediations += 1;
                    state.terminal_review_complete = false;
                    state.too_large = None;
                    state.change = Some(remediation.change.clone());
                    state.agenda = Some(remediation.clone());
                    state.stage = Stage::Propose;
                    state.planning_session = None;
                    state.planning_backend = None;
                    state.verify_retries = 0;
                    state.pending_repair = None;
                    state.proposal_head = None;
                    state.final_head = None;
                    state.frontier_replans = 0;
                    state.before_changes = openspec_snapshot(repo, &self.ui)?;
                    state.slice_baseline = None;
                    if let Err(error) = release_slice_baseline(repo, &baseline, &self.ui) {
                        self.ui.warn(&format!(
                            "Could not release superseded terminal rollback metadata; workflow state is unaffected: {error}"
                        ));
                    }
                    arm_slice_baseline(repo, &mut state, &self.ui)?;
                    persist_state(repo, &state, &self.ui)?;
                    self.ui.change_name(state.change.as_deref());
                    self.ui.success(&format!(
                        "Frontier inserted acceptance remediation `{}` before the unchanged 9999 gate",
                        remediation.change
                    ));
                    return Ok(state);
                }
                Err(error) if attempt == 0 => {
                    postcondition_failure = Some(error.to_string());
                    self.ui.warn(&format!(
                        "Frontier acceptance review failed its repository contract: {}",
                        postcondition_failure.as_deref().unwrap_or_default()
                    ));
                    reset_to_slice_baseline(repo, &baseline, &self.ui)?;
                }
                Err(error) => {
                    reset_to_slice_baseline(repo, &baseline, &self.ui)?;
                    return Err(error.context(
                        "frontier acceptance review failed its repository contract twice; the pre-9999 baseline was restored",
                    ));
                }
            }
        }
        unreachable!("terminal review retry loop always returns")
    }

    fn present_campaign(&self, state: &RunState) {
        let Some(campaign) = &state.campaign else {
            self.ui.campaign(None);
            return;
        };
        self.ui.campaign(Some(CampaignView {
            iteration: campaign.iteration,
            max_iterations: campaign.max_iterations,
            completed: campaign
                .completed
                .iter()
                .map(|entry| CampaignIterationView {
                    iteration: entry.iteration,
                    change: entry.change.clone(),
                    final_head: entry.final_head.clone(),
                    elapsed_seconds: entry.elapsed_seconds,
                })
                .collect(),
        }));
    }

    fn execute(
        &self,
        repo: &Path,
        launcher: &AgentLauncher,
        frontier_launcher: Option<&AgentLauncher>,
        commands: &SkillCommands,
        mut state: RunState,
    ) -> Result<WorkflowOutcome> {
        let model_confusion_plugin = ensure_model_confusion_plugin(repo, &self.ui)?;
        let opencode_server = match launcher {
            AgentLauncher::OpenCode(launcher) => {
                Some(Arc::new(OpenCodeServer::new(repo, launcher)))
            }
            AgentLauncher::Claude(_) | AgentLauncher::Codex(_) => None,
        };
        let codex_server = match launcher {
            AgentLauncher::Codex(launcher) => Some(Arc::new(CodexServer::new(repo, launcher))),
            AgentLauncher::Claude(_) | AgentLauncher::OpenCode(_) => None,
        };
        let agent: Box<dyn AgentBackend + '_> = match launcher {
            AgentLauncher::Claude(launcher) => {
                let client = ClaudeBackend::new(
                    repo,
                    launcher,
                    &self.cli.permission_mode,
                    self.ui.supports_stream_input(),
                    self.cli.stream_claude,
                    self.cli.max_output_retries,
                    &self.ui,
                )
                .with_provider_retries(self.cli.max_provider_retries)
                .with_plugin_dir(&model_confusion_plugin);
                let client = if let Some(product) = state.product_repo.as_deref() {
                    client
                        .with_additional_dir(product)
                        .with_resume_repo(product)
                } else {
                    client
                };
                Box::new(client)
            }
            AgentLauncher::OpenCode(launcher) => Box::new(
                OpenCodeBackend::new(
                    repo,
                    launcher,
                    self.ui.supports_stream_input(),
                    self.cli.stream_claude,
                    &self.ui,
                )
                .with_server(
                    opencode_server
                        .as_ref()
                        .expect("OpenCode launcher has a shared server")
                        .clone(),
                )
                .with_retries(self.cli.max_output_retries, self.cli.max_provider_retries),
            ),
            AgentLauncher::Codex(launcher) => {
                let client = CodexBackend::new(
                    repo,
                    launcher,
                    &self.cli.permission_mode,
                    self.cli.stream_claude,
                    &self.ui,
                )
                .with_server(
                    codex_server
                        .as_ref()
                        .expect("Codex launcher has a shared server")
                        .clone(),
                )
                .with_retries(self.cli.max_output_retries, self.cli.max_provider_retries);
                let client = if let Some(product) = state.product_repo.as_deref() {
                    client
                        .with_additional_dir(product)
                        .with_resume_repo(product)
                } else {
                    client
                };
                Box::new(client)
            }
        };
        let worker_agent: Box<dyn AgentBackend + '_> = match launcher {
            AgentLauncher::Claude(launcher) => {
                let client = ClaudeBackend::new(
                    repo,
                    launcher,
                    &self.cli.permission_mode,
                    true,
                    self.cli.stream_claude,
                    self.cli.max_output_retries,
                    &self.ui,
                )
                .with_provider_retries(self.cli.max_provider_retries)
                .with_plugin_dir(&model_confusion_plugin);
                let client = if let Some(product) = state.product_repo.as_deref() {
                    client
                        .with_additional_dir(product)
                        .with_resume_repo(product)
                } else {
                    client
                };
                let client = if state.bootstrap {
                    client
                } else {
                    client.with_stage_timeout(Duration::from_secs(
                        u64::from(self.cli.local_worker_timeout_minutes) * 60,
                    ))
                };
                Box::new(client)
            }
            AgentLauncher::OpenCode(launcher) => {
                let client =
                    OpenCodeBackend::new(repo, launcher, true, self.cli.stream_claude, &self.ui)
                        .with_server(
                            opencode_server
                                .as_ref()
                                .expect("OpenCode launcher has a shared server")
                                .clone(),
                        )
                        .with_retries(self.cli.max_output_retries, self.cli.max_provider_retries);
                let client = if state.bootstrap {
                    client
                } else {
                    client.with_stage_timeout(Duration::from_secs(
                        u64::from(self.cli.local_worker_timeout_minutes) * 60,
                    ))
                };
                Box::new(client)
            }
            AgentLauncher::Codex(launcher) => {
                let client = CodexBackend::new(
                    repo,
                    launcher,
                    &self.cli.permission_mode,
                    self.cli.stream_claude,
                    &self.ui,
                )
                .with_server(
                    codex_server
                        .as_ref()
                        .expect("Codex launcher has a shared server")
                        .clone(),
                )
                .with_retries(self.cli.max_output_retries, self.cli.max_provider_retries);
                let client = if let Some(product) = state.product_repo.as_deref() {
                    client
                        .with_additional_dir(product)
                        .with_resume_repo(product)
                } else {
                    client
                };
                let client = if state.bootstrap {
                    client
                } else {
                    client.with_stage_timeout(Duration::from_secs(
                        u64::from(self.cli.local_worker_timeout_minutes) * 60,
                    ))
                };
                Box::new(client)
            }
        };
        let frontier_agent = frontier_launcher
            .map(|launcher| self.frontier_backend(repo, launcher, &model_confusion_plugin));
        self.ui.change_name(state.change.as_deref());

        loop {
            if self.cli.yolo && state.stage == Stage::ProposalCommit {
                self.ui
                    .warn("YOLO mode: skipping the proposal milestone commit");
                state.stage = Stage::Apply;
                persist_state(repo, &state, &self.ui)?;
            }
            if state.too_large.is_some() {
                return Ok(WorkflowOutcome::TooLarge(state));
            }
            if state.stage == Stage::Complete {
                release_state_baseline(repo, &mut state, &self.ui);
                persist_state(repo, &state, &self.ui)?;
                return Ok(WorkflowOutcome::Complete(state));
            }
            if state.stage == Stage::Done {
                release_state_baseline(repo, &mut state, &self.ui);
                persist_state(repo, &state, &self.ui)?;
                return Ok(WorkflowOutcome::Done(state));
            }

            let use_terminal_frontier = uses_terminal_frontier(&state, frontier_agent.is_some());
            let stage_uses_frontier =
                stage_uses_frontier_model(state.stage, state.bootstrap, use_terminal_frontier);
            let stage_role = if stage_uses_frontier {
                "frontier"
            } else {
                "worker"
            };
            let (stage_number, total_stages) = displayed_stage_position(
                state.stage,
                state.agenda.is_some(),
                use_terminal_frontier,
                self.cli.yolo,
            );
            let stage_title = if stage_uses_frontier {
                model_stage_title(
                    state.stage.title(),
                    stage_role,
                    frontier_launcher.context("frontier stage requires a frontier connection")?,
                )
            } else {
                model_stage_title(state.stage.title(), stage_role, launcher)
            };
            self.ui.stage(stage_number, total_stages, &stage_title);
            let planning_agent: &dyn AgentBackend = if use_terminal_frontier {
                frontier_agent
                    .as_deref()
                    .context("terminal Propose requires a frontier connection")?
            } else {
                worker_agent.as_ref()
            };
            let verifying_agent: &dyn AgentBackend = if use_terminal_frontier {
                frontier_agent
                    .as_deref()
                    .context("terminal Verify requires a frontier connection")?
            } else {
                worker_agent.as_ref()
            };
            let applying_agent: &dyn AgentBackend = if use_terminal_frontier {
                frontier_agent
                    .as_deref()
                    .context("terminal Apply/repair requires a frontier connection")?
            } else {
                worker_agent.as_ref()
            };
            match state.stage {
                Stage::Explore => {
                    self.run_explore(repo, worker_agent.as_ref(), commands, &mut state)?
                }
                Stage::Propose => self.run_propose(
                    repo,
                    planning_agent,
                    commands,
                    &mut state,
                    use_terminal_frontier,
                )?,
                Stage::ProposalCommit => {
                    self.run_proposal_commit(repo, agent.as_ref(), &mut state)?
                }
                Stage::Apply => self.run_apply(
                    repo,
                    applying_agent,
                    commands,
                    &mut state,
                    use_terminal_frontier,
                )?,
                Stage::Verify => self.run_verify(
                    repo,
                    verifying_agent,
                    commands,
                    &mut state,
                    use_terminal_frontier,
                )?,
                Stage::Repair => self.run_repair(
                    repo,
                    applying_agent,
                    commands,
                    &mut state,
                    use_terminal_frontier,
                )?,
                Stage::Archive => self.run_archive(repo, agent.as_ref(), commands, &mut state)?,
                Stage::FinalCommit => self.run_final_commit(repo, agent.as_ref(), &mut state)?,
                Stage::Complete | Stage::Done => unreachable!(),
            }
        }
    }

    fn run_explore(
        &self,
        repo: &Path,
        claude: &dyn AgentBackend,
        commands: &SkillCommands,
        state: &mut RunState,
    ) -> Result<()> {
        let session = planning_session(state, claude)?;
        persist_state(repo, state, &self.ui)?;
        let subject = campaign_subject(state);
        let Some(result) = worker_result(
            claude.invoke(
                session.clone(),
                &stage_prompt(&commands.explore, &subject, StageProtocol::Worker),
                "Worker agent is exploring the change",
                StageProtocol::Worker,
            ),
            repo,
            state,
            &self.ui,
        )?
        else {
            return Ok(());
        };
        let session = result
            .session_id
            .as_deref()
            .map(SessionId::new)
            .unwrap_or_else(|| session.id().clone());
        state.planning_session = Some(session.clone());
        persist_state(repo, state, &self.ui)?;
        if !handle_worker_result(
            repo,
            state,
            "explore",
            &result.text,
            result.signal,
            &self.ui,
        )? {
            return Ok(());
        }
        state.stage = state.stage.after_ready()?;
        persist_state(repo, state, &self.ui)?;
        claude.compact_session(&session, "Explore")?;
        Ok(())
    }

    fn run_propose(
        &self,
        repo: &Path,
        claude: &dyn AgentBackend,
        commands: &SkillCommands,
        state: &mut RunState,
        frontier_terminal: bool,
    ) -> Result<()> {
        let mut session = planning_session(state, claude)?;
        persist_state(repo, state, &self.ui)?;
        let planning_context = if state.bootstrap {
            BOOTSTRAP_PROPOSAL_CONTEXT
        } else if is_terminal_gate(state) && frontier_terminal {
            "Act as the frontier architect that set the project goal. Formulate the terminal whole-project acceptance change from the exact 9999 agenda assignment, the complete goal in `openspec/config.yaml`, canonical and archived OpenSpec evidence, and the delivered repository. The resulting specs, design, tasks, and acceptance scenarios must test the original goal as an integrated whole rather than merely restating the final agenda file. This is planning-only and must not implement product code. Do not report DONE: the orchestrator has established that the terminal acceptance gate remains."
        } else if state.agenda.is_some() {
            "Use the assigned agenda slice as the planning authority and preserve correct partial artifacts for its exact assigned change. Do not report DONE: the orchestrator has already established that this agenda slice remains."
        } else if state.change.is_some() {
            "Use the exact prescribed change name and create or continue only that OpenSpec change. Do not substitute a generated name or modify another active change."
        } else {
            "Use conclusions from exploration and preserve correct partial proposal artifacts from any interrupted attempt. If the requested objective is already satisfied and no coherent implementation work remains, do not create or modify OpenSpec artifacts; report DONE."
        };
        let base = format!(
            "{}\n\n{planning_context} Run every OpenSpec command synchronously: never background or detach commands, and do not return while a command or subagent is still working.",
            campaign_subject(state)
        );
        let mut retrying_postcondition = false;
        loop {
            let subject = if retrying_postcondition {
                proposal_postcondition_repair(
                    &base,
                    "The preceding READY result did not satisfy the deterministic OpenSpec postcondition.",
                )
            } else {
                base.clone()
            };
            let Some(result) = worker_result(
                claude.invoke(
                    session.clone(),
                    &stage_prompt(&commands.propose, &subject, StageProtocol::Propose),
                    if retrying_postcondition && frontier_terminal {
                        "Frontier agent is correcting the terminal acceptance proposal"
                    } else if retrying_postcondition {
                        "Worker agent is correcting the incomplete proposal"
                    } else if frontier_terminal {
                        "Frontier agent is defining final project acceptance"
                    } else {
                        "Worker agent is creating OpenSpec artifacts"
                    },
                    StageProtocol::Propose,
                ),
                repo,
                state,
                &self.ui,
            )?
            else {
                return Ok(());
            };
            session = SessionMode::Resume {
                id: result
                    .session_id
                    .as_deref()
                    .map(SessionId::new)
                    .unwrap_or_else(|| session.id().clone()),
            };
            state.planning_session = Some(session.id().clone());
            persist_state(repo, state, &self.ui)?;

            let mut after = openspec_snapshot(repo, &self.ui)?;
            match result.signal {
                StageSignal::Done => {
                    if let Some(assignment) = &state.agenda {
                        bail!(
                            "Propose returned DONE despite assigned agenda slice `{}`; the assignment remains preserved in the checkpoint",
                            assignment.path
                        );
                    }
                    if after != state.before_changes {
                        bail!(
                            "Propose returned DONE after changing OpenSpec state; preserving the repository for inspection"
                        );
                    }
                    state.terminal_summary = Some(result.text);
                    state.stage = Stage::Done;
                    return persist_state(repo, state, &self.ui);
                }
                StageSignal::TooLarge => {
                    return record_too_large(repo, state, result.text, &self.ui);
                }
                StageSignal::Ready => {}
                StageSignal::Blocked => return blocked("propose", &result.text),
                other => bail!("propose returned unexpected terminal status {other:?}"),
            }

            let assigned_change = state
                .agenda
                .as_ref()
                .map(|assignment| assignment.change.as_str())
                .or(state.change.as_deref());
            if let Some(assigned) = assigned_change
                && let Some(created) =
                    numeric_prefix_reconciliation_candidate(&state.before_changes, &after, assigned)
            {
                self.ui.warn(&format!(
                    "Propose dropped the numeric prefix from `{assigned}`; reconciling unambiguous new change `{created}`"
                ));
                rename_change_directory(repo, &created, assigned)?;
                after = openspec_snapshot(repo, &self.ui)?;
            }
            let assigned_complete_without_list_change = assigned_change
                .filter(|change| after.changes.contains_key(*change))
                .map(|change| {
                    planning_status(repo, change, &self.ui).map(|status| status.is_complete)
                })
                .transpose()?
                .unwrap_or(false);
            if after == state.before_changes && !assigned_complete_without_list_change {
                if !retrying_postcondition {
                    self.ui.warn(
                        "Propose reported READY without creating or modifying an active OpenSpec change; retrying the same planning session once",
                    );
                    retrying_postcondition = true;
                    continue;
                }
                bail!(
                    "Propose still created or modified no active OpenSpec change after one corrective turn. Last agent summary: {}",
                    result.text.trim()
                );
            }

            let change_result = match assigned_change {
                Some(change) if assigned_complete_without_list_change => Ok(change.to_owned()),
                Some(change) => identify_assigned_change(&state.before_changes, &after, change),
                None => identify_change(&state.before_changes, &after),
            };
            let change = change_result.with_context(|| {
                if retrying_postcondition {
                    format!(
                        "Propose still did not satisfy its READY postcondition after one corrective turn. Last agent summary: {}",
                        result.text.trim()
                    )
                } else {
                    format!("Propose reported READY. Last agent summary: {}", result.text.trim())
                }
            })?;
            validate_change_name(&change)?;
            let status = planning_status(repo, &change, &self.ui)?;
            if !status.is_complete {
                let next_steps = if status.next_steps.is_empty() {
                    "OpenSpec reported no next-step detail".to_owned()
                } else {
                    status.next_steps.join("; ")
                };
                if !retrying_postcondition {
                    self.ui.warn(&format!(
                        "Propose reported READY for `{change}`, but OpenSpec planning is incomplete ({next_steps}); retrying the same planning session once"
                    ));
                    retrying_postcondition = true;
                    continue;
                }
                bail!(
                    "Propose still left OpenSpec change `{change}` planning-incomplete after one corrective turn. Remaining work: {next_steps}. Last agent summary: {}",
                    result.text.trim()
                );
            }
            state.change = Some(change.clone());
            self.ui.change_name(Some(&change));
            self.ui
                .info(&format!("Selected OpenSpec change `{change}`"));
            if let Err(error) = claude.rename_session(session.id(), &change) {
                self.ui
                    .warn(&format!("Could not rename planning session: {error}"));
            }
            state.planning_session = None;
            state.planning_backend = None;
            state.stage = stage_after_propose(self.cli.yolo);
            persist_state(repo, state, &self.ui)?;
            return Ok(());
        }
    }

    fn run_proposal_commit(
        &self,
        repo: &Path,
        claude: &dyn AgentBackend,
        state: &mut RunState,
    ) -> Result<()> {
        let change = require_change(state)?;
        let commit_message = proposal_commit_message(&change);
        let task = if state.bootstrap {
            format!(
                "Create the proposal milestone Git commit for bootstrap OpenSpec change `{change}`. Inspect Git status and diffs. Commit the generated bootstrap scaffold (`openspec/config.yaml`, `automation/bootstrap.md`, the opsx-build managed fragments in `CLAUDE.md` and `AGENTS.md`, and OpenSpec's project-local agent integration) together with the proposal artifacts for this exact change. Do not commit the source Markdown passed to the bootstrap command merely because it is present. {commit_message} Preserve all unrelated work. Never reset, stash, restore, discard, amend, or rewrite existing history. If the relevant proposal work is already committed and nothing remains to commit, confirm that and report READY."
            )
        } else if let Some(product_repo) = state.product_repo.as_deref() {
            format!(
                "Create the proposal milestone Git commit for sidecar OpenSpec change `{change}`. The current working directory is the planning repository. Commit all planning files belonging to this exact change, including its OpenSpec artifacts and sidecar-local agent integration. {commit_message} Do not create a commit in the product repository `{product}` during this stage. Preserve unrelated work and never reset, stash, restore, discard, amend, or rewrite history. If the planning work is already committed, confirm that and report READY.",
                product = product_repo.display()
            )
        } else {
            format!(
                "Create the proposal milestone Git commit for OpenSpec change `{change}`. Inspect Git status and diffs. Commit only the proposal artifacts for this change and directly related canonical OpenSpec specification updates. {commit_message} Preserve all unrelated work. Never reset, stash, restore, discard, amend, or rewrite existing history. If the relevant proposal work is already committed and nothing remains to commit, confirm that and report READY."
            )
        };
        let baseline = current_head(repo, &self.ui)?;
        let result = invoke_fresh(
            claude,
            &format!("{change}-proposal-commit"),
            &stage_prompt("", &task, StageProtocol::Ready),
            "Worker agent is committing the proposal",
            StageProtocol::Ready,
        );
        verify_milestone_ancestry(repo, baseline.as_deref(), "proposal", &self.ui)?;
        let result = result?;
        require_ready("proposal commit", &result.text, result.signal)?;
        state.proposal_head = current_head(repo, &self.ui)?;
        state.stage = state.stage.after_ready()?;
        persist_state(repo, state, &self.ui)
    }

    fn run_apply(
        &self,
        repo: &Path,
        claude: &dyn AgentBackend,
        commands: &SkillCommands,
        state: &mut RunState,
        frontier_terminal: bool,
    ) -> Result<()> {
        let change = require_change(state)?;
        let base = if state.bootstrap {
            format!("{change}\n\n{BOOTSTRAP_APPLY_CONTEXT}")
        } else if frontier_terminal {
            format!(
                "{change}\n\nImplement or continue implementing this terminal whole-project acceptance change completely. This is frontier-owned work: exercise the real acceptance criteria against the delivered project, preserve correct partial work, and replace incorrect task-owned work where necessary. Run the required project checks, using narrowly scoped sandbox escape when the checks genuinely require facilities unavailable inside the sandbox. Do not archive the change. Report TOO_LARGE only if the terminal acceptance work cannot safely be completed even by the frontier model; do not subdivide merely because it would be too large for the worker model."
            )
        } else {
            format!(
                "{change}\n\nImplement or continue implementing this OpenSpec change completely. Preserve correct partial work and run appropriate project checks. Do not archive the change. If the assigned slice cannot reliably be completed and verified as one bounded worker-model change, report TOO_LARGE with evidence and an ordered decomposition instead of digging an increasingly broad implementation hole. Do not use TOO_LARGE for ordinary difficulty or correctable engineering failures."
            )
        };
        let base = with_sidecar_context(state, &base);
        let subject = with_direction(&base, state.pending_direction.as_deref());
        let Some(result) = worker_result(
            invoke_fresh(
                claude,
                &format!("{change}-apply"),
                &stage_prompt(&commands.apply, &subject, StageProtocol::Worker),
                if frontier_terminal {
                    "Frontier agent is applying whole-project acceptance"
                } else {
                    "Worker agent is applying the OpenSpec change"
                },
                StageProtocol::Worker,
            ),
            repo,
            state,
            &self.ui,
        )?
        else {
            return Ok(());
        };
        if !handle_worker_result(repo, state, "apply", &result.text, result.signal, &self.ui)? {
            return Ok(());
        }
        state.consume_direction();
        state.stage = state.stage.after_ready()?;
        persist_state(repo, state, &self.ui)
    }

    fn run_verify(
        &self,
        repo: &Path,
        claude: &dyn AgentBackend,
        commands: &SkillCommands,
        state: &mut RunState,
        frontier_terminal: bool,
    ) -> Result<()> {
        let change = require_change(state)?;
        let verify_subject = if state.bootstrap {
            format!(
                "{change}\n\nVerify the generated implementation agenda against `automation/bootstrap.md` and the complete project goal in `openspec/config.yaml`. Confirm that no product code was implemented, every material goal is assigned, each slice is independently bounded for the worker model, and `9999-project-acceptance.md` is a genuine whole-project DONE gate. Report RETRY with all concrete corrections when it is not."
            )
        } else if frontier_terminal {
            format!(
                "{change}\n\nPerform the independent frontier verification of the complete project goal. Verify this terminal change against its OpenSpec artifacts, then compare the delivered repository with every material requirement in `openspec/config.yaml`, the ordered agenda including `automation/slices/9999-project-acceptance.md`, synchronized canonical specs, archived change evidence, and the real end-to-end acceptance results. Run the required whole-project checks rather than relying only on task checkboxes or earlier summaries. Report VERIFIED only when the original project goal is demonstrably complete. Report RETRY with concrete findings for bounded correctable defects; make clear when a finding represents missing functionality broad enough to require new remediation slices before the terminal gate."
            )
        } else {
            format!(
                "{change}\n\nVerify the implementation against its OpenSpec artifacts and run relevant checks. Report RETRY only for a concrete, correctable implementation issue and explain the required repair."
            )
        };
        let verify_subject = with_sidecar_context(state, &verify_subject);
        let Some(result) = worker_result(
            invoke_fresh(
                claude,
                &format!("{change}-verify-{}", state.verify_retries + 1),
                &stage_prompt(&commands.verify, &verify_subject, StageProtocol::Verify),
                if frontier_terminal {
                    "Frontier agent is verifying the complete project goal"
                } else {
                    "Worker agent is verifying specification compliance"
                },
                StageProtocol::Verify,
            ),
            repo,
            state,
            &self.ui,
        )?
        else {
            return Ok(());
        };
        match result.signal {
            StageSignal::Verified => {
                if state.bootstrap {
                    match validate_bootstrap_agenda(repo) {
                        Ok(count) => self.ui.success(&format!(
                            "Bootstrap agenda passed structural checks ({count} slices)"
                        )),
                        Err(error) => {
                            state.verify_retries += 1;
                            if state.verify_retries > self.cli.max_verify_retries {
                                bail!(
                                    "bootstrap agenda still failed its structural postcondition after {} repair cycle(s): {error}",
                                    self.cli.max_verify_retries
                                );
                            }
                            state.pending_repair = Some(format!(
                                "OpenSpec verification reported success, but the deterministic bootstrap agenda check failed: {error}. Correct the agenda without implementing product code."
                            ));
                            state.stage = Stage::Repair;
                            persist_state(repo, state, &self.ui)?;
                            self.ui.warn(&format!(
                                "Bootstrap agenda requires structural repair {}/{}: {error}",
                                state.verify_retries, self.cli.max_verify_retries
                            ));
                            return Ok(());
                        }
                    }
                }
                self.ui.success("Verification passed");
                state.pending_repair = None;
                state.stage = state.stage.after_verified()?;
                persist_state(repo, state, &self.ui)
            }
            StageSignal::Retry => {
                state.verify_retries += 1;
                state.pending_repair = Some(result.text.clone());
                if state.verify_retries > self.cli.max_verify_retries {
                    let summary = if frontier_terminal {
                        format!(
                            "frontier acceptance review still found material project-goal gaps after {} frontier repair cycle(s); insert bounded remediation slices before the terminal gate. Latest findings:\n{}",
                            self.cli.max_verify_retries,
                            result.text.trim()
                        )
                    } else {
                        format!(
                            "local worker exhausted {} verification repair cycle(s); the failed attempt requires frontier subdivision",
                            self.cli.max_verify_retries
                        )
                    };
                    return record_too_large(repo, state, summary, &self.ui);
                }
                state.stage = Stage::Repair;
                persist_state(repo, state, &self.ui)?;
                self.ui.warn(&format!(
                    "Verification requested repair {}/{}",
                    state.verify_retries, self.cli.max_verify_retries
                ));
                Ok(())
            }
            StageSignal::Blocked => blocked("verify", &result.text),
            StageSignal::Ready
            | StageSignal::Done
            | StageSignal::TooLarge
            | StageSignal::Replanned => {
                bail!(
                    "verify returned an invalid terminal status instead of VERIFIED, RETRY, or BLOCKED"
                )
            }
        }
    }

    fn run_repair(
        &self,
        repo: &Path,
        claude: &dyn AgentBackend,
        commands: &SkillCommands,
        state: &mut RunState,
        frontier_terminal: bool,
    ) -> Result<()> {
        if state.verify_retries > self.cli.max_verify_retries && state.pending_direction.is_none() {
            let summary = if frontier_terminal {
                format!(
                    "frontier terminal acceptance exhausted {} verification repair cycle(s); the remaining findings require bounded remediation slices before the terminal gate",
                    self.cli.max_verify_retries
                )
            } else {
                format!(
                    "local worker exhausted {} verification repair cycle(s); the failed attempt requires frontier subdivision",
                    self.cli.max_verify_retries
                )
            };
            return record_too_large(repo, state, summary, &self.ui);
        }
        let change = require_change(state)?;
        let finding = state.pending_repair.as_deref().unwrap_or(
            "No verifier finding was supplied; apply the user's direction and re-run relevant checks.",
        );
        let base = if state.bootstrap {
            format!(
                "{change}\n\nRepair the planning-only implementation agenda and its bootstrap OpenSpec artifacts. Do not implement product code and do not create implementation OpenSpec changes. Re-check the complete goal in `openspec/config.yaml`.\n\nVerifier context:\n{finding}"
            )
        } else if frontier_terminal {
            format!(
                "{change}\n\nRepair or continue repairing the terminal whole-project acceptance implementation and tests. This is frontier-owned work. Preserve correct partial work, correct task-owned defects, run the real acceptance checks, and keep the approved OpenSpec scope. Use narrowly scoped sandbox escape when required checks genuinely cannot run inside the sandbox. Report TOO_LARGE only if the work cannot safely be completed even by the frontier model.\n\nVerifier context:\n{finding}"
            )
        } else {
            format!(
                "{change}\n\nRepair or continue repairing the implementation and tests. Preserve correct partial work and the approved OpenSpec scope. Re-run relevant checks. If the verifier has exposed that the assigned slice cannot reliably fit one bounded worker-model change, report TOO_LARGE with evidence and an ordered decomposition. Do not use TOO_LARGE for an ordinary correctable verification failure.\n\nVerifier context:\n{finding}"
            )
        };
        let base = with_sidecar_context(state, &base);
        let subject = with_direction(&base, state.pending_direction.as_deref());
        let Some(result) = worker_result(
            invoke_fresh(
                claude,
                &format!("{change}-repair-{}", state.verify_retries),
                &stage_prompt(&commands.apply, &subject, StageProtocol::Worker),
                if frontier_terminal {
                    "Frontier agent is repairing whole-project acceptance"
                } else {
                    "Worker agent is repairing the implementation"
                },
                StageProtocol::Worker,
            ),
            repo,
            state,
            &self.ui,
        )?
        else {
            return Ok(());
        };
        if !handle_worker_result(repo, state, "repair", &result.text, result.signal, &self.ui)? {
            return Ok(());
        }
        state.pending_repair = None;
        state.consume_direction();
        state.stage = state.stage.after_ready()?;
        persist_state(repo, state, &self.ui)
    }

    fn run_archive(
        &self,
        repo: &Path,
        claude: &dyn AgentBackend,
        commands: &SkillCommands,
        state: &mut RunState,
    ) -> Result<()> {
        let change = require_change(state)?;
        let subject = archive_subject(&change);
        let subject = with_sidecar_context(state, &subject);
        for attempt in 0..=1 {
            let attempt_subject = if attempt == 0 {
                subject.clone()
            } else {
                format!(
                    "{subject}\n\nRECOVERY: The preceding attempt ended without a usable terminal result while this OpenSpec change remained active. Inspect durable OpenSpec state and complete the archive now. Invoke actual agent tools one at a time; do not print XML, JSON, or any other textual representation of intended tool calls."
                )
            };
            let result = invoke_fresh(
                claude,
                &format!("{change}-archive"),
                &stage_prompt(&commands.archive, &attempt_subject, StageProtocol::Ready),
                if attempt == 0 {
                    "Worker agent is archiving the OpenSpec change"
                } else {
                    "Worker agent is retrying the OpenSpec archive"
                },
                StageProtocol::Ready,
            );
            let result = match result {
                Ok(result) => result,
                Err(error) if error.downcast_ref::<PauseRequested>().is_some() => {
                    return Err(error);
                }
                Err(error) => match openspec_snapshot(repo, &self.ui) {
                    Ok(changes) if archive_is_absent(&changes, &change) => {
                        self.ui.warn(
                            "Archive completed but the worker agent omitted its terminal result; accepting OpenSpec state",
                        );
                        state.stage = state.stage.after_ready()?;
                        return persist_state(repo, state, &self.ui);
                    }
                    Ok(_) if attempt == 0 && is_missing_terminal_result(&error) => {
                        self.ui.warn(
                            "The worker agent omitted its Archive terminal result while the change remains active; retrying once in a fresh session",
                        );
                        continue;
                    }
                    Ok(_) => return Err(error),
                    Err(check_error) => {
                        return Err(error.context(format!(
                            "could not also confirm archive state: {check_error}"
                        )));
                    }
                },
            };
            require_ready("archive", &result.text, result.signal)?;
            state.stage = state.stage.after_ready()?;
            return persist_state(repo, state, &self.ui);
        }
        unreachable!("Archive retry loop always returns")
    }

    fn run_final_commit(
        &self,
        repo: &Path,
        claude: &dyn AgentBackend,
        state: &mut RunState,
    ) -> Result<()> {
        let change = require_change(state)?;
        let commit_message = completion_commit_message(&change);
        let planning_baseline = current_head(repo, &self.ui)?;
        let product_repo = state.product_repo.clone();
        let product_baseline = product_repo
            .as_deref()
            .map(|product| current_head(product, &self.ui))
            .transpose()?
            .flatten();
        let task = if state.bootstrap {
            format!(
                "Create the completion milestone Git commit for bootstrap OpenSpec change `{change}`. Inspect Git status, history, and diffs. Commit the generated `automation/slices/` agenda, archived bootstrap change artifacts, and any remaining files belonging only to this bootstrap workflow. {commit_message} Preserve all unrelated work, including the source Markdown supplied to the bootstrap command unless it was already deliberately tracked as project documentation. Never reset, stash, restore, discard, amend, or rewrite existing history. If all relevant work is already committed and nothing remains to commit, confirm that and report READY."
            )
        } else if let Some(product_repo) = product_repo.as_deref() {
            format!(
                "Complete the two Git milestones for sidecar OpenSpec change `{change}`. First inspect the product repository `{product}` and commit only the implementation, tests, and product documentation belonging to this change there. {commit_message} Obtain that product commit hash. Then inspect the current planning repository and commit its synchronized specifications, archived change artifacts, sidecar metadata, and other planning-only files with subject exactly `openspec: archive {change}`. Give that planning commit a concise imperative-mood body summarizing the synchronized and archived specification outcome. Apply the same short-paragraph, blank-line, 72-column wrapping, and no-inventory rules as the product commit, then add a final `Product-Commit: <hash>` trailer. Preserve unrelated work in both repositories. Never reset, stash, restore, discard, amend, or rewrite either history. If one repository's relevant work is already committed, preserve it and still complete the other milestone. Report READY only after both repositories have no uncommitted work belonging to this change.",
                product = product_repo.display()
            )
        } else {
            format!(
                "Create the completion milestone Git commit for OpenSpec change `{change}`. Inspect Git status, history, and diffs. Commit the implementation, tests, synchronized specifications, archived change artifacts, and documentation that belong to this completed change. {commit_message} Preserve all unrelated work. Never reset, stash, restore, discard, amend, or rewrite existing history. If all relevant work is already committed and nothing remains to commit, confirm that and report READY."
            )
        };
        let result = invoke_fresh(
            claude,
            &format!("{change}-completion-commit"),
            &stage_prompt("", &task, StageProtocol::Ready),
            "Worker agent is committing the completed change",
            StageProtocol::Ready,
        );
        verify_milestone_ancestry(
            repo,
            planning_baseline.as_deref(),
            if product_repo.is_some() {
                "sidecar archive"
            } else {
                "completion"
            },
            &self.ui,
        )?;
        if let Some(product_repo) = product_repo.as_deref() {
            verify_milestone_ancestry(
                product_repo,
                product_baseline.as_deref(),
                "product completion",
                &self.ui,
            )?;
        }
        let result = result?;
        require_ready("completion commit", &result.text, result.signal)?;
        if let Some(product_repo) = product_repo.as_deref() {
            state.final_head = current_head(product_repo, &self.ui)?;
            state.planning_final_head = current_head(repo, &self.ui)?;
        } else {
            state.final_head = current_head(repo, &self.ui)?;
        }
        state.stage = state.stage.after_ready()?;
        persist_state(repo, state, &self.ui)
    }

    fn print_new_dry_run(
        &self,
        repo: &Path,
        launcher: &AgentLauncher,
        frontier_launcher: Option<&AgentLauncher>,
        commands: &SkillCommands,
    ) -> Result<()> {
        self.ui
            .warn("DRY RUN — no workflow state or subprocesses will be created");
        let mut dry_state = if self.cli.loop_workflow {
            RunState::new_campaign(
                self.cli.request.clone(),
                ChangeSnapshot::default(),
                self.cli.max_iterations,
            )
        } else {
            RunState::new(self.cli.request.clone(), ChangeSnapshot::default())
        };
        self.assign_requested_change(&mut dry_state)?;
        self.assign_agenda(repo, &mut dry_state)?;
        if dry_state.stage == Stage::Done {
            self.ui.info("Advance: the ordered agenda is complete");
            return Ok(());
        }
        let terminal_frontier = is_terminal_gate(&dry_state) && frontier_launcher.is_some();
        if terminal_frontier {
            self.ui.info(
                "Frontier acceptance review: confirm readiness or commit bounded remediation slices before 9999",
            );
            self.ui
                .info("Terminal routing: frontier Propose/Apply/repair/Verify; worker milestone commits/archive");
        }
        let (planning_command, protocol, label) = if dry_state.stage == Stage::Propose {
            (&commands.propose, StageProtocol::Propose, "Propose")
        } else {
            (&commands.explore, StageProtocol::Worker, "Explore")
        };
        let prompt = stage_prompt(planning_command, &campaign_subject(&dry_state), protocol);
        let session = SessionMode::New {
            id: SessionId::new(Uuid::nil().to_string()),
            name: Some("opsx-build-planning".to_owned()),
        };
        let selected_launcher = if terminal_frontier {
            frontier_launcher.context("terminal dry run requires a frontier connection")?
        } else {
            launcher
        };
        let command = build_agent_dry_run_command(
            repo,
            selected_launcher,
            &self.cli.permission_mode,
            &session,
            &prompt,
            protocol,
        );
        self.ui.info(&format!("{label}: {}", command.display()));
        if dry_state.stage == Stage::Explore {
            self.ui
                .info("Hard compact the planning session after Explore");
            self.ui.info(&format!(
                "Propose in same session: {} (READY, DONE, TOO_LARGE, or BLOCKED)",
                commands.propose
            ));
        }
        if self.cli.yolo {
            self.ui
                .warn("YOLO mode: skip the proposal milestone commit");
        } else {
            self.ui.info(
                "Ask the worker agent in a fresh session to create the proposal milestone commit",
            );
        }
        self.ui.info(&format!("Apply: {}", commands.apply));
        self.ui.info(&format!(
            "Verify/repair: {} / {}",
            commands.verify, commands.apply
        ));
        self.ui.info(&format!("Archive: {}", commands.archive));
        self.ui
            .info("Ask the worker agent to create the completion milestone commit");
        if let Some(frontier_launcher) = frontier_launcher {
            self.ui.info(&format!(
                "Frontier fallback: {} after local TOO_LARGE or a {} minute worker timeout",
                frontier_launcher.program(),
                self.cli.local_worker_timeout_minutes
            ));
        } else {
            self.ui
                .info("LOCAL-ONLY: stop on TOO_LARGE; never invoke frontier fallback");
        }
        if self.cli.loop_workflow {
            let limit = self.cli.max_iterations.map_or_else(
                || "until the agenda or objective is complete".to_owned(),
                |maximum| format!("until completion or {maximum} completed iterations"),
            );
            self.ui.info(&format!(
            "Campaign: repeat the complete workflow {limit}; frontier-replan an oversized agenda slice, and stop on BLOCKED or an unrecoverable error"
        ));
        }
        Ok(())
    }

    fn print_resume_dry_run(&self, state: &RunState, commands: &SkillCommands) -> Result<()> {
        self.ui
            .warn("DRY RUN — checkpoint and repository will not be changed");
        let next_stage = if self.cli.yolo && state.stage == Stage::ProposalCommit {
            Stage::Apply
        } else {
            state.stage
        };
        self.ui.info(&format!("Next stage: {}", next_stage.title()));
        if self.cli.yolo && state.stage == Stage::ProposalCommit {
            self.ui
                .warn("YOLO mode: the saved proposal commit stage would be skipped");
        }
        if let Some(campaign) = &state.campaign {
            self.ui.info(&format!(
                "Campaign iteration {}; {} completed change(s); continue after this workflow",
                campaign.iteration,
                campaign.completed.len()
            ));
        }
        if let Some(direction) = &state.pending_direction {
            self.ui
                .info(&format!("Pending one-shot direction: {direction}"));
        }
        if matches!(next_stage, Stage::Apply | Stage::Repair | Stage::Verify) {
            self.ui
                .info(&format!("Implementation workflow: {}", commands.apply));
            self.ui
                .info(&format!("Verification workflow: {}", commands.verify));
        }
        if is_terminal_gate(state) {
            self.ui.info(
                "Terminal routing: frontier readiness review/Propose/Apply/repair/Verify; worker milestone commits/archive",
            );
        }
        Ok(())
    }

    fn run_interactive(&self, repo: &Path, launcher: &ClaudeLauncher) -> Result<()> {
        let initial_prompt = (!self.cli.request.is_empty()).then_some(self.cli.request.as_str());
        let command = build_interactive_claude_command(
            repo,
            launcher,
            &self.cli.permission_mode,
            &self.cli.interactive_args,
            initial_prompt,
        );
        self.ui
            .debug(&format!("interactive command: {}", command.display()));
        if self.cli.dry_run {
            self.ui
                .warn("DRY RUN — interactive Claude will not be launched");
            self.ui.info(&command.display());
            return Ok(());
        }
        ProcessRunner::new(&self.ui).run_interactive(&command)
    }

    fn run_interactive_opencode(&self, repo: &Path, launcher: &OpenCodeLauncher) -> Result<()> {
        let initial_prompt = (!self.cli.request.is_empty()).then_some(self.cli.request.as_str());
        let command = build_interactive_opencode_command(
            repo,
            launcher,
            self.cli.permission_mode == "auto",
            &self.cli.interactive_args,
            initial_prompt,
        );
        self.ui
            .debug(&format!("interactive command: {}", command.display()));
        if self.cli.dry_run {
            self.ui
                .warn("DRY RUN — interactive OpenCode will not be launched");
            self.ui.info(&command.display());
            return Ok(());
        }
        ProcessRunner::new(&self.ui).run_interactive(&command)
    }

    fn run_interactive_codex(&self, repo: &Path, launcher: &CodexLauncher) -> Result<()> {
        let initial_prompt = (!self.cli.request.is_empty()).then_some(self.cli.request.as_str());
        let command = build_interactive_codex_command(
            repo,
            launcher,
            &self.cli.permission_mode,
            &self.cli.interactive_args,
            initial_prompt,
        )?;
        self.ui
            .debug(&format!("interactive command: {}", command.display()));
        if self.cli.dry_run {
            self.ui
                .warn("DRY RUN — interactive Codex will not be launched");
            self.ui.info(&command.display());
            return Ok(());
        }
        ProcessRunner::new(&self.ui).run_interactive(&command)
    }

    fn run_connection_test(&self, repo: &Path, launcher: &ClaudeLauncher) -> Result<()> {
        let connection = connection_description(&AgentLauncher::Claude(launcher.clone()));
        let mode = if self.cli.basic_connection_test {
            ConnectionTestMode::Basic
        } else {
            ConnectionTestMode::Agentic
        };
        let command =
            build_connection_test_command(repo, launcher, &self.cli.permission_mode, mode);
        self.ui
            .info(&format!("Testing Claude connection {connection} ({mode})"));
        self.ui
            .debug(&format!("connection test command: {}", command.display()));
        if self.cli.dry_run {
            self.ui
                .warn("DRY RUN — the connection test will not contact the model");
            self.ui.info(&command.display());
            return Ok(());
        }

        let output = ProcessRunner::new(&self.ui).run(&command, "Waiting for model response")?;
        let response = parse_connection_test_output(&output, mode)?;
        if response == CONNECTION_TEST_MARKER {
            self.ui
                .success(&format!("Connection {connection} responded successfully"));
        } else {
            bail!(
                "Claude transport succeeded, but response compatibility failed: expected exactly `{CONNECTION_TEST_MARKER}`, received {response:?}"
            );
        }
        if self.cli.verbose {
            self.ui.info(&format!("Model response: {response}"));
        }
        Ok(())
    }

    fn run_opencode_connection_test(&self, repo: &Path, launcher: &OpenCodeLauncher) -> Result<()> {
        let connection = connection_description(&AgentLauncher::OpenCode(launcher.clone()));
        let mode = if self.cli.basic_connection_test {
            ConnectionTestMode::Basic
        } else {
            ConnectionTestMode::Agentic
        };
        let command = build_opencode_connection_test_command(repo, launcher, mode);
        self.ui.info(&format!(
            "Testing OpenCode connection {connection} ({mode})"
        ));
        self.ui
            .debug(&format!("connection test command: {}", command.display()));
        if self.cli.dry_run {
            self.ui
                .warn("DRY RUN — the connection test will not contact the model");
            self.ui.info(&command.display());
            return Ok(());
        }

        let output = ProcessRunner::new(&self.ui).run(&command, "Waiting for model response")?;
        let response = parse_opencode_connection_test_output(&output, mode)?;
        if response == CONNECTION_TEST_MARKER {
            self.ui
                .success(&format!("Connection {connection} responded successfully"));
        } else {
            bail!(
                "OpenCode transport succeeded, but response compatibility failed: expected exactly `{CONNECTION_TEST_MARKER}`, received {response:?}"
            );
        }
        if self.cli.verbose {
            self.ui.info(&format!("Model response: {response}"));
        }
        Ok(())
    }

    fn run_codex_connection_test(&self, repo: &Path, launcher: &CodexLauncher) -> Result<()> {
        let connection = connection_description(&AgentLauncher::Codex(launcher.clone()));
        let prompt = if self.cli.basic_connection_test {
            format!(
                "Connectivity test only. Do not inspect files or perform any other work. Return the required structured output with opsx_status READY and summary exactly {CONNECTION_TEST_MARKER}."
            )
        } else {
            format!(
                "Codex provider compatibility test. Invoke the shell tool exactly once with the command `printf 'OPSX_TOOL_ROUNDTRIP_OK\\n'`. After receiving the tool result, return the required structured output with opsx_status READY and summary exactly {CONNECTION_TEST_MARKER}. Do not inspect files or perform any other work."
            )
        };
        self.ui
            .info(&format!("Testing Codex connection {connection}"));
        if self.cli.dry_run {
            self.ui
                .warn("DRY RUN — the connection test will not contact the model");
            self.ui.info(&launcher.server_command(repo).display());
            return Ok(());
        }
        let backend = CodexBackend::new(
            repo,
            launcher,
            &self.cli.permission_mode,
            self.cli.stream_claude,
            &self.ui,
        )
        .without_session_persistence();
        let result = backend.invoke(
            SessionMode::New {
                id: SessionId::new(Uuid::new_v4().to_string()),
                name: Some("opsx-build-connection-test".to_owned()),
            },
            &prompt,
            "Waiting for model response",
            StageProtocol::Ready,
        )?;
        if result.text.trim() != CONNECTION_TEST_MARKER {
            bail!(
                "Codex transport succeeded, but response compatibility failed: expected exactly `{CONNECTION_TEST_MARKER}`, received {:?}",
                result.text.trim()
            );
        }
        self.ui
            .success(&format!("Connection {connection} responded successfully"));
        Ok(())
    }

    fn debug_configuration(&self, repo: &Path, launcher: &AgentLauncher) {
        self.ui
            .debug("complete prompts are visible and may contain repository content");
        self.ui.debug(&format!("repository: {}", repo.display()));
        match launcher {
            AgentLauncher::Claude(launcher) => self.ui.debug(&format!(
                "Claude launcher: connection={:?}, shared environment={:?}, program=`{}`, prefix args={:?}, model={:?}, context window={:?}, auto-compact window={:?}, auto-compact percent={:?}, max output tokens={:?}, max provider retries={}, environment variables={:?}, unset environment={:?}, permission mode=`{}`, stream filter={:?}",
                launcher.connection_name,
                launcher.environment_name,
                launcher.program,
                launcher.prefix_args,
                launcher.model,
                launcher.context_window,
                launcher.auto_compact_window,
                launcher.auto_compact_percent,
                launcher.max_output_tokens,
                self.cli.max_provider_retries,
                launcher
                    .environment
                    .iter()
                    .map(|variable| variable.name.as_str())
                    .collect::<Vec<_>>(),
                launcher.unset_environment,
                self.cli.permission_mode,
                self.cli.stream_claude
            )),
            AgentLauncher::OpenCode(launcher) => self.ui.debug(&format!(
                "OpenCode launcher: connection={:?}, shared environment={:?}, program=`{}`, prefix args={:?}, model={:?}, stream filter={:?}",
                launcher.connection_name,
                launcher.environment_name,
                launcher.program,
                launcher.prefix_args,
                launcher.model,
                self.cli.stream_claude
            )),
            AgentLauncher::Codex(launcher) => self.ui.debug(&format!(
                "Codex launcher: connection={:?}, shared environment={:?}, program=`{}`, prefix args={:?}, model={:?}, permission profile={:?}, context window={:?}, auto-compact window={:?}, auto-compact percent={:?}, max output tokens={:?}, permission mode=`{}`, stream filter={:?}",
                launcher.connection_name,
                launcher.environment_name,
                launcher.program,
                launcher.prefix_args,
                launcher.model,
                launcher.permission_profile,
                launcher.context_window,
                launcher.auto_compact_window,
                launcher.auto_compact_percent,
                launcher.max_output_tokens,
                self.cli.permission_mode,
                self.cli.stream_claude
            )),
        }
        self.ui.debug(&format!(
            "resolved config file: {}",
            self.cli
                .config_path
                .as_ref()
                .map_or_else(|| "none".to_owned(), |path| path.display().to_string())
        ));
        self.ui.debug(&format!(
            "campaign loop: {}, max iterations: {:?}",
            self.cli.loop_workflow, self.cli.max_iterations
        ));
    }
}

fn build_agent_dry_run_command(
    repo: &Path,
    launcher: &AgentLauncher,
    permission_mode: &str,
    session: &SessionMode,
    prompt: &str,
    protocol: StageProtocol,
) -> CommandSpec {
    match launcher {
        AgentLauncher::Claude(launcher) => build_claude_command(
            repo,
            launcher,
            permission_mode,
            session,
            prompt,
            ClaudeOutputFormat::Json,
            Some(protocol.json_schema()),
        ),
        AgentLauncher::OpenCode(launcher) => {
            build_opencode_command(repo, launcher, session, prompt)
        }
        AgentLauncher::Codex(launcher) => launcher.server_command(repo),
    }
}

fn connection_description(launcher: &AgentLauncher) -> String {
    let name = launcher.connection_name().unwrap_or("default");
    let model = launcher.model().unwrap_or("harness default model");
    let backend = launcher.backend_name();
    launcher.environment_name().map_or_else(
        || format!("`{name}` ({backend}, {model})"),
        |environment| format!("`{name}` ({backend}, {model}, environment `{environment}`)"),
    )
}

fn model_stage_title(title: &str, role: &str, launcher: &AgentLauncher) -> String {
    let connection = launcher.connection_name().unwrap_or("default");
    format!(
        "{title} · {role} ({connection}, {})",
        launcher.backend_name()
    )
}

fn ensure_launcher_prerequisites<U: Ui>(
    launcher: &AgentLauncher,
    repo: &Path,
    ui: &U,
) -> Result<()> {
    match launcher {
        AgentLauncher::Claude(_) => {
            if launcher.program() != "claude" {
                prerequisite_exists(launcher.program(), repo, ui)?;
            }
            prerequisite_exists("claude", repo, ui)
        }
        AgentLauncher::OpenCode(_) => prerequisite_exists(launcher.program(), repo, ui),
        AgentLauncher::Codex(_) => prerequisite_exists(launcher.program(), repo, ui),
    }
}

fn stage_uses_frontier_model(stage: Stage, bootstrap: bool, terminal_frontier: bool) -> bool {
    bootstrap
        || (terminal_frontier
            && matches!(
                stage,
                Stage::Propose | Stage::Apply | Stage::Verify | Stage::Repair
            ))
}

fn planning_session(state: &mut RunState, backend: &dyn AgentBackend) -> Result<SessionMode> {
    let saved_backend = state
        .planning_backend
        .as_deref()
        .or_else(|| state.planning_session.as_ref().map(|_| "Claude"));
    let requested = match (
        &state.planning_session,
        saved_backend == Some(backend.name()),
    ) {
        (Some(id), true) => SessionMode::Resume { id: id.clone() },
        _ => SessionMode::New {
            id: SessionId::new(Uuid::new_v4().to_string()),
            name: Some("opsx-build-planning".to_owned()),
        },
    };
    let session = backend.prepare_session(requested)?;
    state.planning_session = Some(session.id().clone());
    state.planning_backend = Some(backend.name().to_owned());
    Ok(session)
}

fn invoke_fresh(
    backend: &dyn AgentBackend,
    name: &str,
    prompt: &str,
    activity: &str,
    protocol: StageProtocol,
) -> Result<StageResult> {
    backend.invoke(
        SessionMode::New {
            id: SessionId::new(Uuid::new_v4().to_string()),
            name: Some(name.to_owned()),
        },
        prompt,
        activity,
        protocol,
    )
}

fn require_ready(stage: &str, response: &str, signal: StageSignal) -> Result<()> {
    match signal {
        StageSignal::Ready => Ok(()),
        StageSignal::Blocked => blocked(stage, response),
        other => bail!("{stage} returned unexpected terminal status {other:?}"),
    }
}

fn handle_worker_result<U: Ui>(
    repo: &Path,
    state: &mut RunState,
    stage: &str,
    response: &str,
    signal: StageSignal,
    ui: &U,
) -> Result<bool> {
    match signal {
        StageSignal::Ready => Ok(true),
        StageSignal::TooLarge => {
            record_too_large(repo, state, response.to_owned(), ui)?;
            Ok(false)
        }
        StageSignal::Blocked => blocked(stage, response),
        other => bail!("{stage} returned unexpected terminal status {other:?}"),
    }
}

fn record_too_large<U: Ui>(
    repo: &Path,
    state: &mut RunState,
    summary: String,
    ui: &U,
) -> Result<()> {
    state.too_large = Some(TooLargeOutcome {
        stage: state.stage,
        summary,
    });
    persist_state(repo, state, ui)
}

fn worker_result<U: Ui>(
    result: Result<StageResult>,
    repo: &Path,
    state: &mut RunState,
    ui: &U,
) -> Result<Option<StageResult>> {
    match result {
        Ok(result) => Ok(Some(result)),
        Err(error) => {
            let Some(escalation) = error.downcast_ref::<WorkerEscalationRequested>() else {
                return Err(error);
            };
            record_too_large(repo, state, escalation.reason().to_owned(), ui)?;
            Ok(None)
        }
    }
}

fn arm_slice_baseline<U: Ui>(repo: &Path, state: &mut RunState, ui: &U) -> Result<()> {
    if state.local_only || matches!(state.stage, Stage::Complete | Stage::Done) {
        state.slice_baseline = None;
        return Ok(());
    }
    let product_repo = product_repository(repo, state).to_path_buf();
    let baseline = capture_slice_baseline(&product_repo, ui)?;
    state.slice_baseline = Some(baseline);
    Ok(())
}

fn release_state_baseline<U: Ui>(repo: &Path, state: &mut RunState, ui: &U) {
    let Some(baseline) = state.slice_baseline.take() else {
        return;
    };
    let product_repo = product_repository(repo, state).to_path_buf();
    if let Err(error) = release_slice_baseline(&product_repo, &baseline, ui) {
        ui.warn(&format!(
            "Could not release private rollback metadata; workflow state is unaffected: {error}"
        ));
    }
}

fn product_repository<'a>(planning_repo: &'a Path, state: &'a RunState) -> &'a Path {
    state.product_repo.as_deref().unwrap_or(planning_repo)
}

fn is_terminal_gate(state: &RunState) -> bool {
    state.agenda.as_ref().is_some_and(is_terminal_assignment)
}

fn needs_terminal_review(state: &RunState) -> bool {
    is_terminal_gate(state)
        && state.stage == Stage::Propose
        && !state.terminal_review_complete
        && state.too_large.is_none()
}

fn uses_terminal_frontier(state: &RunState, frontier_available: bool) -> bool {
    is_terminal_gate(state) && !state.local_only && frontier_available
}

fn proposal_postcondition_repair(subject: &str, failure: &str) -> String {
    format!(
        "{subject}\n\nPOSTCONDITION REPAIR: {failure} Continue in this same planning session and perform the proposal workflow now; do not merely describe what should be proposed and do not create a duplicate change. Inspect active OpenSpec changes first and continue the intended existing scaffold when one is present. Follow any exact agenda assignment above; otherwise use the completed exploration and repository's durable planning evidence. Run all commands synchronously and wait for every command and subagent to finish. Return READY only after `openspec status --change <name> --json` reports `isPlanningComplete: true`. If no change is warranted, return DONE. If the selected slice exceeds one reliable worker change, return TOO_LARGE with an ordered decomposition. Return BLOCKED only for a genuine external decision."
    )
}

fn campaign_subject(state: &RunState) -> String {
    if state.bootstrap {
        return format!(
            "Bootstrap this repository by carrying out the exact planning assignment below. Create or continue only the OpenSpec change `{BOOTSTRAP_CHANGE}`. This change decomposes the complete project goal into a durable agenda for later worker-model runs; it does not implement product functionality. Treat `openspec/config.yaml` as the project authority and do not survey the product source tree.\n\nAssigned bootstrap file: `{BOOTSTRAP_PATH}`\n\n--- BEGIN BOOTSTRAP ASSIGNMENT ---\n{}\n--- END BOOTSTRAP ASSIGNMENT ---",
            bootstrap_instructions().trim()
        );
    }
    if let Some(assignment) = &state.agenda {
        let iteration = state
            .campaign
            .as_ref()
            .map(|campaign| format!("campaign iteration {}", campaign.iteration))
            .unwrap_or_else(|| "an advance operation".to_owned());
        return format!(
            "Create the OpenSpec planning artifacts for the exact ordered agenda assignment below. This is {iteration}. This Propose stage is planning-only: do not implement production code or invoke an Apply or implementation skill. Do not select, create, or modify a different slice. Create or continue the OpenSpec change with the exact name `{change}`. The agenda content is authoritative; use repository inspection only to elaborate its implementation details.\n\nAssigned agenda file: `{path}`\n\n--- BEGIN ASSIGNED AGENDA SLICE ---\n{content}\n--- END ASSIGNED AGENDA SLICE ---",
            change = assignment.change,
            path = assignment.path,
            content = assignment.content.trim()
        );
    }
    if let (Some(product_repo), Some(change)) = (state.product_repo.as_deref(), &state.change) {
        return format!(
            "{}\n\nCreate the complete OpenSpec planning artifacts for the exact bounded brownfield change `{change}`. The current working directory is the external planning repository; the product repository is `{}` and is available as an additional Claude directory. Treat `openspec/config.yaml` as durable supplied context. Inspect only the product files, symbols, references, and tests needed for this change; do not attempt to understand or survey the entire product repository. During Propose, write only planning artifacts and do not implement product code.",
            state.request,
            product_repo.display()
        );
    }
    if let Some(change) = &state.change {
        return format!(
            "{}\n\nUse the exact OpenSpec change name `{change}`. During exploration, investigate without creating artifacts. During proposal, create or continue only `{change}` and do not select or modify a differently named change.",
            state.request
        );
    }
    match &state.campaign {
        Some(campaign) => format!(
            "{}\n\nThis is campaign iteration {}. Select and pursue exactly one coherent, bounded remaining slice toward the objective. Use the repository and archived OpenSpec history as durable evidence of earlier iterations. If the overall objective is already satisfied and no meaningful slice remains, carry that conclusion into Propose so it can return DONE without creating artifacts.",
            state.request, campaign.iteration
        ),
        None => state.request.clone(),
    }
}

fn with_sidecar_context(state: &RunState, subject: &str) -> String {
    let Some(product_repo) = state.product_repo.as_deref() else {
        return subject.to_owned();
    };
    format!(
        "{subject}\n\nSIDECAR WORKSPACE: The current working directory contains OpenSpec planning state only. The product source and its Git repository are at `{}` and are available through Claude's additional-directory access. Read the product's existing `CLAUDE.md` when present. Make product code/test/documentation edits only there, run product commands from there, and use targeted language-server navigation and searches rather than surveying the whole repository. Keep OpenSpec artifacts and sidecar-local workflow files in the current planning repository.",
        product_repo.display()
    )
}

fn frontier_replan_prompt(assignment: &AgendaAssignment, outcome: &TooLargeOutcome) -> String {
    format!(
        "The local worker could not reliably complete the ordered agenda slice below. The orchestrator has discarded that failed attempt and restored the exact pre-Propose repository state. Replan the agenda using your stronger planning judgement; do not implement the slice and do not create or modify any OpenSpec change.\n\nOriginal agenda file: `{path}`\nOriginal OpenSpec change name: `{change}`\nLocal failure stage: {stage}\nLocal failure report:\n{failure}\n\nRewrite the original agenda file so it describes only the first independently implementable and verifiable subset. Keep its current filename and therefore its current OpenSpec change name. Add the remaining work as immediately following child slices whose numeric ordinal appends `.1`, `.2`, and so on to the original ordinal. For example, `0009-feature.md` may be followed by `0009.1-next-part.md` and `0009.2-final-part.md`; those filenames map to OpenSpec change names `0009-1-next-part` and `0009-2-final-part`. Child slices may later be subdivided recursively in the same way. Choose boundaries that a local coding model can complete reliably, not merely conceptual chapter boundaries. Preserve dependencies and acceptance criteria so the sequence still delivers the original objective.\n\nModify only files under `automation/slices/`. Update an agenda index there if one exists and needs updating. Do not modify source code, tests, OpenSpec artifacts, CLAUDE.md, or other project files. Inspect repository evidence only as needed to choose sound boundaries; do not perform an exhaustive repository survey.\n\nCommit the agenda-only replan with commit message exactly `opsx: subdivide {change}`. Preserve every pre-existing working-tree change exactly. Do not reset, stash, restore, discard, amend, or rewrite existing history. Return REPLANNED only after the subdivision is committed, the original slice is materially narrower, at least one child slice exists, and no uncommitted changes from your work remain. Return BLOCKED only if the agenda cannot be subdivided safely from available evidence.\n\n--- BEGIN ORIGINAL AGENDA SLICE ---\n{content}\n--- END ORIGINAL AGENDA SLICE ---",
        path = assignment.path,
        change = assignment.change,
        stage = outcome.stage.title(),
        failure = outcome.summary.trim(),
        content = assignment.content.trim(),
    )
}

fn terminal_review_prompt(
    assignment: &AgendaAssignment,
    escalation: Option<&TooLargeOutcome>,
) -> String {
    let failure = escalation.map_or_else(String::new, |outcome| {
        format!(
            "\n\nA preceding terminal attempt was abandoned and the repository was restored to its exact pre-Propose state. Remediation is required; READY is not a valid result for this review.\nFailed stage: {}\nFailure report:\n{}",
            outcome.stage.title(),
            outcome.summary.trim()
        )
    });
    format!(
        "Act as the frontier architect's independent whole-project acceptance reviewer before the terminal agenda slice. Re-read the complete goal in `openspec/config.yaml`, the ordered agenda and its README, the unchanged terminal gate `{path}`, canonical and archived OpenSpec evidence, existing implementation, and real tests. Use targeted inspection and run relevant existing end-to-end checks where practical. Determine whether every material project goal is implemented and the repository is ready for the terminal acceptance change. Do not create or modify any OpenSpec change and do not implement product code.{failure}\n\nIf the project is ready, leave the repository, index, working tree, and Git history exactly unchanged and return READY.\n\nIf material functionality or validation is missing, preserve `{path}` byte-for-byte as the terminal gate. Add one or more bounded, independently implementable remediation slice files under `automation/slices/` whose numeric ordinals sort after every existing nonterminal slice and before `9999`. Update the agenda README so the new execution order and project-goal coverage remain accurate. Each remediation slice must satisfy the existing agenda contract and be small enough for the worker model. Modify only files under `automation/slices/`; do not modify source, tests, OpenSpec state, CLAUDE.md, or any other path. Commit the agenda-only remediation with subject exactly `opsx: add acceptance remediation`. Preserve pre-existing user work and never reset, stash, restore, discard, amend, or rewrite history. Return REPLANNED only after the remediation agenda is committed and the working state outside the permitted agenda paths is unchanged.\n\nReturn BLOCKED only when this review or safe remediation genuinely requires a human decision or unavailable external input.\n\n--- BEGIN TERMINAL ACCEPTANCE SLICE ---\n{content}\n--- END TERMINAL ACCEPTANCE SLICE ---",
        path = assignment.path,
        content = assignment.content.trim(),
    )
}

fn terminal_review_ready_postcondition<U: Ui>(
    repo: &Path,
    baseline: &SliceBaseline,
    terminal: &AgendaAssignment,
    before_changes: &ChangeSnapshot,
    ui: &U,
) -> Result<()> {
    if current_head(repo, ui)?.as_deref() != Some(baseline.head.as_str()) {
        bail!("frontier returned READY after changing Git history");
    }
    if !baseline_matches_except(repo, baseline, None, ui)? {
        bail!("frontier returned READY after changing repository state");
    }
    if openspec_snapshot(repo, ui)? != *before_changes {
        bail!("frontier returned READY after changing OpenSpec state");
    }
    if fs::read_to_string(repo.join(&terminal.path))? != terminal.content {
        bail!("frontier returned READY after changing the terminal acceptance slice");
    }
    Ok(())
}

fn terminal_remediation_postcondition<U: Ui>(
    repo: &Path,
    baseline: &SliceBaseline,
    terminal: &AgendaAssignment,
    before_changes: &ChangeSnapshot,
    ui: &U,
) -> Result<AgendaAssignment> {
    let head = current_head(repo, ui)?.context("frontier remediation did not leave a Git HEAD")?;
    if head == baseline.head {
        bail!("frontier remediation did not create a commit");
    }
    if !head_descends_from(repo, &baseline.head, ui)? {
        bail!("frontier remediation rewrote or replaced pre-9999 Git history");
    }

    let committed = committed_paths_since(repo, &baseline.head, ui)?;
    if committed.is_empty() {
        bail!("frontier remediation commit changed no files");
    }
    let outside_agenda = committed
        .iter()
        .filter(|path| !path.starts_with("automation/slices/"))
        .cloned()
        .collect::<Vec<_>>();
    if !outside_agenda.is_empty() {
        bail!(
            "frontier remediation committed files outside `automation/slices/`: {}",
            outside_agenda.join(", ")
        );
    }
    if !baseline_matches_except(repo, baseline, Some("automation/slices/"), ui)? {
        bail!(
            "frontier remediation did not preserve pre-existing state outside `automation/slices/` exactly"
        );
    }
    if openspec_snapshot(repo, ui)? != *before_changes {
        bail!("frontier remediation changed OpenSpec state");
    }
    if fs::read_to_string(repo.join(&terminal.path))? != terminal.content {
        bail!("frontier remediation modified the terminal 9999 acceptance slice");
    }
    let AgendaSelection::Next(remediation) = discover_agenda(repo, before_changes)? else {
        bail!("frontier remediation left no next ordered agenda slice");
    };
    validate_terminal_remediation_assignment(&remediation, &committed)?;
    if !repo.join("automation/slices/README.md").is_file() {
        bail!("frontier remediation removed the agenda README");
    }
    Ok(remediation)
}

fn validate_terminal_remediation_assignment(
    remediation: &AgendaAssignment,
    committed: &[String],
) -> Result<()> {
    if is_terminal_assignment(remediation) {
        bail!("frontier remediation inserted no executable slice before 9999");
    }
    if !committed.iter().any(|path| path == &remediation.path) {
        bail!(
            "frontier remediation did not commit the newly selected slice `{}`",
            remediation.path
        );
    }
    for heading in [
        "# ",
        "## Objective",
        "## Prerequisites",
        "## Acceptance Criteria",
        "## Required Tests",
    ] {
        if !remediation
            .content
            .lines()
            .any(|line| line.starts_with(heading))
        {
            bail!(
                "frontier remediation slice `{}` omitted required heading `{heading}`",
                remediation.path
            );
        }
    }
    Ok(())
}

fn frontier_replan_postcondition<U: Ui>(
    repo: &Path,
    baseline: &SliceBaseline,
    original: &AgendaAssignment,
    before_changes: &ChangeSnapshot,
    ui: &U,
) -> Result<AgendaAssignment> {
    let head = current_head(repo, ui)?.context("frontier replan did not leave a Git HEAD")?;
    if head == baseline.head {
        bail!("frontier replan did not create a commit");
    }
    if !head_descends_from(repo, &baseline.head, ui)? {
        bail!("frontier replan rewrote or replaced the pre-Propose Git history");
    }

    let committed = committed_paths_since(repo, &baseline.head, ui)?;
    if committed.is_empty() {
        bail!("frontier replan commit changed no files");
    }
    let outside_agenda = committed
        .iter()
        .filter(|path| !path.starts_with("automation/slices/"))
        .cloned()
        .collect::<Vec<_>>();
    if !outside_agenda.is_empty() {
        bail!(
            "frontier replan committed files outside `automation/slices/`: {}",
            outside_agenda.join(", ")
        );
    }

    if !baseline_matches_except(repo, baseline, Some("automation/slices/"), ui)? {
        bail!(
            "frontier replan did not preserve the pre-existing state outside `automation/slices/` exactly"
        );
    }
    if openspec_snapshot(repo, ui)? != *before_changes {
        bail!("frontier replan changed OpenSpec state");
    }

    let AgendaSelection::Next(refreshed) = discover_agenda(repo, before_changes)? else {
        bail!("frontier replan left no next ordered agenda slice");
    };
    if refreshed.path != original.path || refreshed.change != original.change {
        bail!(
            "frontier replan replaced the assigned first slice instead of preserving `{}`",
            original.path
        );
    }
    if refreshed.content == original.content {
        bail!("frontier replan did not narrow the original agenda slice");
    }
    if !has_subdivision(repo, original)? {
        bail!("frontier replan did not add a hierarchical child slice");
    }
    Ok(refreshed)
}

fn blocked<T>(stage: &str, response: &str) -> Result<T> {
    bail!("BLOCKED during {stage}:\n{}", response.trim())
}

fn with_direction(base: &str, direction: Option<&str>) -> String {
    match direction {
        Some(direction) => format!(
            "{base}\n\nUser direction for this iteration:\n{direction}\n\nTreat this as implementation guidance. Do not change the approved OpenSpec requirements."
        ),
        None => base.to_owned(),
    }
}

fn archive_subject(change: &str) -> String {
    format!(
        "{change}\n\nArchive this successfully verified OpenSpec change, including normal specification synchronization. The user has already authorized the normal archive choices: if delta specs need synchronization, choose `Sync now (recommended)`, verify the sync, and continue the archive; if they are already synchronized, choose `Archive now`. Do not stop to request routine confirmation for either choice, and do not treat that confirmation as a BLOCKED condition. Report BLOCKED only if synchronization, verification, or archival cannot safely be completed without a genuine human decision or unavailable external input."
    )
}

fn proposal_commit_message(change: &str) -> String {
    format!(
        "Use commit subject exactly `openspec: propose {change}`. Add a conventional Git commit body derived from the completed OpenSpec proposal, specs, design, and tasks. Use imperative mood. Write a short first paragraph explaining the intended observable change and why it is being made. Add at most one second paragraph for an important scope boundary or acceptance condition. Separate paragraphs with a blank line and hard-wrap every body line at 72 columns or fewer. Prefer roughly four to eight body lines in total. Do not write one dense summary paragraph, Markdown headings or lists, file or task inventories, command invocations, generated boilerplate, or behavior not approved by those artifacts. Check the subject and body formatting before creating each commit. Once all relevant work is committed, message formatting alone is not a BLOCKED condition: preserve the existing commits, mention any formatting imperfection briefly, and report READY without requesting permission to amend or creating a replacement commit."
    )
}

fn completion_commit_message(change: &str) -> String {
    format!(
        "Use commit subject exactly `openspec: complete {change}`. Add a conventional Git commit body derived from the completed OpenSpec artifacts and actual verification results. Use imperative mood. Write a short first paragraph explaining the delivered observable behavior and its purpose. Add at most one second paragraph summarizing the most important validation that actually ran or a material scope boundary. Separate paragraphs with a blank line and hard-wrap every body line at 72 columns or fewer. Prefer roughly four to eight body lines in total. Do not write one dense summary paragraph, Markdown headings or lists, file or task inventories, exhaustive implementation mechanics, generated boilerplate, or claims about checks that did not run. Check the subject and body formatting before creating each commit. Once all relevant work is committed, message formatting alone is not a BLOCKED condition: preserve the existing commits, mention any formatting imperfection briefly, and report READY without requesting permission to amend or creating a replacement commit."
    )
}

fn queue_direction(state: &mut RunState, direction: &str) {
    state.pending_direction = Some(direction.to_owned());
    if matches!(
        state.stage,
        Stage::Verify | Stage::Archive | Stage::FinalCommit
    ) {
        state.stage = Stage::Repair;
        state.pending_repair = None;
    }
}

fn require_change(state: &RunState) -> Result<String> {
    state
        .change
        .clone()
        .context("workflow state has no OpenSpec change name")
}

fn verify_milestone_ancestry<U: Ui>(
    repo: &Path,
    baseline: Option<&str>,
    milestone: &str,
    ui: &U,
) -> Result<()> {
    let Some(baseline) = baseline else {
        return Ok(());
    };
    if head_descends_from(repo, baseline, ui)? {
        return Ok(());
    }
    bail!(
        "the {milestone} milestone replaced previously committed Git history: starting commit `{baseline}` is no longer an ancestor of HEAD. The repository has been left untouched for recovery through Git's reflog"
    )
}

fn archive_is_absent(changes: &ChangeSnapshot, change: &str) -> bool {
    !changes.changes.contains_key(change)
}

fn validate_change_name(change: &str) -> Result<()> {
    let valid = !change.is_empty()
        && change
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !change.starts_with('-')
        && !change.ends_with('-');
    if valid {
        Ok(())
    } else {
        bail!("OpenSpec returned unsafe change name `{change}`; expected lowercase kebab-case")
    }
}

fn state_path<U: Ui>(repo: &Path, ui: &U) -> Result<PathBuf> {
    Ok(metadata_dir(repo, ui)?.join("last-run.json"))
}

fn persist_state<U: Ui>(repo: &Path, state: &RunState, ui: &U) -> Result<()> {
    let path = state_path(repo, ui)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(state)?)
        .with_context(|| format!("could not write checkpoint `{}`", temporary.display()))?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("could not replace checkpoint `{}`", path.display()))
}

fn try_load_state<U: Ui>(repo: &Path, ui: &U) -> Result<Option<RunState>> {
    let path = state_path(repo, ui)?;
    if path.exists() {
        return load_state_file(&path).map(Some);
    }
    let legacy_path = legacy_metadata_dir(repo, ui)?.join("last-run.json");
    if legacy_path.exists() {
        return load_state_file(&legacy_path).map(Some);
    }
    Ok(None)
}

fn load_state<U: Ui>(repo: &Path, ui: &U) -> Result<RunState> {
    try_load_state(repo, ui)?.context("no opsx-build checkpoint exists; start a new run")
}

fn load_state_file(path: &Path) -> Result<RunState> {
    let bytes = fs::read(path)
        .with_context(|| format!("could not read checkpoint `{}`", path.display()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid checkpoint `{}`", path.display()))?;
    let schema = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    match schema {
        STATE_SCHEMA_VERSION => serde_json::from_value(value).context("invalid current checkpoint"),
        4 => migrate_v4(value),
        3 => migrate_v3(value),
        2 => migrate_v2(value),
        other => bail!(
            "unsupported checkpoint schema {other}; use `--forget` to leave repository state untouched and start over"
        ),
    }
}

fn migrate_v4(mut value: Value) -> Result<RunState> {
    value["schema_version"] = Value::from(STATE_SCHEMA_VERSION);
    serde_json::from_value(value).context("invalid version 4 checkpoint")
}

fn migrate_v3(mut value: Value) -> Result<RunState> {
    value["schema_version"] = Value::from(STATE_SCHEMA_VERSION);
    value["agenda"] = Value::Null;
    serde_json::from_value(value).context("invalid version 3 checkpoint")
}

fn migrate_v2(value: Value) -> Result<RunState> {
    let stage_value = value
        .get("stage")
        .cloned()
        .context("old checkpoint omitted stage")?;
    if stage_value.as_str() == Some("blocked") {
        bail!("old checkpoint is marked blocked; use `--forget` after reviewing the repository")
    }
    let stage: Stage =
        serde_json::from_value(stage_value).context("invalid old checkpoint stage")?;
    let planning_session = value
        .get("sessions")
        .and_then(Value::as_array)
        .and_then(|sessions| {
            sessions.iter().rev().find(|session| {
                session.get("stage").and_then(Value::as_str) == Some("explore+propose")
            })
        })
        .and_then(|session| session.get("id"))
        .and_then(Value::as_str)
        .map(SessionId::new);

    Ok(RunState {
        schema_version: STATE_SCHEMA_VERSION,
        request: value
            .get("request")
            .and_then(Value::as_str)
            .context("old checkpoint omitted request")?
            .to_owned(),
        change: value
            .get("change")
            .and_then(Value::as_str)
            .map(str::to_owned),
        stage,
        verify_retries: value
            .get("verify_retries")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        before_changes: value
            .get("before_changes")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default(),
        planning_backend: planning_session.as_ref().map(|_| "Claude".to_owned()),
        planning_session,
        pending_repair: value
            .get("pending_repair")
            .and_then(Value::as_str)
            .map(str::to_owned),
        pending_direction: None,
        proposal_head: value
            .get("proposal_commit")
            .and_then(Value::as_str)
            .map(str::to_owned),
        final_head: value
            .get("final_commit")
            .and_then(Value::as_str)
            .map(str::to_owned),
        terminal_summary: None,
        too_large: None,
        campaign: None,
        agenda: None,
        slice_baseline: None,
        frontier_replans: 0,
        terminal_review_complete: false,
        terminal_remediations: 0,
        bootstrap: false,
        product_repo: None,
        sidecar_store: None,
        planning_final_head: None,
        local_only: false,
    })
}

fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

fn campaign_limit_resume_command(repo: &Path, current_limit: u32) -> String {
    let next_limit = current_limit
        .saturating_mul(2)
        .max(current_limit.saturating_add(1));
    format!(
        "opsx-build --repo {} --resume --loop --max-iterations {next_limit}",
        shell_quote(&repo.to_string_lossy())
    )
}

fn single_advance_continuation_command(repo: &Path, state: &RunState) -> Option<String> {
    if state.request != "advance" || state.campaign.is_some() {
        return None;
    }
    matches!(
        discover_agenda(repo, &ChangeSnapshot::default()),
        Ok(AgendaSelection::Next(_))
    )
    .then(|| {
        format!(
            "opsx-build --repo {} --loop advance",
            shell_quote(&repo.to_string_lossy())
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PlanningBackend {
        name: &'static str,
        actual_session: Option<&'static str>,
    }

    impl AgentBackend for PlanningBackend {
        fn name(&self) -> &'static str {
            self.name
        }

        fn prepare_session(&self, session: SessionMode) -> Result<SessionMode> {
            Ok(match self.actual_session {
                Some(id) => SessionMode::Resume {
                    id: SessionId::new(id),
                },
                None => session,
            })
        }

        fn invoke(
            &self,
            _: SessionMode,
            _: &str,
            _: &str,
            _: StageProtocol,
        ) -> Result<StageResult> {
            bail!("interrupted planning turn")
        }

        fn rename_session(&self, _: &SessionId, _: &str) -> Result<()> {
            unreachable!()
        }

        fn compact_session(&self, _: &SessionId, _: &str) -> Result<()> {
            unreachable!()
        }
    }

    #[test]
    fn direction_is_injected_as_iteration_guidance() {
        let prompt = with_direction("apply slice-m", Some("keep the AST unchanged"));
        assert!(prompt.contains("User direction for this iteration"));
        assert!(prompt.contains("keep the AST unchanged"));
    }

    #[test]
    fn campaign_subject_preserves_the_goal_and_bounds_one_iteration() {
        let mut state = RunState::new_campaign(
            "finish the compiler".to_owned(),
            ChangeSnapshot::default(),
            Some(20),
        );
        state.campaign.as_mut().unwrap().iteration = 7;
        let subject = campaign_subject(&state);
        assert!(subject.contains("finish the compiler"));
        assert!(subject.contains("campaign iteration 7"));
        assert!(subject.contains("exactly one coherent, bounded remaining slice"));
        assert!(subject.contains("return DONE"));
    }

    #[test]
    fn bootstrap_propose_defers_agenda_files_to_apply() {
        assert!(
            BOOTSTRAP_PROPOSAL_CONTEXT.contains("only that change's normal OpenSpec artifacts")
        );
        assert!(
            BOOTSTRAP_PROPOSAL_CONTEXT
                .contains("Do not create or modify `automation/slices/README.md`")
        );
        assert!(
            BOOTSTRAP_PROPOSAL_CONTEXT.contains("Apply stage exclusively owns those deliverables")
        );
        assert!(
            BOOTSTRAP_PROPOSAL_CONTEXT.contains("so a fresh Apply session can materialize them")
        );
    }

    #[test]
    fn bootstrap_apply_self_checks_the_agenda_contract() {
        assert!(BOOTSTRAP_APPLY_CONTEXT.contains("Before reporting READY"));
        assert!(BOOTSTRAP_APPLY_CONTEXT.contains("every structural requirement"));
        assert!(BOOTSTRAP_APPLY_CONTEXT.contains("correct any omission within this Apply stage"));
    }

    #[test]
    fn agenda_subject_assigns_one_exact_change_without_exploration() {
        let mut state = RunState::new("advance".to_owned(), ChangeSnapshot::default());
        state.agenda = Some(AgendaAssignment {
            path: "automation/slices/002-test-harness.md".to_owned(),
            change: "002-test-harness".to_owned(),
            title: "002 - Test harness".to_owned(),
            content: "# 002 - Test harness\n\n## Objective\n\nBuild it.".to_owned(),
        });
        let subject = campaign_subject(&state);
        assert!(subject.contains("exact name `002-test-harness`"));
        assert!(subject.contains("automation/slices/002-test-harness.md"));
        assert!(subject.contains("## Objective"));
        assert!(subject.contains("OpenSpec planning artifacts"));
        assert!(subject.contains("This Propose stage is planning-only"));
        assert!(subject.contains("do not implement production code"));
        assert!(subject.contains("or invoke an Apply or implementation skill"));
        assert!(!subject.contains("by implementing the exact ordered agenda"));
        assert!(subject.contains("Do not select, create, or modify a different slice"));
    }

    #[test]
    fn prescribed_change_subject_forbids_automatic_renaming() {
        let mut state = RunState::new(
            "support remote URI path prefixes".to_owned(),
            ChangeSnapshot::default(),
        );
        state.change = Some("remote-path-prefix".to_owned());

        let subject = campaign_subject(&state);

        assert!(subject.contains("support remote URI path prefixes"));
        assert!(subject.contains("exact OpenSpec change name `remote-path-prefix`"));
        assert!(subject.contains("do not select or modify a differently named change"));
    }

    #[test]
    fn sidecar_subject_keeps_planning_external_and_product_inspection_targeted() {
        let mut state = RunState::new(
            "adjust bounded Ceph recovery behaviour".to_owned(),
            ChangeSnapshot::default(),
        );
        state.change = Some("ceph-recovery-fix".to_owned());
        state.stage = Stage::Propose;
        state.product_repo = Some(PathBuf::from("/src/ceph"));
        state.sidecar_store = Some("opsx-ceph-demo".to_owned());
        state.local_only = true;

        let subject = campaign_subject(&state);
        assert!(subject.contains("exact bounded brownfield change `ceph-recovery-fix`"));
        assert!(subject.contains("external planning repository"));
        assert!(subject.contains("product repository is `/src/ceph`"));
        assert!(subject.contains("do not attempt to understand or survey the entire"));
        assert!(!subject.contains("During exploration"));

        let apply = with_sidecar_context(&state, "apply it");
        assert!(apply.contains("current working directory contains OpenSpec planning state only"));
        assert!(apply.contains("Make product code/test/documentation edits only there"));
        assert!(apply.contains("Keep OpenSpec artifacts"));

        let decoded: RunState =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(decoded.stage, Stage::Propose);
        assert_eq!(decoded.product_repo, Some(PathBuf::from("/src/ceph")));
        assert_eq!(decoded.sidecar_store.as_deref(), Some("opsx-ceph-demo"));
        assert!(decoded.local_only);
    }

    #[test]
    fn campaign_checkpoint_round_trips_completed_iterations() {
        let mut state = RunState::new_campaign(
            "finish the compiler".to_owned(),
            ChangeSnapshot::default(),
            Some(10),
        );
        state
            .campaign
            .as_mut()
            .unwrap()
            .completed
            .push(CompletedIteration {
                iteration: 1,
                change: "slice-a".to_owned(),
                proposal_head: Some("proposal".to_owned()),
                final_head: Some("complete".to_owned()),
                elapsed_seconds: Some(90),
            });
        let decoded: RunState =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        let campaign = decoded.campaign.unwrap();
        assert_eq!(campaign.iteration, 1);
        assert_eq!(campaign.max_iterations, Some(10));
        assert_eq!(campaign.completed[0].change, "slice-a");
        assert_eq!(campaign.completed[0].elapsed_seconds, Some(90));
    }

    #[test]
    fn checkpoint_round_trips_too_large_worker_outcome() {
        let mut state = RunState::new("finish the compiler".to_owned(), ChangeSnapshot::default());
        state.stage = Stage::Apply;
        state.too_large = Some(TooLargeOutcome {
            stage: Stage::Apply,
            summary: "Split parser semantics from backend lowering".to_owned(),
        });

        let decoded: RunState =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(decoded.stage, Stage::Apply);
        assert_eq!(decoded.too_large, state.too_large);
    }

    #[test]
    fn frontier_prompt_preserves_the_parent_and_defines_hierarchical_children() {
        let assignment = AgendaAssignment {
            path: "automation/slices/0009-conditionals.md".to_owned(),
            change: "0009-conditionals".to_owned(),
            title: "0009 - Conditionals".to_owned(),
            content: "# 0009 - Conditionals\n\nImplement all conditionals.".to_owned(),
        };
        let outcome = TooLargeOutcome {
            stage: Stage::Apply,
            summary: "frontend and lowering are too broad together".to_owned(),
        };

        let prompt = frontier_replan_prompt(&assignment, &outcome);

        assert!(prompt.contains("Keep its current filename"));
        assert!(prompt.contains("0009.1-next-part.md"));
        assert!(prompt.contains("0009-1-next-part"));
        assert!(prompt.contains("Modify only files under `automation/slices/`"));
        assert!(prompt.contains("do not implement the slice"));
        assert!(prompt.contains("frontend and lowering are too broad together"));
    }

    #[test]
    fn terminal_review_preserves_9999_and_inserts_remediation_before_it() {
        let assignment = AgendaAssignment {
            path: "automation/slices/9999-project-acceptance.md".to_owned(),
            change: "9999-project-acceptance".to_owned(),
            title: "Project acceptance".to_owned(),
            content: "# Project acceptance\n\n## Objective\n\nAccept it.".to_owned(),
        };

        let ready = terminal_review_prompt(&assignment, None);
        assert!(ready.contains("complete goal in `openspec/config.yaml`"));
        assert!(ready.contains(
            "leave the repository, index, working tree, and Git history exactly unchanged"
        ));
        assert!(ready.contains("return READY"));
        assert!(
            ready.contains("preserve `automation/slices/9999-project-acceptance.md` byte-for-byte")
        );
        assert!(ready.contains("sort after every existing nonterminal slice and before `9999`"));
        assert!(ready.contains("Modify only files under `automation/slices/`"));

        let failure = TooLargeOutcome {
            stage: Stage::Verify,
            summary: "TLS hostname rejection is not implemented".to_owned(),
        };
        let remediation = terminal_review_prompt(&assignment, Some(&failure));
        assert!(remediation.contains("READY is not a valid result"));
        assert!(remediation.contains("TLS hostname rejection is not implemented"));
    }

    #[test]
    fn terminal_gate_requires_one_durable_frontier_review() {
        let mut state =
            RunState::new_campaign("advance".to_owned(), ChangeSnapshot::default(), Some(20));
        state.stage = Stage::Propose;
        state.change = Some("9999-project-acceptance".to_owned());
        state.agenda = Some(AgendaAssignment {
            path: "automation/slices/9999-project-acceptance.md".to_owned(),
            change: "9999-project-acceptance".to_owned(),
            title: "Project acceptance".to_owned(),
            content: "# Project acceptance\n".to_owned(),
        });

        assert!(is_terminal_gate(&state));
        assert!(needs_terminal_review(&state));
        assert!(uses_terminal_frontier(&state, true));
        assert!(!uses_terminal_frontier(&state, false));
        state.terminal_review_complete = true;
        assert!(!needs_terminal_review(&state));
        state.local_only = true;
        assert!(!uses_terminal_frontier(&state, true));

        let decoded: RunState =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(decoded.terminal_review_complete);
        assert_eq!(decoded.terminal_remediations, 0);
    }

    #[test]
    fn terminal_acceptance_keeps_model_work_on_frontier() {
        for stage in [Stage::Propose, Stage::Apply, Stage::Verify, Stage::Repair] {
            assert!(stage_uses_frontier_model(stage, false, true));
        }
        for stage in [
            Stage::ProposalCommit,
            Stage::Archive,
            Stage::FinalCommit,
            Stage::Complete,
            Stage::Done,
        ] {
            assert!(!stage_uses_frontier_model(stage, false, true));
        }
        assert!(!stage_uses_frontier_model(Stage::Apply, false, false));
        assert!(stage_uses_frontier_model(Stage::Apply, true, false));
    }

    #[test]
    fn stage_title_identifies_role_and_connection() {
        let mut launcher = ClaudeLauncher::parse("claude", None, None, None, None, None).unwrap();
        launcher.connection_name = Some("anthropic".to_owned());
        let agent_launcher = AgentLauncher::Claude(launcher);

        assert_eq!(
            model_stage_title("Apply", "worker", &agent_launcher),
            "Apply · worker (anthropic, claude)"
        );
    }

    #[test]
    fn terminal_remediation_must_be_committed_and_structurally_executable() {
        let remediation = AgendaAssignment {
            path: "automation/slices/0014-missing-tls-case.md".to_owned(),
            change: "0014-missing-tls-case".to_owned(),
            title: "Missing TLS case".to_owned(),
            content: "# Missing TLS case\n\n## Objective\nAdd it.\n\n## Prerequisites\nEarlier slices.\n\n## Acceptance Criteria\n- Works.\n\n## Required Tests\n- End to end.\n".to_owned(),
        };
        let committed = vec![
            remediation.path.clone(),
            "automation/slices/README.md".to_owned(),
        ];
        assert!(validate_terminal_remediation_assignment(&remediation, &committed).is_ok());

        let mut malformed = remediation.clone();
        malformed.content = "# Missing TLS case\n\n## Objective\nAdd it.\n".to_owned();
        assert!(validate_terminal_remediation_assignment(&malformed, &committed).is_err());

        let uncommitted = vec!["automation/slices/README.md".to_owned()];
        assert!(validate_terminal_remediation_assignment(&remediation, &uncommitted).is_err());
    }

    #[test]
    fn proposal_postcondition_repair_preserves_the_campaign_rubric() {
        let prompt = proposal_postcondition_repair("next slice", "No change exists.");
        assert!(prompt.starts_with("next slice"));
        assert!(prompt.contains("same planning session"));
        assert!(prompt.contains("do not merely describe"));
        assert!(prompt.contains("isPlanningComplete: true"));
        assert!(prompt.contains("do not create a duplicate change"));
        assert!(prompt.contains("return TOO_LARGE"));
    }

    #[test]
    fn direction_reopens_repair_after_implementation() {
        let mut state = RunState::new("build it".to_owned(), ChangeSnapshot::default());
        state.change = Some("build-it".to_owned());
        state.stage = Stage::Archive;
        state.pending_repair = Some("obsolete verifier result".to_owned());
        queue_direction(&mut state, "keep the AST unchanged");
        assert_eq!(state.stage, Stage::Repair);
        assert_eq!(
            state.pending_direction.as_deref(),
            Some("keep the AST unchanged")
        );
        assert!(state.pending_repair.is_none());

        state.consume_direction();
        assert!(state.pending_direction.is_none());
    }

    #[test]
    fn existing_change_starts_at_proposal_commit() {
        let state = RunState::continue_existing(
            "test-infrastructure".to_owned(),
            ChangeSnapshot::default(),
        );
        assert_eq!(state.change.as_deref(), Some("test-infrastructure"));
        assert_eq!(state.stage, Stage::ProposalCommit);
        assert!(state.planning_session.is_none());
    }

    #[test]
    fn planning_resume_starts_fresh_when_the_backend_changes() {
        let mut state = RunState::new("build it".to_owned(), ChangeSnapshot::default());
        state.planning_session = Some(SessionId::new("claude-session"));
        state.planning_backend = Some("Claude".to_owned());

        let backend = PlanningBackend {
            name: "OpenCode",
            actual_session: None,
        };
        let session = planning_session(&mut state, &backend).unwrap();

        assert!(matches!(&session, SessionMode::New { .. }));
        assert_ne!(session.id().as_str(), "claude-session");
        assert_eq!(state.planning_backend.as_deref(), Some("OpenCode"));
    }

    #[test]
    fn planning_resume_preserves_an_existing_session_with_the_same_backend() {
        let mut state = RunState::new("build it".to_owned(), ChangeSnapshot::default());
        state.planning_session = Some(SessionId::new("claude-session"));
        let backend = PlanningBackend {
            name: "Claude",
            actual_session: None,
        };

        let session = planning_session(&mut state, &backend).unwrap();

        assert!(matches!(session, SessionMode::Resume { id } if id.as_str() == "claude-session"));
    }

    #[test]
    fn planning_checkpoint_has_actual_session_before_an_interrupted_turn() {
        let backend = PlanningBackend {
            name: "Codex",
            actual_session: Some("actual-thread"),
        };
        for saved_session in [None, Some(SessionId::new("stale-thread"))] {
            let mut state =
                RunState::new_campaign("advance".to_owned(), ChangeSnapshot::default(), Some(10));
            state.stage = Stage::Propose;
            state.change = Some("0001-pinned-contract-build".to_owned());
            state.planning_session = saved_session;
            state.planning_backend = Some("Codex".to_owned());
            let mut expected = serde_json::to_value(&state).unwrap();
            expected["planning_session"] = Value::from("actual-thread");

            let session = planning_session(&mut state, &backend).unwrap();
            let checkpoint = serde_json::to_value(&state).unwrap();
            assert_eq!(checkpoint, expected);
            assert!(
                backend
                    .invoke(
                        session,
                        "continue proposal",
                        "Propose",
                        StageProtocol::Propose
                    )
                    .is_err()
            );
            assert_eq!(serde_json::to_value(&state).unwrap(), checkpoint);
        }
    }

    #[test]
    fn yolo_skips_the_proposal_commit_stage() {
        assert_eq!(stage_after_propose(false), Stage::ProposalCommit);
        assert_eq!(stage_after_propose(true), Stage::Apply);
    }

    #[test]
    fn yolo_stage_numbers_omit_the_proposal_commit() {
        assert_eq!(
            displayed_stage_position(Stage::Propose, true, false, true),
            (1, 5)
        );
        assert_eq!(
            displayed_stage_position(Stage::Apply, true, false, true),
            (2, 5)
        );
        assert_eq!(
            displayed_stage_position(Stage::FinalCommit, true, false, true),
            (5, 5)
        );
        assert_eq!(
            displayed_stage_position(Stage::FinalCommit, true, false, false),
            (6, 6)
        );
    }

    #[test]
    fn migrates_existing_schema_two_checkpoint() {
        let value: Value = serde_json::from_str(
            r#"{
                "schema_version": 2,
                "request": "continue slice m",
                "change": "slice-m",
                "stage": "repair",
                "verify_retries": 2,
                "proposal_commit": "abc123",
                "final_commit": null,
                "before_changes": {"changes": {}},
                "pending_repair": "fix lowering",
                "sessions": [{
                    "stage": "explore+propose",
                    "id": "00000000-0000-0000-0000-000000000000",
                    "name": "slice-m"
                }]
            }"#,
        )
        .unwrap();
        let state = migrate_v2(value).unwrap();
        assert_eq!(state.schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(state.stage, Stage::Repair);
        assert_eq!(state.change.as_deref(), Some("slice-m"));
        assert_eq!(
            state.planning_session,
            Some(SessionId::new(Uuid::nil().to_string()))
        );
        assert_eq!(state.pending_repair.as_deref(), Some("fix lowering"));
    }

    #[test]
    fn migrates_schema_three_checkpoint_without_an_agenda_assignment() {
        let mut value = serde_json::to_value(RunState::new(
            "build it".to_owned(),
            ChangeSnapshot::default(),
        ))
        .unwrap();
        value["schema_version"] = Value::from(3);
        value.as_object_mut().unwrap().remove("agenda");
        let state = migrate_v3(value).unwrap();
        assert_eq!(state.schema_version, STATE_SCHEMA_VERSION);
        assert!(state.agenda.is_none());
        assert_eq!(state.stage, Stage::Explore);
    }

    #[test]
    fn rejects_blocked_legacy_checkpoint_without_guessing_previous_stage() {
        let value: Value = serde_json::from_str(
            r#"{
                "schema_version": 2,
                "request": "build",
                "stage": "blocked"
            }"#,
        )
        .unwrap();
        assert!(migrate_v2(value).is_err());
    }

    #[test]
    fn validates_change_names() {
        assert!(validate_change_name("slice-m").is_ok());
        assert!(validate_change_name("../slice-m").is_err());
    }

    #[test]
    fn absent_active_change_is_durable_archive_evidence() {
        let mut changes = ChangeSnapshot::default();
        changes
            .changes
            .insert("other-change".to_owned(), Value::Null);
        assert!(archive_is_absent(&changes, "slice-n"));
        assert!(!archive_is_absent(&changes, "other-change"));
    }

    #[test]
    fn archive_prompt_pre_authorizes_normal_spec_sync() {
        let subject = archive_subject("0009-https-tls-forwarding-valid");

        assert!(subject.contains("choose `Sync now (recommended)`"));
        assert!(subject.contains("choose `Archive now`"));
        assert!(subject.contains("do not treat that confirmation as a BLOCKED condition"));
    }

    #[test]
    fn milestone_commit_prompts_require_useful_openspec_summaries() {
        let proposal = proposal_commit_message("0009-https-tls-forwarding-valid");
        assert!(proposal.contains("subject exactly `openspec: propose"));
        assert!(proposal.contains("intended observable change and why"));
        assert!(proposal.contains("Use imperative mood"));
        assert!(proposal.contains("hard-wrap every body line at 72 columns or fewer"));
        assert!(proposal.contains("Do not write one dense summary paragraph"));
        assert!(proposal.contains("file or task inventories"));

        let completion = completion_commit_message("0009-https-tls-forwarding-valid");
        assert!(completion.contains("subject exactly `openspec: complete"));
        assert!(completion.contains("delivered observable behavior"));
        assert!(completion.contains("validation that actually ran"));
        assert!(completion.contains("Separate paragraphs with a blank line"));
        assert!(completion.contains("checks that did not run"));
    }

    #[test]
    fn milestone_commit_prompts_do_not_block_on_committed_message_formatting() {
        for prompt in [
            proposal_commit_message("slice-formatting"),
            completion_commit_message("slice-formatting"),
        ] {
            assert!(
                prompt
                    .contains("Check the subject and body formatting before creating each commit")
            );
            assert!(prompt.contains("Once all relevant work is committed, message formatting alone is not a BLOCKED condition"));
            assert!(prompt.contains("preserve the existing commits"));
            assert!(prompt.contains("report READY without requesting permission to amend or creating a replacement commit"));
        }
    }

    #[test]
    fn campaign_limit_resume_command_raises_the_cumulative_ceiling() {
        assert_eq!(
            campaign_limit_resume_command(Path::new("/tmp/compiler project"), 5),
            "opsx-build --repo '/tmp/compiler project' --resume --loop --max-iterations 10"
        );
    }

    #[test]
    fn single_advance_completion_points_to_remaining_campaign_work() {
        let repo = std::env::temp_dir().join(format!(
            "opsx-build-single-advance-continuation-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(repo.join("automation/slices")).unwrap();
        std::fs::create_dir_all(repo.join("openspec/changes/archive/0001-first")).unwrap();
        std::fs::write(repo.join("automation/slices/0001-first.md"), "# First\n").unwrap();
        std::fs::write(repo.join("automation/slices/0002-second.md"), "# Second\n").unwrap();
        let state = RunState::new("advance".to_owned(), ChangeSnapshot::default());

        assert_eq!(
            single_advance_continuation_command(&repo, &state),
            Some(format!(
                "opsx-build --repo {} --loop advance",
                shell_quote(&repo.to_string_lossy())
            ))
        );
        let campaign =
            RunState::new_campaign("advance".to_owned(), ChangeSnapshot::default(), None);
        assert_eq!(single_advance_continuation_command(&repo, &campaign), None);

        std::fs::create_dir(repo.join("openspec/changes/archive/0002-second")).unwrap();
        assert_eq!(single_advance_continuation_command(&repo, &state), None);

        std::fs::remove_dir_all(repo).unwrap();
    }
}
