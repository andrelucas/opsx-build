use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    agenda::{AgendaAssignment, AgendaSelection, discover as discover_agenda, has_subdivision},
    claude::{
        ClaudeClient, ClaudeLauncher, ClaudeOutputFormat, SessionMode, SkillCommands,
        StageProtocol, StageSignal, build_claude_command, build_interactive_claude_command,
        stage_prompt,
    },
    cli::Cli,
    git::{
        SliceBaseline, baseline_matches_except, capture_slice_baseline, committed_paths_since,
        current_head, head_descends_from, legacy_metadata_dir, metadata_dir,
        release_slice_baseline, remove_metadata, repository_root, reset_to_slice_baseline,
    },
    openspec::{
        ChangeSnapshot, identify_assigned_change, identify_change, planning_status,
        select_existing_change, snapshot as openspec_snapshot,
    },
    process::{
        PauseRequested, ProcessRunner, WorkerEscalationRequested, prerequisite_exists, shell_quote,
    },
    skills::{SkillInstallAction, ensure_unattended_skills},
    state::Stage,
    ui::{CampaignIterationView, CampaignView, Ui},
};

const TOTAL_STAGES: usize = 7;
const AGENDA_TOTAL_STAGES: usize = 6;
const STATE_SCHEMA_VERSION: u32 = 4;
const MAX_FRONTIER_REPLANS: u32 = 3;

pub struct App<U: Ui> {
    cli: Cli,
    ui: U,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunState {
    schema_version: u32,
    request: String,
    change: Option<String>,
    stage: Stage,
    verify_retries: u32,
    before_changes: ChangeSnapshot,
    planning_session: Option<Uuid>,
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
        }
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

        prerequisite_exists("git", &requested_repo, &self.ui)?;
        let repo = repository_root(&requested_repo, &self.ui)?;
        self.ui.banner(&repo.display().to_string());

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

        let launcher = ClaudeLauncher::from_connection(&self.cli.worker_connection)?;
        if launcher.program != "claude" {
            prerequisite_exists(&launcher.program, &repo, &self.ui)?;
        }
        prerequisite_exists("claude", &repo, &self.ui)?;

        self.debug_configuration(&repo, &launcher);
        if let Some(path) = &self.cli.config_path {
            self.ui.info(&format!("Using config `{}`", path.display()));
        }

        if self.cli.interactive {
            return self.run_interactive(&repo, &launcher);
        }

