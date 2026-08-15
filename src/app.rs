use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    claude::{
        ClaudeClient, ClaudeLauncher, ClaudeOutputFormat, SessionMode, SkillCommands, StageSignal,
        build_claude_command, build_interactive_claude_command, stage_prompt,
    },
    cli::Cli,
    git::{
        RepoSnapshot, abort_paths, assert_paths_unchanged, changed_baseline_paths, commit_parent,
        commit_paths, commit_subject, commits_between, current_head, ensure_head,
        ensure_no_baseline_overlap, final_candidate_paths, is_ancestor, metadata_dir,
        repository_root, snapshot as git_snapshot,
    },
    openspec::{identify_change, snapshot as openspec_snapshot},
    process::{ProcessRunner, prerequisite_exists},
    state::{Event, Stage, WorkflowState},
    ui::Ui,
};

const TOTAL_STAGES: usize = 7;
const METADATA_SCHEMA_VERSION: u32 = 2;

pub struct App<U: Ui> {
    cli: Cli,
    ui: U,
}

#[derive(Debug, Serialize, Deserialize)]
struct RunMetadata {
    schema_version: u32,
    request: String,
    change: Option<String>,
    stage: Stage,
    verify_retries: u32,
    proposal_commit: Option<String>,
    final_commit: Option<String>,
    #[serde(default)]
    expected_head: Option<String>,
    #[serde(default)]
    adopted_commits: Vec<String>,
    #[serde(default)]
    tolerated_dirty_paths: Vec<String>,
    baseline: RepoSnapshot,
    before_changes: crate::openspec::ChangeSnapshot,
    pending_repair: Option<String>,
    sessions: Vec<SessionRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionRecord {
    stage: String,
    id: Uuid,
    name: String,
}

impl<U: Ui> App<U> {
    pub fn new(cli: Cli, ui: U) -> Self {
        Self { cli, ui }
    }

    pub fn run(self) -> Result<()> {
        let requested_repo = self.cli.repo.canonicalize().with_context(|| {
            format!(
                "repository path `{}` does not exist",
                self.cli.repo.display()
            )
        })?;
        if self.cli.abort {
            prerequisite_exists("git", &requested_repo, &self.ui)?;
            let repo = repository_root(&requested_repo, &self.ui)?;
            self.ui.banner(&repo.display().to_string());
            self.ui
                .debug(&format!("abort repository: {}", repo.display()));
            if let Some(path) = &self.cli.config_path {
                self.ui.info(&format!("Using config `{}`", path.display()));
            }
            return self.abort_run(&repo);
        }
        let launcher =
            ClaudeLauncher::parse(&self.cli.claude_command, self.cli.claude_model.clone())?;
        if launcher.program != "claude" {
            prerequisite_exists(&launcher.program, &requested_repo, &self.ui)?;
        }
        prerequisite_exists("claude", &requested_repo, &self.ui)?;

        if self.cli.interactive {
            return self.run_interactive(&requested_repo, &launcher);
        }

        for prerequisite in ["openspec", "git"] {
            prerequisite_exists(prerequisite, &requested_repo, &self.ui)?;
        }
        let repo = repository_root(&requested_repo, &self.ui)?;
        if !repo.join("openspec/config.yaml").is_file() {
            bail!(
                "no OpenSpec setup found at `{}`; expected `openspec/config.yaml` (run `openspec init` first)",
                repo.display()
            );
        }

        self.ui.banner(&repo.display().to_string());
        self.debug_configuration(&repo, &launcher);
        if let Some(path) = &self.cli.config_path {
            self.ui.info(&format!("Using config `{}`", path.display()));
        }
        if !self.cli.resume {
            ensure_no_unfinished_run(&repo, &self.ui)?;
        }
        let commands = SkillCommands::discover(&repo, &self.cli)?;
        self.ui.debug(&format!(
            "workflow commands: explore=`{}`, propose=`{}`, apply=`{}`, verify=`{}`, archive=`{}`",
            commands.explore, commands.propose, commands.apply, commands.verify, commands.archive
        ));
        if self.cli.resume {
            return self.resume_run(&repo, &commands, &launcher);
        }
        let before_changes = openspec_snapshot(&repo, &self.ui)?;
        let baseline = git_snapshot(&repo, &self.ui)?;

        if self.cli.dry_run {
            return self.print_dry_run(&repo, &commands, &launcher);
        }

        let state = WorkflowState::new(self.cli.max_verify_retries);
        let metadata = RunMetadata {
            schema_version: METADATA_SCHEMA_VERSION,
            request: self.cli.request.clone(),
            change: None,
            stage: state.stage,
            verify_retries: state.verify_retries,
            proposal_commit: None,
            final_commit: None,
            expected_head: baseline.head.clone(),
            adopted_commits: Vec::new(),
            tolerated_dirty_paths: Vec::new(),
            baseline,
            before_changes,
            pending_repair: None,
            sessions: Vec::new(),
        };
        persist_metadata(&repo, &metadata, &self.ui)?;
        let claude = ClaudeClient::new(
            &repo,
            &launcher,
            &self.cli.permission_mode,
            self.cli.stream_claude,
            &self.ui,
        );
        self.continue_planning(&repo, &commands, &claude, state, metadata)
    }

