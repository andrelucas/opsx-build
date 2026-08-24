use std::{
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    claude::{
        ClaudeClient, ClaudeLauncher, ClaudeOutputFormat, SessionMode, SkillCommands,
        StageProtocol, StageSignal, build_claude_command, build_interactive_claude_command,
        stage_prompt,
    },
    cli::Cli,
    git::{current_head, legacy_metadata_dir, metadata_dir, remove_metadata, repository_root},
    openspec::{
        ChangeSnapshot, identify_change, select_existing_change, snapshot as openspec_snapshot,
    },
    process::{PauseRequested, ProcessRunner, prerequisite_exists},
    skills::{SkillInstallAction, ensure_unattended_skills},
    state::Stage,
    ui::{CampaignIterationView, CampaignView, Ui},
};

const TOTAL_STAGES: usize = 7;
const STATE_SCHEMA_VERSION: u32 = 3;

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

        let launcher = ClaudeLauncher::parse(
            &self.cli.claude_command,
            self.cli.claude_model.clone(),
            self.cli.auto_compact_window,
            self.cli.auto_compact_percent,
            self.cli.max_output_tokens,
        )?;
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

        prerequisite_exists("openspec", &repo, &self.ui)?;
        if !repo.join("openspec/config.yaml").is_file() {
            bail!(
                "no OpenSpec setup found at `{}`; expected `openspec/config.yaml`",
                repo.display()
            );
        }
        for installed in ensure_unattended_skills(&repo, self.cli.dry_run)? {
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
        let commands = SkillCommands::discover(&repo, &self.cli)?;
        self.ui.debug(&format!(
            "workflow commands: explore=`{}`, propose=`{}`, apply=`{}`, verify=`{}`, archive=`{}`",
            commands.explore, commands.propose, commands.apply, commands.verify, commands.archive
        ));

        if self.cli.resume {
            return self.resume(&repo, &launcher, &commands);
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
            return self.continue_existing(&repo, &launcher, &commands);
        }

        if self.cli.dry_run {
            return self.print_new_dry_run(&repo, &launcher, &commands);
        }

        let before_changes = openspec_snapshot(&repo, &self.ui)?;
        let state = if self.cli.loop_workflow {
            RunState::new_campaign(
                self.cli.request.clone(),
                before_changes,
                self.cli.max_iterations,
            )
        } else {
            RunState::new(self.cli.request.clone(), before_changes)
        };
        persist_state(&repo, &state, &self.ui)?;
        self.run_workflows(&repo, &launcher, &commands, state)
    }

    fn continue_existing(
        &self,
        repo: &Path,
        launcher: &ClaudeLauncher,
        commands: &SkillCommands,
    ) -> Result<()> {
        let active_changes = openspec_snapshot(repo, &self.ui)?;
        let change = select_existing_change(&active_changes, self.cli.change.as_deref())?;
        validate_change_name(&change)?;
        let state = RunState::continue_existing(change.clone(), active_changes);
        self.ui.info(&format!(
            "Continuing OpenSpec change `{change}`; planning work will be committed if necessary"
        ));
        if self.cli.dry_run {
            return self.print_resume_dry_run(&state, commands);
        }
        persist_state(repo, &state, &self.ui)?;
        self.run_workflows(repo, launcher, commands, state)
    }

    fn forget_checkpoint(&self, repo: &Path) -> Result<()> {
        if self.cli.dry_run {
            self.ui.warn("DRY RUN — the checkpoint would be forgotten");
            return Ok(());
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

        if let Some(outcome) = state.too_large.take() {
            self.ui.warn(&format!(
                "Retrying {} after previous TOO_LARGE outcome: {}",
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
        self.run_workflows(repo, launcher, commands, state)
    }

    fn run_workflows(
        &self,
        repo: &Path,
        launcher: &ClaudeLauncher,
        commands: &SkillCommands,
        mut state: RunState,
    ) -> Result<()> {
        loop {
            self.present_campaign(&state);
            let iteration_started = (state.stage == Stage::Explore
                && state.planning_session.is_none())
            .then(Instant::now);
            match self.execute(repo, launcher, commands, state)? {
                WorkflowOutcome::TooLarge(state) => {
                    self.ui.finish_dashboard();
                    let outcome = state
                        .too_large
                        .as_ref()
                        .context("worker returned TOO_LARGE without durable outcome details")?;
                    bail!(
                        "TOO_LARGE during {}:\n{}\n\nAutomatic frontier decomposition is not implemented yet. Decompose or revise the assigned slice, then resume the preserved {} checkpoint.",
                        outcome.stage.title(),
                        outcome.summary.trim(),
                        outcome.stage.title()
                    );
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
                    if max_iterations.is_some_and(|maximum| iteration >= maximum) {
                        self.ui.finish_dashboard();
                        bail!(
                            "campaign reached its configured limit of {iteration} completed iteration(s) before Propose returned DONE"
                        );
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
                    persist_state(repo, &state, &self.ui)?;
                }
            }
        }
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
        self.ui.change_name(state.change.as_deref());

        loop {
            if state.too_large.is_some() {
                return Ok(WorkflowOutcome::TooLarge(state));
            }
            if state.stage == Stage::Complete {
                return Ok(WorkflowOutcome::Complete(state));
            }
            if state.stage == Stage::Done {
                return Ok(WorkflowOutcome::Done(state));
            }

            self.ui
                .stage(state.stage.number(), TOTAL_STAGES, state.stage.title());
            match state.stage {
                Stage::Explore => self.run_explore(repo, &claude, commands, &mut state)?,
                Stage::Propose => self.run_propose(repo, &claude, commands, &mut state)?,
                Stage::ProposalCommit => self.run_proposal_commit(repo, &claude, &mut state)?,
                Stage::Apply => self.run_apply(repo, &claude, commands, &mut state)?,
                Stage::Verify => self.run_verify(repo, &claude, commands, &mut state)?,
                Stage::Repair => self.run_repair(repo, &claude, commands, &mut state)?,
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
        let result = claude.invoke(
            session_mode(session, is_new, "opsx-build-planning"),
            &stage_prompt(&commands.explore, &subject, StageProtocol::Worker),
            "Claude is exploring the change",
            StageProtocol::Worker,
        )?;
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
        let base = format!(
            "{}\n\nUse conclusions from exploration and preserve correct partial proposal artifacts from any interrupted attempt. If the requested objective is already satisfied and no coherent implementation work remains, do not create or modify OpenSpec artifacts; report DONE.",
            campaign_subject(state)
        );
        let mut retrying_postcondition = false;
        loop {
            let subject = if retrying_postcondition {
                proposal_postcondition_repair(&base)
            } else {
                base.clone()
            };
            let result = claude.invoke(
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
            )?;

            let after = openspec_snapshot(repo, &self.ui)?;
            match result.signal {
                StageSignal::Done => {
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
                    if after != state.before_changes {
                        bail!(
                            "Propose returned TOO_LARGE after changing OpenSpec state; preserving the repository for inspection"
                        );
                    }
                    return record_too_large(repo, state, result.text, &self.ui);
                }
                StageSignal::Ready => {}
                StageSignal::Blocked => return blocked("propose", &result.text),
                other => bail!("propose returned unexpected terminal status {other:?}"),
            }

            if after == state.before_changes && !retrying_postcondition {
                self.ui.warn(
                    "Propose reported READY without creating or modifying an active OpenSpec change; retrying the same planning session once",
                );
                retrying_postcondition = true;
                continue;
            }

            let change = identify_change(&state.before_changes, &after).with_context(|| {
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
            state.change = Some(change.clone());
            self.ui.change_name(Some(&change));
            self.ui
                .info(&format!("Selected OpenSpec change `{change}`"));
            claude.compact_session(session, "Propose")?;
            if let Err(error) = claude.rename_session(session, &change) {
                self.ui
                    .warn(&format!("Could not rename planning session: {error}"));
            }
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
        let (session, is_new) = planning_session(state);
        persist_state(repo, state, &self.ui)?;
        let task = format!(
            "Create the proposal milestone Git commit for OpenSpec change `{change}`. Inspect Git status and diffs. Commit only the proposal artifacts for this change and directly related canonical OpenSpec specification updates, with commit message exactly `openspec: propose {change}`. Preserve all unrelated work. Never reset, stash, restore, discard, amend, or rewrite existing history. If the relevant proposal work is already committed and nothing remains to commit, confirm that and report READY."
        );
        let result = claude.invoke(
            session_mode(session, is_new, &format!("{change}-proposal-commit")),
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
        let result = invoke_fresh(
            claude,
            &format!("{change}-apply"),
            &stage_prompt(&commands.apply, &subject, StageProtocol::Worker),
            "Claude is applying the OpenSpec change",
            StageProtocol::Worker,
        )?;
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
        let result = invoke_fresh(
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
        )?;
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
                state.stage = Stage::Repair;
                persist_state(repo, state, &self.ui)?;
                if state.verify_retries > self.cli.max_verify_retries {
                    bail!(
                        "verification reached the configured retry limit of {}; resume with `--direction`, or raise `--max-verify-retries`",
                        self.cli.max_verify_retries
                    );
                }
                self.ui.warn(&format!(
                    "Verification requested repair {}/{}",
                    state.verify_retries, self.cli.max_verify_retries
                ));
                Ok(())
            }
            StageSignal::Blocked => blocked("verify", &result.text),
            StageSignal::Ready | StageSignal::Done | StageSignal::TooLarge => {
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
            bail!(
                "verification retry limit reached; resume with `--direction` or raise `--max-verify-retries`"
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
        let result = invoke_fresh(
            claude,
            &format!("{change}-repair-{}", state.verify_retries),
            &stage_prompt(&commands.apply, &subject, StageProtocol::Worker),
            "Claude is repairing the implementation",
            StageProtocol::Worker,
        )?;
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
        commands: &SkillCommands,
    ) -> Result<()> {
        self.ui
            .warn("DRY RUN — no workflow state or subprocesses will be created");
        let dry_state = if self.cli.loop_workflow {
            RunState::new_campaign(
                self.cli.request.clone(),
                ChangeSnapshot::default(),
                self.cli.max_iterations,
            )
        } else {
            RunState::new(self.cli.request.clone(), ChangeSnapshot::default())
        };
        let prompt = stage_prompt(
            &commands.explore,
            &campaign_subject(&dry_state),
            StageProtocol::Worker,
        );
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
            Some(StageProtocol::Worker.json_schema()),
        );
        self.ui.info(&format!("Explore: {}", command.display()));
        self.ui
            .info("Hard compact the planning session after Explore");
        self.ui.info(&format!(
            "Propose in same session: {} (READY, DONE, TOO_LARGE, or BLOCKED)",
            commands.propose
        ));
        self.ui
            .info("Hard compact the planning session after Propose");
        self.ui
            .info("Ask Claude to create proposal milestone commit");
        self.ui.info(&format!("Apply: {}", commands.apply));
        self.ui.info(&format!(
            "Verify/repair: {} / {}",
            commands.verify, commands.apply
        ));
        self.ui.info(&format!("Archive: {}", commands.archive));
        self.ui
            .info("Ask Claude to create completion milestone commit");
        if self.cli.loop_workflow {
            let limit = self.cli.max_iterations.map_or_else(
                || "until Propose returns DONE".to_owned(),
                |maximum| format!("until DONE or {maximum} completed iterations"),
            );
            self.ui.info(&format!(
                "Campaign: repeat the complete workflow {limit}; stop on any error, TOO_LARGE, or BLOCKED"
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
            "Claude launcher: program=`{}`, prefix args={:?}, model={:?}, auto-compact window={:?}, auto-compact percent={:?}, max output tokens={:?}, permission mode=`{}`, stream filter={:?}",
            launcher.program,
            launcher.prefix_args,
            launcher.model,
            launcher.auto_compact_window,
            launcher.auto_compact_percent,
            launcher.max_output_tokens,
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

fn proposal_postcondition_repair(subject: &str) -> String {
    format!(
        "{subject}\n\nPOSTCONDITION REPAIR: The preceding Propose turn returned READY, but a deterministic `openspec list --json` check found no new or modified active change. Continue in this same planning session and perform the proposal workflow now; do not merely describe what should be proposed. Use the completed exploration and repository's durable slice plan to select the intended next slice. Return READY only after the active OpenSpec change exists and every artifact required before implementation is complete. If no change is warranted, return DONE. If the selected slice exceeds one reliable worker change, return TOO_LARGE with an ordered decomposition. Return BLOCKED only for a genuine external decision."
    )
}

fn campaign_subject(state: &RunState) -> String {
    match &state.campaign {
        Some(campaign) => format!(
            "{}\n\nThis is campaign iteration {}. Select and pursue exactly one coherent, bounded remaining slice toward the objective. Use the repository and archived OpenSpec history as durable evidence of earlier iterations. If the overall objective is already satisfied and no meaningful slice remains, carry that conclusion into Propose so it can return DONE without creating artifacts.",
            state.request, campaign.iteration
        ),
        None => state.request.clone(),
    }
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
        2 => migrate_v2(value),
        other => bail!(
            "unsupported checkpoint schema {other}; use `--forget` to leave repository state untouched and start over"
        ),
    }
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
    })
}

fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
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
    fn proposal_postcondition_repair_preserves_the_campaign_rubric() {
        let prompt = proposal_postcondition_repair("next slice");
        assert!(prompt.starts_with("next slice"));
        assert!(prompt.contains("same planning session"));
        assert!(prompt.contains("do not merely describe"));
        assert!(prompt.contains("Return READY only after"));
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
}