        let frontier_launcher = ClaudeLauncher::from_connection(&self.cli.frontier_connection)?;
        if frontier_launcher.program != "claude" {
            prerequisite_exists(&frontier_launcher.program, &repo, &self.ui)?;
        }
        if launcher.connection_name.is_some() || frontier_launcher.connection_name.is_some() {
            self.ui.info(&format!(
                "Connections: worker {}, frontier {}",
                connection_description(&launcher),
                connection_description(&frontier_launcher)
            ));
        }
        self.ui.debug(&format!(
            "frontier launcher: connection={:?}, program=`{}`, prefix args={:?}, model={:?}, environment={:?}, unset environment={:?}",
            frontier_launcher.connection_name,
            frontier_launcher.program,
            frontier_launcher.prefix_args,
            frontier_launcher.model,
            frontier_launcher
                .environment
                .iter()
                .map(|variable| variable.name.as_str())
                .collect::<Vec<_>>(),
            frontier_launcher.unset_environment
        ));

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
            return self.resume(&repo, &launcher, &frontier_launcher, &commands);
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
            return self.continue_existing(&repo, &launcher, &frontier_launcher, &commands);
        }

        if self.cli.dry_run {
            return self.print_new_dry_run(&repo, &launcher, &frontier_launcher, &commands);
        }

        let before_changes = openspec_snapshot(&repo, &self.ui)?;
        let mut state = if self.cli.loop_workflow {
            RunState::new_campaign(
                self.cli.request.clone(),
                before_changes,
                self.cli.max_iterations,
            )
        } else {
            RunState::new(self.cli.request.clone(), before_changes)
        };
        self.assign_agenda(&repo, &mut state)?;
        arm_slice_baseline(&repo, &mut state, &self.ui)?;
        persist_state(&repo, &state, &self.ui)?;
        self.run_workflows(&repo, &launcher, &frontier_launcher, &commands, state)
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
                        "Assigned change `{}` already has complete planning; continuing at proposal commit",
                        assignment.change
                    ));
                    Stage::ProposalCommit
                } else {
                    Stage::Propose
                };
                state.agenda = Some(assignment);
            }
        }
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
                "{action} bundled Claude skill `{}` in target repository",
                installed.name
            ));
        }
        Ok(changes.len())
    }

    fn continue_existing(
        &self,
        repo: &Path,
        launcher: &ClaudeLauncher,
        frontier_launcher: &ClaudeLauncher,
        commands: &SkillCommands,
    ) -> Result<()> {
        let active_changes = openspec_snapshot(repo, &self.ui)?;
        let change = select_existing_change(&active_changes, self.cli.change.as_deref())?;
        validate_change_name(&change)?;
        let mut state = RunState::continue_existing(change.clone(), active_changes);
        self.ui.info(&format!(
            "Continuing OpenSpec change `{change}`; planning work will be committed if necessary"
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

    fn resume(
        &self,
        repo: &Path,
        launcher: &ClaudeLauncher,
        frontier_launcher: &ClaudeLauncher,
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

        if state.slice_baseline.is_none() && matches!(state.stage, Stage::Explore | Stage::Propose)
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

        persist_state(repo, &state, &self.ui)?;
        self.run_workflows(repo, launcher, frontier_launcher, commands, state)
    }

    fn run_workflows(
        &self,
        repo: &Path,
        launcher: &ClaudeLauncher,
        frontier_launcher: &ClaudeLauncher,
        commands: &SkillCommands,
        mut state: RunState,
    ) -> Result<()> {
        loop {
            self.present_campaign(&state);
            let iteration_started = (matches!(state.stage, Stage::Explore | Stage::Propose)
                && state.planning_session.is_none())
            .then(Instant::now);
            match self.execute(repo, launcher, commands, state)? {
                WorkflowOutcome::TooLarge(escalated) => {
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
                    let Some(campaign) = completed_state.campaign.as_ref() else {
                        let change = completed_state.change.as_deref().unwrap_or("change");
                        let head = completed_state
                            .final_head
                            .as_deref()
                            .unwrap_or("unknown HEAD");
                        self.ui.finish_dashboard();
                        self.ui
                            .success(&format!("COMPLETE `{change}` — final {}", short_hash(head)));
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

    fn run_frontier_replan(
        &self,
        repo: &Path,
        frontier_launcher: &ClaudeLauncher,
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

        let frontier = ClaudeClient::new(
            repo,
            frontier_launcher,
            &self.cli.permission_mode,
            self.ui.supports_stream_input(),
            self.cli.stream_claude,
            self.cli.max_output_retries,
            &self.ui,
        );
        let session = Uuid::new_v4();
        let stage_number = outcome.stage.number().saturating_sub(1).max(1);
        self.ui
            .stage(stage_number, AGENDA_TOTAL_STAGES, "Frontier replan");
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
                        id: session,
                        name: Some(format!("{}-frontier-replan", assignment.change)),
                    }
                } else {
                    SessionMode::Resume { id: session }
                },
                &stage_prompt("", &prompt, StageProtocol::Frontier),
                if attempt == 0 {
                    "Frontier Claude is subdividing the oversized agenda slice"
                } else {
                    "Frontier Claude is correcting the agenda subdivision"
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
        launcher: &ClaudeLauncher,
        commands: &SkillCommands,
        mut state: RunState,
    ) -> Result<WorkflowOutcome> {
        let claude = ClaudeClient::new(
            repo,
            launcher,
            &self.cli.permission_mode,
            self.ui.supports_stream_input(),
            self.cli.stream_claude,
            self.cli.max_output_retries,
            &self.ui,
        );
        let worker_claude = ClaudeClient::new(
            repo,
            launcher,
            &self.cli.permission_mode,
            true,
            self.cli.stream_claude,
            self.cli.max_output_retries,
            &self.ui,
        )
        .with_stage_timeout(Duration::from_secs(
            u64::from(self.cli.local_worker_timeout_minutes) * 60,
        ));
        self.ui.change_name(state.change.as_deref());

        loop {
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

            let (stage_number, total_stages) = if state.agenda.is_some() {
                (state.stage.number() - 1, AGENDA_TOTAL_STAGES)
            } else {
                (state.stage.number(), TOTAL_STAGES)
            };
            self.ui
                .stage(stage_number, total_stages, state.stage.title());
            match state.stage {
                Stage::Explore => self.run_explore(repo, &worker_claude, commands, &mut state)?,
                Stage::Propose => self.run_propose(repo, &worker_claude, commands, &mut state)?,
                Stage::ProposalCommit => self.run_proposal_commit(repo, &claude, &mut state)?,
                Stage::Apply => self.run_apply(repo, &worker_claude, commands, &mut state)?,
                Stage::Verify => self.run_verify(repo, &worker_claude, commands, &mut state)?,
                Stage::Repair => self.run_repair(repo, &worker_claude, commands, &mut state)?,
                Stage::Archive => self.run_archive(repo, &claude, commands, &mut state)?,
                Stage::FinalCommit => self.run_final_commit(repo, &claude, &mut state)?,
                Stage::Complete | Stage::Done => unreachable!(),
            }
        }
    }

    fn run_explore(
        &self,
        repo: &Path,
        claude: &ClaudeClient<'_, U>,
        commands: &SkillCommands,
        state: &mut RunState,
    ) -> Result<()> {
        let (session, is_new) = planning_session(state);
        persist_state(repo, state, &self.ui)?;
        let subject = campaign_subject(state);
        let Some(result) = worker_result(
            claude.invoke(
                session_mode(session, is_new, "opsx-build-planning"),
                &stage_prompt(&commands.explore, &subject, StageProtocol::Worker),
                "Claude is exploring the change",
                StageProtocol::Worker,
            ),
            repo,
            state,
            &self.ui,
        )?
        else {
            return Ok(());
        };
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
        claude.compact_session(session, "Explore")?;
        Ok(())
    }

    fn run_propose(
        &self,
        repo: &Path,
        claude: &ClaudeClient<'_, U>,
        commands: &SkillCommands,
        state: &mut RunState,
    ) -> Result<()> {
        let (session, is_new) = planning_session(state);
        persist_state(repo, state, &self.ui)?;
        let planning_context = if state.agenda.is_some() {
            "Use the assigned agenda slice as the planning authority and preserve correct partial artifacts for its exact assigned change. Do not report DONE: the orchestrator has already established that this agenda slice remains."
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
                    session_mode(
                        session,
                        is_new && !retrying_postcondition,
                        "opsx-build-planning",
                    ),
                    &stage_prompt(&commands.propose, &subject, StageProtocol::Propose),
                    if retrying_postcondition {
                        "Claude is correcting the incomplete proposal"
                    } else {
                        "Claude is creating OpenSpec artifacts"
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

            let after = openspec_snapshot(repo, &self.ui)?;
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

            let assigned_complete_without_list_change = state
                .agenda
                .as_ref()
                .filter(|assignment| after.changes.contains_key(&assignment.change))
                .map(|assignment| {
                    planning_status(repo, &assignment.change, &self.ui)
                        .map(|status| status.is_complete)
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
                    "Propose still created or modified no active OpenSpec change after one corrective turn. Last Claude summary: {}",
                    result.text.trim()
                );
            }

            let change_result = match &state.agenda {
                Some(assignment) if assigned_complete_without_list_change => {
                    Ok(assignment.change.clone())
                }
                Some(assignment) => {
                    identify_assigned_change(&state.before_changes, &after, &assignment.change)
                }
                None => identify_change(&state.before_changes, &after),
            };
            let change = change_result.with_context(|| {
                if retrying_postcondition {
                    format!(
                        "Propose still did not satisfy its READY postcondition after one corrective turn. Last Claude summary: {}",
                        result.text.trim()
                    )
                } else {
                    format!("Propose reported READY. Last Claude summary: {}", result.text.trim())
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
                    "Propose still left OpenSpec change `{change}` planning-incomplete after one corrective turn. Remaining work: {next_steps}. Last Claude summary: {}",
                    result.text.trim()
                );
            }
            state.change = Some(change.clone());
            self.ui.change_name(Some(&change));
            self.ui
                .info(&format!("Selected OpenSpec change `{change}`"));
            if let Err(error) = claude.rename_session(session, &change) {
                self.ui
                    .warn(&format!("Could not rename planning session: {error}"));
            }
            state.planning_session = None;
            state.stage = state.stage.after_ready()?;
            persist_state(repo, state, &self.ui)?;
            return Ok(());
        }
    }

    fn run_proposal_commit(
        &self,
        repo: &Path,
        claude: &ClaudeClient<'_, U>,
        state: &mut RunState,
    ) -> Result<()> {
        let change = require_change(state)?;
        let task = format!(
            "Create the proposal milestone Git commit for OpenSpec change `{change}`. Inspect Git status and diffs. Commit only the proposal artifacts for this change and directly related canonical OpenSpec specification updates, with commit message exactly `openspec: propose {change}`. Preserve all unrelated work. Never reset, stash, restore, discard, amend, or rewrite existing history. If the relevant proposal work is already committed and nothing remains to commit, confirm that and report READY."
        );
        let result = invoke_fresh(
            claude,
            &format!("{change}-proposal-commit"),
            &stage_prompt("", &task, StageProtocol::Ready),
            "Claude is committing the proposal",
            StageProtocol::Ready,
        )?;
        require_ready("proposal commit", &result.text, result.signal)?;
        state.proposal_head = current_head(repo, &self.ui)?;
        state.stage = state.stage.after_ready()?;
        persist_state(repo, state, &self.ui)
    }

    fn run_apply(
        &self,
        repo: &Path,
        claude: &ClaudeClient<'_, U>,
        commands: &SkillCommands,
        state: &mut RunState,
    ) -> Result<()> {
        let change = require_change(state)?;
        let base = format!(
            "{change}\n\nImplement or continue implementing this OpenSpec change completely. Preserve correct partial work and run appropriate project checks. Do not archive the change. If the assigned slice cannot reliably be completed and verified as one bounded worker-model change, report TOO_LARGE with evidence and an ordered decomposition instead of digging an increasingly broad implementation hole. Do not use TOO_LARGE for ordinary difficulty or correctable engineering failures."
        );
        let subject = with_direction(&base, state.pending_direction.as_deref());
        let Some(result) = worker_result(
            invoke_fresh(
                claude,
                &format!("{change}-apply"),
                &stage_prompt(&commands.apply, &subject, StageProtocol::Worker),
                "Claude is applying the OpenSpec change",
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
        claude: &ClaudeClient<'_, U>,
        commands: &SkillCommands,
        state: &mut RunState,
    ) -> Result<()> {
        let change = require_change(state)?;
        let Some(result) = worker_result(
            invoke_fresh(
                claude,
                &format!("{change}-verify-{}", state.verify_retries + 1),
                &stage_prompt(
                    &commands.verify,
                    &format!(
                        "{change}\n\nVerify the implementation against its OpenSpec artifacts and run relevant checks. Report RETRY only for a concrete, correctable implementation issue and explain the required repair."
                    ),
                    StageProtocol::Verify,
                ),
                "Claude is verifying specification compliance",
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
                self.ui.success("Verification passed");
                state.pending_repair = None;
                state.stage = state.stage.after_verified()?;
                persist_state(repo, state, &self.ui)
            }
            StageSignal::Retry => {
                state.verify_retries += 1;
                state.pending_repair = Some(result.text);
                if state.verify_retries > self.cli.max_verify_retries {
                    return record_too_large(
                        repo,
                        state,
                        format!(
                            "local worker exhausted {} verification repair cycle(s); the failed attempt requires frontier subdivision",
                            self.cli.max_verify_retries
                        ),
                        &self.ui,
                    );
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
        claude: &ClaudeClient<'_, U>,
        commands: &SkillCommands,
        state: &mut RunState,
    ) -> Result<()> {
        if state.verify_retries > self.cli.max_verify_retries && state.pending_direction.is_none() {
            return record_too_large(
                repo,
                state,
                format!(
                    "local worker exhausted {} verification repair cycle(s); the failed attempt requires frontier subdivision",
                    self.cli.max_verify_retries
                ),
                &self.ui,
            );
        }
        let change = require_change(state)?;
        let finding = state.pending_repair.as_deref().unwrap_or(
            "No verifier finding was supplied; apply the user's direction and re-run relevant checks.",
        );
        let base = format!(
            "{change}\n\nRepair or continue repairing the implementation and tests. Preserve correct partial work and the approved OpenSpec scope. Re-run relevant checks. If the verifier has exposed that the assigned slice cannot reliably fit one bounded worker-model change, report TOO_LARGE with evidence and an ordered decomposition. Do not use TOO_LARGE for an ordinary correctable verification failure.\n\nVerifier context:\n{finding}"
        );
        let subject = with_direction(&base, state.pending_direction.as_deref());
        let Some(result) = worker_result(
            invoke_fresh(
                claude,
                &format!("{change}-repair-{}", state.verify_retries),
                &stage_prompt(&commands.apply, &subject, StageProtocol::Worker),
                "Claude is repairing the implementation",
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
        claude: &ClaudeClient<'_, U>,
        commands: &SkillCommands,
        state: &mut RunState,
    ) -> Result<()> {
        let change = require_change(state)?;
        let result = invoke_fresh(
            claude,
            &format!("{change}-archive"),
            &stage_prompt(
                &commands.archive,
                &format!(
                    "{change}\n\nArchive this successfully verified OpenSpec change, including normal specification synchronization."
                ),
                StageProtocol::Ready,
            ),
            "Claude is archiving the OpenSpec change",
            StageProtocol::Ready,
        );
        let result = match result {
            Ok(result) => result,
            Err(error) if error.downcast_ref::<PauseRequested>().is_some() => return Err(error),
            Err(error) => match openspec_snapshot(repo, &self.ui) {
                Ok(changes) if archive_is_absent(&changes, &change) => {
                    self.ui.warn(
                        "Archive completed but Claude omitted its terminal result; accepting OpenSpec state",
                    );
                    state.stage = state.stage.after_ready()?;
                    return persist_state(repo, state, &self.ui);
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
        persist_state(repo, state, &self.ui)
    }

    fn run_final_commit(
        &self,
        repo: &Path,
        claude: &ClaudeClient<'_, U>,
        state: &mut RunState,
    ) -> Result<()> {
        let change = require_change(state)?;
        let task = format!(
            "Create the completion milestone Git commit for OpenSpec change `{change}`. Inspect Git status, history, and diffs. Commit the implementation, tests, synchronized specifications, archived change artifacts, and documentation that belong to this completed change, with commit message exactly `openspec: complete {change}`. Preserve all unrelated work. Never reset, stash, restore, discard, amend, or rewrite existing history. If all relevant work is already committed and nothing remains to commit, confirm that and report READY."
        );
        let result = invoke_fresh(
            claude,
            &format!("{change}-completion-commit"),
            &stage_prompt("", &task, StageProtocol::Ready),
            "Claude is committing the completed change",
            StageProtocol::Ready,
        )?;
        require_ready("completion commit", &result.text, result.signal)?;
        state.final_head = current_head(repo, &self.ui)?;
        state.stage = state.stage.after_ready()?;
        persist_state(repo, state, &self.ui)
    }

    fn print_new_dry_run(
        &self,
        repo: &Path,
        launcher: &ClaudeLauncher,
        frontier_launcher: &ClaudeLauncher,
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
        self.assign_agenda(repo, &mut dry_state)?;
        if dry_state.stage == Stage::Done {
            self.ui.info("Advance: the ordered agenda is complete");
            return Ok(());
        }
        let (planning_command, protocol, label) = if dry_state.stage == Stage::Propose {
            (&commands.propose, StageProtocol::Propose, "Propose")
        } else {
            (&commands.explore, StageProtocol::Worker, "Explore")
        };
        let prompt = stage_prompt(planning_command, &campaign_subject(&dry_state), protocol);
        let command = build_claude_command(
            repo,
            launcher,
            &self.cli.permission_mode,
            &SessionMode::New {
                id: Uuid::nil(),
                name: Some("opsx-build-planning".to_owned()),
            },
            &prompt,
            ClaudeOutputFormat::Json,
            Some(protocol.json_schema()),
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
        self.ui
            .info("Ask Claude in a fresh session to create proposal milestone commit");
        self.ui.info(&format!("Apply: {}", commands.apply));
        self.ui.info(&format!(
            "Verify/repair: {} / {}",
            commands.verify, commands.apply
        ));
        self.ui.info(&format!("Archive: {}", commands.archive));
        self.ui
            .info("Ask Claude to create completion milestone commit");
        self.ui.info(&format!(
            "Frontier fallback: {} after local TOO_LARGE or a {} minute worker timeout",
            frontier_launcher.program, self.cli.local_worker_timeout_minutes
        ));
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
        self.ui
            .info(&format!("Next stage: {}", state.stage.title()));
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
        if matches!(state.stage, Stage::Apply | Stage::Repair | Stage::Verify) {
            self.ui
                .info(&format!("Implementation workflow: {}", commands.apply));
            self.ui
                .info(&format!("Verification workflow: {}", commands.verify));
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

    fn debug_configuration(&self, repo: &Path, launcher: &ClaudeLauncher) {
        self.ui
            .debug("complete prompts are visible and may contain repository content");
        self.ui.debug(&format!("repository: {}", repo.display()));
        self.ui.debug(&format!(
            "Claude launcher: connection={:?}, program=`{}`, prefix args={:?}, model={:?}, auto-compact window={:?}, auto-compact percent={:?}, max output tokens={:?}, environment={:?}, unset environment={:?}, permission mode=`{}`, stream filter={:?}",
            launcher.connection_name,
            launcher.program,
            launcher.prefix_args,
            launcher.model,
            launcher.auto_compact_window,
            launcher.auto_compact_percent,
            launcher.max_output_tokens,
            launcher
                .environment
                .iter()
                .map(|variable| variable.name.as_str())
                .collect::<Vec<_>>(),
            launcher.unset_environment,
            self.cli.permission_mode,
            self.cli.stream_claude
        ));
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

fn connection_description(launcher: &ClaudeLauncher) -> String {
    let name = launcher.connection_name.as_deref().unwrap_or("default");
    match launcher.model.as_deref() {
        Some(model) => format!("`{name}` ({model})"),
        None => format!("`{name}` (harness default model)"),
    }
}

fn planning_session(state: &mut RunState) -> (Uuid, bool) {
    match state.planning_session {
        Some(id) => (id, false),
        None => {
            let id = Uuid::new_v4();
            state.planning_session = Some(id);
            (id, true)
        }
    }
}

fn session_mode(id: Uuid, is_new: bool, name: &str) -> SessionMode {
    if is_new {
        SessionMode::New {
            id,
            name: Some(name.to_owned()),
        }
    } else {
        SessionMode::Resume { id }
    }
}

fn invoke_fresh<U: Ui>(
    claude: &ClaudeClient<'_, U>,
    name: &str,
    prompt: &str,
    activity: &str,
    protocol: StageProtocol,
) -> Result<crate::claude::ClaudeResult> {
    claude.invoke(
        SessionMode::New {
            id: Uuid::new_v4(),
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
    result: Result<crate::claude::ClaudeResult>,
    repo: &Path,
    state: &mut RunState,
    ui: &U,
) -> Result<Option<crate::claude::ClaudeResult>> {
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
    if matches!(state.stage, Stage::Complete | Stage::Done) {
        state.slice_baseline = None;
        return Ok(());
    }
    let baseline = capture_slice_baseline(repo, ui)?;
    state.slice_baseline = Some(baseline);
    Ok(())
}

fn release_state_baseline<U: Ui>(repo: &Path, state: &mut RunState, ui: &U) {
    let Some(baseline) = state.slice_baseline.take() else {
        return;
    };
    if let Err(error) = release_slice_baseline(repo, &baseline, ui) {
        ui.warn(&format!(
            "Could not release private rollback metadata; workflow state is unaffected: {error}"
        ));
    }
}

fn proposal_postcondition_repair(subject: &str, failure: &str) -> String {
    format!(
        "{subject}\n\nPOSTCONDITION REPAIR: {failure} Continue in this same planning session and perform the proposal workflow now; do not merely describe what should be proposed and do not create a duplicate change. Inspect active OpenSpec changes first and continue the intended existing scaffold when one is present. Follow any exact agenda assignment above; otherwise use the completed exploration and repository's durable planning evidence. Run all commands synchronously and wait for every command and subagent to finish. Return READY only after `openspec status --change <name> --json` reports `isPlanningComplete: true`. If no change is warranted, return DONE. If the selected slice exceeds one reliable worker change, return TOO_LARGE with an ordered decomposition. Return BLOCKED only for a genuine external decision."
    )
}

fn campaign_subject(state: &RunState) -> String {
    if let Some(assignment) = &state.agenda {
        let iteration = state
            .campaign
            .as_ref()
            .map(|campaign| format!("campaign iteration {}", campaign.iteration))
            .unwrap_or_else(|| "an advance operation".to_owned());
        return format!(
            "Advance the repository by implementing the exact ordered agenda assignment below. This is {iteration}. Do not select, create, or modify a different slice. Create or continue the OpenSpec change with the exact name `{change}`. The agenda content is authoritative; use repository inspection only to elaborate its implementation details.\n\nAssigned agenda file: `{path}`\n\n--- BEGIN ASSIGNED AGENDA SLICE ---\n{content}\n--- END ASSIGNED AGENDA SLICE ---",
            change = assignment.change,
            path = assignment.path,
            content = assignment.content.trim()
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
        3 => migrate_v3(value),
        2 => migrate_v2(value),
        other => bail!(
            "unsupported checkpoint schema {other}; use `--forget` to leave repository state untouched and start over"
        ),
    }
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
        .and_then(|id| Uuid::parse_str(id).ok());

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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(subject.contains("Do not select, create, or modify a different slice"));
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
        assert_eq!(state.planning_session, Some(Uuid::nil()));
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
    fn campaign_limit_resume_command_raises_the_cumulative_ceiling() {
        assert_eq!(
            campaign_limit_resume_command(Path::new("/tmp/compiler project"), 5),
            "opsx-build --repo '/tmp/compiler project' --resume --loop --max-iterations 10"
        );
    }
}