    fn abort_run(&self, repo: &Path) -> Result<()> {
        let metadata = load_metadata(repo, &self.ui)?;
        validate_metadata_schema(&metadata)?;
        if metadata.proposal_commit.is_some()
            || !matches!(
                metadata.stage,
                Stage::Explore | Stage::Propose | Stage::ProposalCommit
            )
        {
            bail!(
                "BLOCKED: --abort only cleans an unfinished planning transaction before its proposal milestone commit; this run is at {:?}",
                metadata.stage
            );
        }

        let current = git_snapshot(repo, &self.ui)?;
        assert_paths_unchanged(&metadata.baseline, &current)?;
        if current.head != metadata.baseline.head {
            bail!(
                "BLOCKED: Git HEAD changed during the planning transaction; abort will not rewrite unknown commits"
            );
        }
        let paths = run_owned_paths(&metadata.baseline, &current);
        let label = metadata.change.as_deref().unwrap_or("planning transaction");
        self.ui.warn(&format!(
            "Aborting `{label}` at {:?}; {} run-owned path(s) will be restored or removed",
            metadata.stage,
            paths.len()
        ));
        for path in &paths {
            self.ui.info(&format!("Abort path: {path}"));
        }
        if self.cli.dry_run {
            self.ui
                .warn("DRY RUN — no files or metadata will be changed");
            return Ok(());
        }

        abort_paths(repo, &paths, &self.ui)?;
        let restored = git_snapshot(repo, &self.ui)?;
        if restored.head != metadata.baseline.head || restored.dirty != metadata.baseline.dirty {
            bail!(
                "BLOCKED: abort cleanup did not reproduce the original Git baseline exactly; checkpoint metadata was retained"
            );
        }
        remove_metadata(repo, &self.ui)?;
        self.ui.success(&format!(
            "ABORTED `{label}` — original working-tree baseline restored"
        ));
        Ok(())
    }

    fn continue_planning(
        &self,
        repo: &Path,
        commands: &SkillCommands,
        claude: &ClaudeClient<'_, U>,
        mut state: WorkflowState,
        mut metadata: RunMetadata,
    ) -> Result<()> {
        if state.stage == Stage::Explore {
            self.ui.stage(1, TOTAL_STAGES, "Explore");
            let exploration_session = Uuid::new_v4();
            metadata.sessions.push(SessionRecord {
                stage: "explore+propose".to_owned(),
                id: exploration_session,
                name: "ospx-build-planning".to_owned(),
            });
            persist_metadata(repo, &metadata, &self.ui)?;
            let explore_prompt =
                stage_prompt(&commands.explore, &metadata.request, "READY or BLOCKED");
            let explore = claude.invoke(
                SessionMode::New {
                    id: exploration_session,
                    name: Some("ospx-build-planning".to_owned()),
                },
                &explore_prompt,
                "Claude is exploring the change",
            )?;
            require_ready(&mut state, explore.signal, "explore", &explore.text)?;
            sync_metadata(&mut metadata, &state);
            persist_metadata(repo, &metadata, &self.ui)?;
        }

        if state.stage == Stage::Propose {
            self.ui.stage(2, TOTAL_STAGES, "Propose");
            let exploration_session = metadata
                .sessions
                .iter()
                .rev()
                .find(|session| session.stage == "explore+propose")
                .map(|session| session.id)
                .context("BLOCKED: planning checkpoint contains no exploration session")?;
            let propose_subject = format!(
                "{}\n\nUse the conclusions from the preceding exploration in this same Claude session. Preserve any correct partial proposal artifacts already present from an interrupted attempt.",
                metadata.request
            );
            let propose_prompt =
                stage_prompt(&commands.propose, &propose_subject, "READY or BLOCKED");
            let propose = claude.invoke(
                SessionMode::Resume {
                    id: exploration_session,
                },
                &propose_prompt,
                "Claude is creating OpenSpec artifacts",
            )?;
            require_ready(&mut state, propose.signal, "propose", &propose.text)?;

            let after_changes = openspec_snapshot(repo, &self.ui)?;
            let change = identify_change(&metadata.before_changes, &after_changes)?;
            validate_change_name(&change)?;
            self.ui
                .info(&format!("Selected OpenSpec change `{change}`"));
            metadata.change = Some(change.clone());

            if let Err(error) = claude.rename_session(exploration_session, &change) {
                self.ui.warn(&format!(
                    "Could not rename the planning session ({error}); the session mapping is retained locally"
                ));
            } else if let Some(session) = metadata
                .sessions
                .iter_mut()
                .rev()
                .find(|session| session.id == exploration_session)
            {
                session.name = change;
            }
            sync_metadata(&mut metadata, &state);
            persist_metadata(repo, &metadata, &self.ui)?;
        }

        if state.stage == Stage::ProposalCommit {
            self.ui.stage(3, TOTAL_STAGES, "Proposal milestone");
            let change = metadata
                .change
                .clone()
                .context("BLOCKED: proposal checkpoint contains no selected change")?;
            let proposal_path = format!("openspec/changes/{change}");
            ensure_no_baseline_overlap(&metadata.baseline, &proposal_path)?;
            ensure_baseline_policy(
                repo,
                &mut metadata,
                &self.ui,
                "before Proposal milestone",
                self.cli.strict,
            )?;
            let proposal_state = git_snapshot(repo, &self.ui)?;
            let (proposal_paths, ignored_paths) =
                proposal_candidate_paths(&metadata.baseline, &proposal_state, &proposal_path)?;
            if !ignored_paths.is_empty() {
                if self.cli.strict {
                    bail!(
                        "BLOCKED: proposal stage changed files outside the selected change and canonical OpenSpec specs: {}",
                        ignored_paths.join(", ")
                    );
                }
                self.ui.warn(&format!(
                    "Leaving unrelated proposal-stage path(s) out of the proposal commit: {}",
                    ignored_paths.join(", ")
                ));
            }
            let proposal_commit = commit_paths(
                repo,
                &proposal_paths,
                &format!("openspec: propose {change}"),
                &self.ui,
            )?;
            self.ui.success(&format!(
                "Proposal committed as {}",
                short_hash(&proposal_commit)
            ));
            state.advance(Event::Committed)?;
            metadata.proposal_commit = Some(proposal_commit);
            metadata.expected_head = metadata.proposal_commit.clone();
            sync_metadata(&mut metadata, &state);
            ensure_baseline_policy(
                repo,
                &mut metadata,
                &self.ui,
                "after Proposal milestone",
                self.cli.strict,
            )?;
            persist_metadata(repo, &metadata, &self.ui)?;
        }

        self.continue_run(repo, commands, claude, state, metadata)
    }

    fn resume_run(
        &self,
        repo: &Path,
        commands: &SkillCommands,
        launcher: &ClaudeLauncher,
    ) -> Result<()> {
        let mut metadata = load_metadata(repo, &self.ui)?;
        validate_metadata_schema(&metadata)?;
        if let Some(change) = &metadata.change {
            validate_change_name(change)?;
        }
        if !self.cli.request.is_empty() && self.cli.request != metadata.request {
            bail!(
                "BLOCKED: the supplied request does not match the resumable request stored for `{}`",
                metadata.change.as_deref().unwrap_or("planning run")
            );
        }

        let mut state = WorkflowState::resume(
            metadata.stage,
            metadata.verify_retries,
            self.cli.max_verify_retries,
        )?;
        if state.stage == Stage::Blocked {
            bail!(
                "BLOCKED: the last run recorded a genuine blocker; start a new run after resolving it"
            );
        }
        if state.stage == Stage::Complete {
            let change = metadata
                .change
                .as_deref()
                .context("BLOCKED: completed metadata contains no change name")?;
            let final_commit = metadata
                .final_commit
                .as_deref()
                .unwrap_or("unknown final commit");
            self.ui.success(&format!(
                "Already COMPLETE `{}` — final {}",
                change,
                short_hash(final_commit)
            ));
            return Ok(());
        }

        let current = git_snapshot(repo, &self.ui)?;
        if matches!(
            state.stage,
            Stage::Explore | Stage::Propose | Stage::ProposalCommit
        ) {
            ensure_baseline_policy(
                repo,
                &mut metadata,
                &self.ui,
                "while resuming planning",
                self.cli.strict,
            )?;
            if current.head != metadata.baseline.head {
                bail!(
                    "BLOCKED: Git HEAD changed during the unfinished planning transaction; refusing to adopt unknown commits"
                );
            }
            self.ui.info(&format!(
                "Resuming planning transaction at {:?}; completed planning phases will not rerun",
                state.stage
            ));
            if self.cli.dry_run {
                return self.print_resume_dry_run(&metadata, &state, commands);
            }
            let claude = ClaudeClient::new(
                repo,
                launcher,
                &self.cli.permission_mode,
                self.cli.stream_claude,
                &self.ui,
            );
            return self.continue_planning(repo, commands, &claude, state, metadata);
        }

        let change = metadata
            .change
            .clone()
            .context("BLOCKED: resume metadata has no selected change")?;
        let proposal_commit = metadata
            .proposal_commit
            .clone()
            .context("BLOCKED: resume metadata has no proposal milestone commit")?;
        if metadata.expected_head.is_none() {
            metadata.expected_head = Some(proposal_commit);
        }
        let expected_head = metadata
            .expected_head
            .clone()
            .expect("initialized from proposal commit");
        if state.stage == Stage::FinalCommit
            && current
                .head
                .as_deref()
                .is_some_and(|head| head != expected_head)
        {
            let head = current.head.as_deref().expect("checked as present");
            let expected_subject = format!("openspec: complete {change}");
            if commit_parent(repo, head, &self.ui)? == expected_head
                && commit_subject(repo, head, &self.ui)? == expected_subject
            {
                self.ui.warn(&format!(
                    "Recovered completion commit {} written just before interruption",
                    short_hash(head)
                ));
                state.advance(Event::Committed)?;
                metadata.final_commit = Some(head.to_owned());
                metadata.expected_head = Some(head.to_owned());
                sync_metadata(&mut metadata, &state);
                persist_metadata(repo, &metadata, &self.ui)?;
            } else {
                ensure_stage_repository(
                    repo,
                    &mut metadata,
                    &self.ui,
                    "while resuming Final Commit",
                    self.cli.strict,
                )?;
            }
        } else {
            ensure_stage_repository(
                repo,
                &mut metadata,
                &self.ui,
                "while resuming",
                self.cli.strict,
            )?;
        }

        if matches!(
            state.stage,
            Stage::Apply | Stage::Verify | Stage::Repair | Stage::Archive
        ) {
            let changes = openspec_snapshot(repo, &self.ui)?;
            if !changes.changes.contains_key(&change) {
                if state.stage == Stage::Archive {
                    self.ui.warn(
                        "The active change is absent; treating the interrupted archive as complete",
                    );
                    state.advance(Event::Ready)?;
                    sync_metadata(&mut metadata, &state);
                    persist_metadata(repo, &metadata, &self.ui)?;
                } else {
                    bail!(
                        "BLOCKED: OpenSpec change `{}` is no longer active; refusing to resume {:?}",
                        change,
                        state.stage
                    );
                }
            }
        }

        self.ui.info(&format!(
            "Resuming `{}` at {:?}; Explore, Propose, and completed milestones will not rerun",
            change, state.stage
        ));
        if self.cli.dry_run {
            return self.print_resume_dry_run(&metadata, &state, commands);
        }

        let claude = ClaudeClient::new(
            repo,
            launcher,
            &self.cli.permission_mode,
            self.cli.stream_claude,
            &self.ui,
        );
        self.continue_run(repo, commands, &claude, state, metadata)
    }

    fn continue_run(
        &self,
        repo: &Path,
        commands: &SkillCommands,
        claude: &ClaudeClient<'_, U>,
        mut state: WorkflowState,
        mut metadata: RunMetadata,
    ) -> Result<()> {
        let change = metadata
            .change
            .clone()
            .context("BLOCKED: implementation checkpoint contains no selected change")?;

        if state.stage == Stage::Apply {
            self.ui.stage(4, TOTAL_STAGES, "Apply");
            ensure_stage_repository(
                repo,
                &mut metadata,
                &self.ui,
                "before Apply",
                self.cli.strict,
            )?;
            let apply_session = Uuid::new_v4();
            metadata.sessions.push(SessionRecord {
                stage: "apply".to_owned(),
                id: apply_session,
                name: format!("{change}-apply"),
            });
            persist_metadata(repo, &metadata, &self.ui)?;
            let apply_prompt = stage_prompt(
                &commands.apply,
                &format!(
                    "{change}\n\nImplement or continue implementing this OpenSpec change completely. Preserve correct partial work already present in the repository. Run appropriate project checks. Do not archive the change."
                ),
                "READY or BLOCKED",
            );
            let apply = claude.invoke(
                SessionMode::New {
                    id: apply_session,
                    name: Some(format!("{change}-apply")),
                },
                &apply_prompt,
                "Claude is applying the OpenSpec change",
            )?;
            ensure_stage_repository(
                repo,
                &mut metadata,
                &self.ui,
                "after Apply",
                self.cli.strict,
            )?;
            require_ready(&mut state, apply.signal, "apply", &apply.text)?;
            sync_metadata(&mut metadata, &state);
            persist_metadata(repo, &metadata, &self.ui)?;
        }

        if matches!(state.stage, Stage::Verify | Stage::Repair) {
            self.ui.stage(5, TOTAL_STAGES, "Verify");
        }
        while matches!(state.stage, Stage::Verify | Stage::Repair) {
            if state.stage == Stage::Repair {
                ensure_stage_repository(
                    repo,
                    &mut metadata,
                    &self.ui,
                    "before Repair",
                    self.cli.strict,
                )?;
                let finding = metadata.pending_repair.clone().context(
                    "BLOCKED: resume metadata says Repair but contains no verification finding",
                )?;
                let repair_session = Uuid::new_v4();
                metadata.sessions.push(SessionRecord {
                    stage: format!("repair-{}", state.verify_retries),
                    id: repair_session,
                    name: format!("{change}-repair-{}", state.verify_retries),
                });
                persist_metadata(repo, &metadata, &self.ui)?;
                let repair_prompt = stage_prompt(
                    &commands.apply,
                    &format!(
                        "{change}\n\nA verification session found the following correctable issue:\n\n{finding}\n\nRepair or continue repairing the implementation and tests without broadening the approved OpenSpec scope. Preserve correct partial work already present. Re-run relevant checks. Do not archive or commit."
                    ),
                    "READY or BLOCKED",
                );
                let repair = claude.invoke(
                    SessionMode::New {
                        id: repair_session,
                        name: Some(format!("{change}-repair-{}", state.verify_retries)),
                    },
                    &repair_prompt,
                    "Claude is repairing verification findings",
                )?;
                ensure_stage_repository(
                    repo,
                    &mut metadata,
                    &self.ui,
                    "after Repair",
                    self.cli.strict,
                )?;
                require_ready(&mut state, repair.signal, "repair", &repair.text)?;
                metadata.pending_repair = None;
                sync_metadata(&mut metadata, &state);
                persist_metadata(repo, &metadata, &self.ui)?;
                continue;
            }

            let verify_number = state.verify_retries + 1;
            ensure_stage_repository(
                repo,
                &mut metadata,
                &self.ui,
                "before Verify",
                self.cli.strict,
            )?;
            let verify_session = Uuid::new_v4();
            metadata.sessions.push(SessionRecord {
                stage: format!("verify-{verify_number}"),
                id: verify_session,
                name: format!("{change}-verify-{verify_number}"),
            });
            persist_metadata(repo, &metadata, &self.ui)?;
            let verify_prompt = stage_prompt(
                &commands.verify,
                &format!(
                    "{change}\n\nVerify implementation against the OpenSpec artifacts and run the relevant checks. Report RETRY only for a concrete, correctable implementation issue and explain the required repair."
                ),
                "VERIFIED, RETRY, or BLOCKED",
            );
            let verify = claude.invoke(
                SessionMode::New {
                    id: verify_session,
                    name: Some(format!("{change}-verify-{verify_number}")),
                },
                &verify_prompt,
                "Claude is verifying specification compliance",
            )?;
            ensure_stage_repository(
                repo,
                &mut metadata,
                &self.ui,
                "after Verify",
                self.cli.strict,
            )?;
            match verify.signal {
                StageSignal::Verified => {
                    state.advance(Event::Verified)?;
                    metadata.pending_repair = None;
                    self.ui.success("Verification passed");
                }
                StageSignal::Blocked => return blocked("verify", &verify.text),
                StageSignal::Retry => {
                    state.advance(Event::Retry)?;
                    metadata.pending_repair = Some(verify.text);
                    self.ui.warn(&format!(
                        "Verification requested repair {}/{}",
                        state.verify_retries, state.max_verify_retries
                    ));
                }
                StageSignal::Ready => {
                    bail!(
                        "verify returned READY rather than VERIFIED, RETRY, or BLOCKED; rerun with --verbose to inspect the response"
                    );
                }
            }
            sync_metadata(&mut metadata, &state);
            persist_metadata(repo, &metadata, &self.ui)?;
        }

        if state.stage == Stage::Archive {
            self.ui.stage(6, TOTAL_STAGES, "Archive");
            ensure_stage_repository(
                repo,
                &mut metadata,
                &self.ui,
                "before Archive",
                self.cli.strict,
            )?;
            let archive_session = Uuid::new_v4();
            metadata.sessions.push(SessionRecord {
                stage: "archive".to_owned(),
                id: archive_session,
                name: format!("{change}-archive"),
            });
            persist_metadata(repo, &metadata, &self.ui)?;
            let archive_prompt = stage_prompt(
                &commands.archive,
                &format!(
                    "{change}\n\nArchive this successfully verified OpenSpec change, including normal specification synchronization. Do not commit."
                ),
                "READY or BLOCKED",
            );
            let archive = claude.invoke(
                SessionMode::New {
                    id: archive_session,
                    name: Some(format!("{change}-archive")),
                },
                &archive_prompt,
                "Claude is archiving the OpenSpec change",
            )?;
            ensure_stage_repository(
                repo,
                &mut metadata,
                &self.ui,
                "after Archive",
                self.cli.strict,
            )?;
            require_ready(&mut state, archive.signal, "archive", &archive.text)?;
            sync_metadata(&mut metadata, &state);
            persist_metadata(repo, &metadata, &self.ui)?;
        }

        if state.stage == Stage::FinalCommit {
            self.ui.stage(7, TOTAL_STAGES, "Completion milestone");
            ensure_stage_repository(
                repo,
                &mut metadata,
                &self.ui,
                "before Completion milestone",
                self.cli.strict,
            )?;
            let final_state = git_snapshot(repo, &self.ui)?;
            let final_paths = final_candidate_paths(&metadata.baseline, &final_state)?;
            let final_commit = if final_paths.is_empty() && !self.cli.strict {
                let head = metadata
                    .expected_head
                    .clone()
                    .context("BLOCKED: completion checkpoint contains no expected Git HEAD")?;
                self.ui.warn(&format!(
                    "No uncommitted completion paths remain; using reconciled HEAD {} as the completion milestone",
                    short_hash(&head)
                ));
                head
            } else {
                commit_paths(
                    repo,
                    &final_paths,
                    &format!("openspec: complete {change}"),
                    &self.ui,
                )?
            };
            state.advance(Event::Committed)?;
            metadata.final_commit = Some(final_commit.clone());
            metadata.expected_head = Some(final_commit);
            sync_metadata(&mut metadata, &state);
            persist_metadata(repo, &metadata, &self.ui)?;
        }

        if state.stage != Stage::Complete {
            bail!(
                "resume reached unsupported workflow stage {:?}",
                state.stage
            );
        }
        let proposal_commit = metadata
            .proposal_commit
            .as_deref()
            .unwrap_or("unknown proposal commit");
        let final_commit = metadata
            .final_commit
            .as_deref()
            .unwrap_or("unknown final commit");
        self.ui.success(&format!(
            "COMPLETE `{change}` — proposal {}, final {}",
            short_hash(proposal_commit),
            short_hash(final_commit)
        ));
        Ok(())
    }

    fn print_resume_dry_run(
        &self,
        metadata: &RunMetadata,
        state: &WorkflowState,
        commands: &SkillCommands,
    ) -> Result<()> {
        let change = metadata.change.as_deref().unwrap_or("planning transaction");
        self.ui.warn("DRY RUN — resume will not execute any stage");
        self.ui.info(&format!(
            "Resume `{}` from {:?} with {} verification retries already used",
            change, state.stage, state.verify_retries
        ));
        if state.stage == Stage::Explore {
            self.ui
                .info(&format!("Fresh Explore session: {}", commands.explore));
        }
        if matches!(state.stage, Stage::Explore | Stage::Propose) {
            self.ui.info(&format!(
                "Continue Propose in the planning session: {}",
                commands.propose
            ));
        }
        if matches!(
            state.stage,
            Stage::Explore | Stage::Propose | Stage::ProposalCommit
        ) {
            self.ui
                .info("Validate and create the scoped proposal milestone commit");
        }
        if state.stage == Stage::Apply {
            self.ui.info(&format!(
                "Fresh Apply session: {} {}",
                commands.apply, change
            ));
        }
        if matches!(state.stage, Stage::Apply | Stage::Verify | Stage::Repair) {
            self.ui.info(&format!(
                "Continue bounded Repair/Verify loop using {} and {}",
                commands.apply, commands.verify
            ));
        }
        if matches!(
            state.stage,
            Stage::Apply | Stage::Verify | Stage::Repair | Stage::Archive
        ) {
            self.ui
                .info(&format!("Archive after verification: {}", commands.archive));
        }
        self.ui
            .info("Create the final scoped milestone commit when Archive is complete");
        Ok(())
    }

    fn run_interactive(&self, repo: &Path, launcher: &ClaudeLauncher) -> Result<()> {
        self.ui.banner(&repo.display().to_string());
        self.debug_configuration(repo, launcher);
        if let Some(path) = &self.cli.config_path {
            self.ui.info(&format!("Using config `{}`", path.display()));
        }

        let initial_prompt = (!self.cli.request.is_empty()).then_some(self.cli.request.as_str());
        if let Some(prompt) = initial_prompt {
            self.ui.debug_prompt("Interactive initial prompt", prompt);
        }
        let command = build_interactive_claude_command(
            repo,
            launcher,
            &self.cli.permission_mode,
            &self.cli.interactive_args,
            initial_prompt,
        );
        self.ui
            .info(&format!("Interactive command: {}", command.display()));

        if self.cli.dry_run {
            self.ui
                .warn("DRY RUN — interactive Claude will not be launched");
            return Ok(());
        }

        ProcessRunner::new(&self.ui).run_interactive(&command)
    }

    fn debug_configuration(&self, repo: &Path, launcher: &ClaudeLauncher) {
        self.ui
            .debug("complete prompts are visible and may contain repository content");
        self.ui.debug(&format!("repository: {}", repo.display()));
        self.ui.debug(&format!(
            "Claude launcher: program=`{}`, prefix args={:?}, model={:?}, permission mode=`{}`, stream filter={:?}, policy={}",
            launcher.program,
            launcher.prefix_args,
            launcher.model,
            self.cli.permission_mode,
            self.cli.stream_claude,
            if self.cli.strict { "strict" } else { "pragmatic" }
        ));
        match &self.cli.config_path {
            Some(path) => self
                .ui
                .debug(&format!("resolved config file: {}", path.display())),
            None => self.ui.debug("resolved config file: none"),
        }
    }

    fn print_dry_run(
        &self,
        repo: &Path,
        commands: &SkillCommands,
        launcher: &ClaudeLauncher,
    ) -> Result<()> {
        self.ui
            .warn("DRY RUN — no Claude stage or git mutation will execute");
        let planning = Uuid::nil();
        let explore_prompt = stage_prompt(&commands.explore, &self.cli.request, "READY or BLOCKED");
        let propose_prompt = stage_prompt(&commands.propose, &self.cli.request, "READY or BLOCKED");
        let output_format = if self.cli.stream_claude.is_some() {
            ClaudeOutputFormat::StreamJson
        } else {
            ClaudeOutputFormat::Json
        };
        self.ui
            .debug_prompt("Explore dry-run prompt", &explore_prompt);
        self.ui
            .debug_prompt("Propose dry-run prompt", &propose_prompt);
        let explore = build_claude_command(
            repo,
            launcher,
            &self.cli.permission_mode,
            &SessionMode::New {
                id: planning,
                name: Some("ospx-build-planning".to_owned()),
            },
            &explore_prompt,
            output_format,
        );
        let propose = build_claude_command(
            repo,
            launcher,
            &self.cli.permission_mode,
            &SessionMode::Resume { id: planning },
            &propose_prompt,
            output_format,
        );
        self.ui.info(&format!("1. Explore: {}", explore.display()));
        self.ui.info(&format!("2. Propose: {}", propose.display()));
        self.ui.info("3. Query `openspec list --json`, identify one new/modified change, rename planning session");
        self.ui.info(
            "4. Commit the selected change and canonical `openspec/specs/**` updates as `openspec: propose <change>`",
        );
        self.ui.info(&format!(
            "5. Fresh Claude session: {} <change>",
            commands.apply
        ));
        self.ui.info(&format!(
            "6. Fresh Claude session: {} <change>; repair/apply and retry up to {} time(s)",
            commands.verify, self.cli.max_verify_retries
        ));
        self.ui.info(&format!(
            "7. Fresh Claude session: {} <change>",
            commands.archive
        ));
        self.ui
            .info("8. Commit only run-owned paths as `openspec: complete <change>`");
        Ok(())
    }
}

fn require_ready(
    state: &mut WorkflowState,
    signal: StageSignal,
    stage: &str,
    response: &str,
) -> Result<()> {
    match signal {
        StageSignal::Ready => {
            state.advance(Event::Ready)?;
            Ok(())
        }
        StageSignal::Blocked => {
            state.advance(Event::Blocked)?;
            blocked(stage, response)
        }
        other => bail!("{stage} returned unexpected terminal status {other:?}"),
    }
}

fn blocked<T>(stage: &str, response: &str) -> Result<T> {
    bail!("BLOCKED during {stage}:\n{}", response.trim())
}

fn validate_change_name(change: &str) -> Result<()> {
    let valid = !change.is_empty()
        && change
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !change.starts_with('-')
        && !change.ends_with('-');
    if !valid {
        bail!(
            "OpenSpec returned unsafe or unexpected change name `{change}`; expected lowercase kebab-case"
        );
    }
    Ok(())
}

fn proposal_candidate_paths(
    baseline: &RepoSnapshot,
    current: &RepoSnapshot,
    proposal_path: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    let change_prefix = format!("{proposal_path}/");
    let specs_path = "openspec/specs";
    let specs_prefix = format!("{specs_path}/");
    let changed: Vec<_> = current
        .dirty
        .keys()
        .filter(|path| !baseline.dirty.contains_key(*path))
        .cloned()
        .collect();
    let unexpected: Vec<_> = current
        .dirty
        .keys()
        .filter(|path| !baseline.dirty.contains_key(*path))
        .filter(|path| {
            path.as_str() != proposal_path
                && !path.starts_with(&change_prefix)
                && path.as_str() != specs_path
                && !path.starts_with(&specs_prefix)
        })
        .cloned()
        .collect();
    let allowed: Vec<_> = changed
        .into_iter()
        .filter(|path| !unexpected.contains(path))
        .collect();
    if !allowed
        .iter()
        .any(|path| path == proposal_path || path.starts_with(&change_prefix))
    {
        bail!("BLOCKED: proposal stage produced no git changes under `{proposal_path}`");
    }
    Ok((allowed, unexpected))
}

fn run_owned_paths(baseline: &RepoSnapshot, current: &RepoSnapshot) -> Vec<String> {
    current
        .dirty
        .keys()
        .filter(|path| !baseline.dirty.contains_key(*path))
        .cloned()
        .collect()
}

fn persist_metadata<U: Ui>(repo: &Path, metadata: &RunMetadata, ui: &U) -> Result<()> {
    let directory = metadata_dir(repo, ui)?;
    let path = directory.join("last-run.json");
    let temporary = directory.join("last-run.json.tmp");
    let json = serde_json::to_vec_pretty(metadata)?;
    fs::write(&temporary, json).with_context(|| {
        format!(
            "could not persist temporary run metadata `{}`",
            temporary.display()
        )
    })?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("could not persist run metadata `{}`", path.display()))
}

fn load_metadata<U: Ui>(repo: &Path, ui: &U) -> Result<RunMetadata> {
    let path = metadata_path(repo, ui)?;
    let json = fs::read_to_string(&path).with_context(|| {
        format!(
            "no resumable ospx-build metadata at `{}`; start a new run first",
            path.display()
        )
    })?;
    serde_json::from_str(&json)
        .with_context(|| format!("invalid resume metadata `{}`", path.display()))
}

fn ensure_no_unfinished_run<U: Ui>(repo: &Path, ui: &U) -> Result<()> {
    let path = metadata_path(repo, ui)?;
    if !path.exists() {
        return Ok(());
    }
    let metadata = load_metadata(repo, ui)?;
    validate_metadata_schema(&metadata)?;
    if metadata.stage != Stage::Complete {
        bail!(
            "BLOCKED: an unfinished ospx-build run is already recorded at {:?}; use `ospx-build --resume` or, before the proposal milestone, `ospx-build --abort`",
            metadata.stage
        );
    }
    Ok(())
}

fn validate_metadata_schema(metadata: &RunMetadata) -> Result<()> {
    if metadata.schema_version != METADATA_SCHEMA_VERSION {
        bail!(
            "BLOCKED: run metadata uses schema version {}, but this ospx-build expects {}",
            metadata.schema_version,
            METADATA_SCHEMA_VERSION
        );
    }
    Ok(())
}

fn metadata_path<U: Ui>(repo: &Path, ui: &U) -> Result<std::path::PathBuf> {
    Ok(metadata_dir(repo, ui)?.join("last-run.json"))
}

fn remove_metadata<U: Ui>(repo: &Path, ui: &U) -> Result<()> {
    let path = metadata_path(repo, ui)?;
    fs::remove_file(&path)
        .with_context(|| format!("could not remove run metadata `{}`", path.display()))
}

fn sync_metadata(metadata: &mut RunMetadata, state: &WorkflowState) {
    metadata.stage = state.stage;
    metadata.verify_retries = state.verify_retries;
}

fn ensure_stage_repository<U: Ui>(
    repo: &Path,
    metadata: &mut RunMetadata,
    ui: &U,
    boundary: &str,
    strict: bool,
) -> Result<()> {
    ensure_baseline_policy(repo, metadata, ui, boundary, strict)?;
    let expected = metadata
        .expected_head
        .clone()
        .or_else(|| metadata.proposal_commit.clone())
        .context("BLOCKED: implementation checkpoint contains no expected Git HEAD")?;
    let actual = current_head(repo, ui)?.context("BLOCKED: repository has no Git HEAD")?;
    if actual == expected {
        return Ok(());
    }
    if strict {
        return ensure_head(repo, &expected, ui).with_context(|| {
            format!(
                "BLOCKED: unexpected Git commit detected {boundary}; strict mode will not adopt it"
            )
        });
    }
    if !is_ancestor(repo, &expected, &actual, ui)? {
        bail!(
            "BLOCKED: Git history diverged {boundary} (expected descendant of {}, found {}); automatic reconciliation would require rewriting history",
            short_hash(&expected),
            short_hash(&actual)
        );
    }
    let commits = commits_between(repo, &expected, &actual, ui)?;
    ui.warn(&format!(
        "Adopting {} linear descendant commit(s) found {boundary}: {} → {}",
        commits.len(),
        short_hash(&expected),
        short_hash(&actual)
    ));
    for commit in commits {
        if !metadata.adopted_commits.contains(&commit) {
            metadata.adopted_commits.push(commit);
        }
    }
    metadata.expected_head = Some(actual);
    persist_metadata(repo, metadata, ui)
}

fn ensure_baseline_policy<U: Ui>(
    repo: &Path,
    metadata: &mut RunMetadata,
    ui: &U,
    boundary: &str,
    strict: bool,
) -> Result<()> {
    let changed_paths = changed_baseline_paths(repo, &metadata.baseline, ui)?;
    if strict && !changed_paths.is_empty() {
        bail!(
            "BLOCKED: pre-existing working-tree paths changed {boundary}: {}",
            changed_paths.join(", ")
        );
    }
    let newly_tolerated: Vec<_> = changed_paths
        .into_iter()
        .filter(|path| !metadata.tolerated_dirty_paths.contains(path))
        .collect();
    if !newly_tolerated.is_empty() {
        ui.warn(&format!(
            "Pre-run dirty path(s) changed {boundary}; leaving them out of orchestrator milestone commits: {}",
            newly_tolerated.join(", ")
        ));
        metadata.tolerated_dirty_paths.extend(newly_tolerated);
        persist_metadata(repo, metadata, ui)?;
    }
    Ok(())
}

fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, process::Command};

    use super::*;
    use crate::git::PathFingerprint;
    use crate::ui::TerminalUi;

    fn dirty_snapshot(paths: &[&str]) -> RepoSnapshot {
        RepoSnapshot {
            head: Some("head".to_owned()),
            dirty: paths
                .iter()
                .map(|path| {
                    (
                        (*path).to_owned(),
                        PathFingerprint {
                            status: "??".to_owned(),
                            worktree_hash: None,
                            index_hash: None,
                        },
                    )
                })
                .collect(),
        }
    }

    fn git_ok(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(repo)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {} failed", args.join(" "));
    }

    #[test]
    fn validates_safe_openspec_names() {
        assert!(validate_change_name("add-function-pointers").is_ok());
        assert!(validate_change_name("../escape").is_err());
        assert!(validate_change_name("Uppercase").is_err());
        assert!(validate_change_name("trailing-").is_err());
    }

    #[test]
    fn resume_metadata_round_trips_ownership_and_repair_state() {
        let metadata = RunMetadata {
            schema_version: METADATA_SCHEMA_VERSION,
            request: "build it".to_owned(),
            change: Some("build-it".to_owned()),
            stage: Stage::Repair,
            verify_retries: 1,
            proposal_commit: Some("abc123".to_owned()),
            final_commit: None,
            expected_head: Some("abc123".to_owned()),
            adopted_commits: vec!["def456".to_owned()],
            tolerated_dirty_paths: vec!["a.out".to_owned()],
            baseline: RepoSnapshot {
                head: Some("before".to_owned()),
                dirty: BTreeMap::new(),
            },
            before_changes: crate::openspec::ChangeSnapshot::default(),
            pending_repair: Some("fix the failing test".to_owned()),
            sessions: Vec::new(),
        };
        let json = serde_json::to_string(&metadata).unwrap();
        let parsed: RunMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.stage, Stage::Repair);
        assert_eq!(parsed.verify_retries, 1);
        assert_eq!(parsed.baseline.head.as_deref(), Some("before"));
        assert_eq!(parsed.expected_head.as_deref(), Some("abc123"));
        assert_eq!(parsed.adopted_commits, ["def456"]);
        assert_eq!(parsed.tolerated_dirty_paths, ["a.out"]);
        assert_eq!(
            parsed.pending_repair.as_deref(),
            Some("fix the failing test")
        );
    }

    #[test]
    fn existing_schema_two_checkpoint_defaults_reconciliation_fields() {
        let json = r#"{
            "schema_version": 2,
            "request": "continue slice m",
            "change": "slice-m",
            "stage": "apply",
            "verify_retries": 0,
            "proposal_commit": "abc123",
            "final_commit": null,
            "baseline": {"head": "before", "dirty": {}},
            "before_changes": {"changes": {}},
            "pending_repair": null,
            "sessions": []
        }"#;
        let parsed: RunMetadata = serde_json::from_str(json).unwrap();
        assert!(parsed.expected_head.is_none());
        assert!(parsed.adopted_commits.is_empty());
        assert!(parsed.tolerated_dirty_paths.is_empty());
    }

    #[test]
    fn pragmatic_policy_adopts_linear_commits_while_strict_policy_rejects_them() {
        let repo = std::env::temp_dir().join(format!("ospx-build-git-{}", Uuid::new_v4()));
        fs::create_dir_all(&repo).unwrap();
        git_ok(&repo, &["init", "--quiet"]);
        git_ok(&repo, &["config", "user.name", "ospx-build test"]);
        git_ok(
            &repo,
            &["config", "user.email", "ospx-build@example.invalid"],
        );
        fs::write(repo.join("initial.txt"), "initial\n").unwrap();
        git_ok(&repo, &["add", "initial.txt"]);
        git_ok(
            &repo,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "initial",
            ],
        );

        let ui = TerminalUi::new(false, false);
        fs::write(repo.join("a.out"), "old build output\n").unwrap();
        let baseline = git_snapshot(&repo, &ui).unwrap();
        let proposal_commit = baseline.head.clone().unwrap();
        let mut metadata = RunMetadata {
            schema_version: METADATA_SCHEMA_VERSION,
            request: "test reconciliation".to_owned(),
            change: Some("test-reconciliation".to_owned()),
            stage: Stage::Apply,
            verify_retries: 0,
            proposal_commit: Some(proposal_commit.clone()),
            final_commit: None,
            expected_head: Some(proposal_commit),
            adopted_commits: Vec::new(),
            tolerated_dirty_paths: Vec::new(),
            baseline,
            before_changes: crate::openspec::ChangeSnapshot::default(),
            pending_repair: None,
            sessions: Vec::new(),
        };

        fs::write(repo.join("implementation.txt"), "implemented\n").unwrap();
        fs::write(repo.join("a.out"), "new build output\n").unwrap();
        git_ok(&repo, &["add", "implementation.txt"]);
        git_ok(
            &repo,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "implementation",
            ],
        );
        let adopted = current_head(&repo, &ui).unwrap().unwrap();
        ensure_stage_repository(&repo, &mut metadata, &ui, "in test", false).unwrap();
        assert_eq!(metadata.expected_head.as_deref(), Some(adopted.as_str()));
        assert_eq!(metadata.adopted_commits, [adopted]);
        assert_eq!(metadata.tolerated_dirty_paths, ["a.out"]);
        let final_state = git_snapshot(&repo, &ui).unwrap();
        assert!(
            final_candidate_paths(&metadata.baseline, &final_state)
                .unwrap()
                .is_empty()
        );

        git_ok(
            &repo,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "strict should reject",
            ],
        );
        assert!(ensure_stage_repository(&repo, &mut metadata, &ui, "in test", true).is_err());
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn proposal_can_update_its_change_and_canonical_specs() {
        let baseline = dirty_snapshot(&["notes.txt"]);
        let current = dirty_snapshot(&[
            "notes.txt",
            "openspec/changes/add-bits/proposal.md",
            "openspec/specs/bitwise-operators/spec.md",
        ]);
        let (paths, ignored) =
            proposal_candidate_paths(&baseline, &current, "openspec/changes/add-bits").unwrap();
        assert_eq!(
            paths,
            [
                "openspec/changes/add-bits/proposal.md",
                "openspec/specs/bitwise-operators/spec.md"
            ]
        );
        assert!(ignored.is_empty());
    }

    #[test]
    fn proposal_separates_non_openspec_and_other_change_paths() {
        let baseline = dirty_snapshot(&[]);
        let outside_source =
            dirty_snapshot(&["openspec/changes/add-bits/proposal.md", "src/compiler.rs"]);
        let (allowed, ignored) =
            proposal_candidate_paths(&baseline, &outside_source, "openspec/changes/add-bits")
                .unwrap();
        assert_eq!(allowed, ["openspec/changes/add-bits/proposal.md"]);
        assert_eq!(ignored, ["src/compiler.rs"]);

        let other_change = dirty_snapshot(&[
            "openspec/changes/add-bits/proposal.md",
            "openspec/changes/unrelated/tasks.md",
        ]);
        let (allowed, ignored) =
            proposal_candidate_paths(&baseline, &other_change, "openspec/changes/add-bits")
                .unwrap();
        assert_eq!(allowed, ["openspec/changes/add-bits/proposal.md"]);
        assert_eq!(ignored, ["openspec/changes/unrelated/tasks.md"]);
    }

    #[test]
    fn proposal_requires_selected_change_output() {
        let baseline = dirty_snapshot(&[]);
        let current = dirty_snapshot(&["src/compiler.rs"]);
        assert!(
            proposal_candidate_paths(&baseline, &current, "openspec/changes/add-bits").is_err()
        );
    }

    #[test]
    fn abort_candidates_exclude_every_preexisting_dirty_path() {
        let baseline = dirty_snapshot(&["notes.txt", "staged.txt"]);
        let current = dirty_snapshot(&[
            "notes.txt",
            "staged.txt",
            "openspec/changes/new/proposal.md",
            "openspec/specs/new/spec.md",
        ]);
        assert_eq!(
            run_owned_paths(&baseline, &current),
            [
                "openspec/changes/new/proposal.md",
                "openspec/specs/new/spec.md"
            ]
        );
    }
}
