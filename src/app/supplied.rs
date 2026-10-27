use super::*;

pub(super) fn compact(state: &RunState) -> bool {
    state.contracts.as_ref().is_some_and(|c| c.compact)
}

pub(super) fn direct(state: &RunState) -> bool {
    compact(state) && (state.bootstrap || is_terminal_gate(state))
}

pub(super) fn displayed_stage_position(state: &RunState) -> Option<(usize, usize)> {
    if !direct(state) {
        return None;
    }
    if state.bootstrap {
        // Assignment review and repairs belong to the planning phase. Whether
        // that review is needed is known only after the assignment is written.
        return Some((
            if state.stage == Stage::FinalCommit {
                2
            } else {
                1
            },
            2,
        ));
    }
    let reused_review = !matches!(state.stage, Stage::Propose | Stage::Verify | Stage::Repair)
        && state
            .verification
            .as_ref()
            .is_some_and(|review| Some(review.change.as_str()) != state.change.as_deref());
    let review_steps = usize::from(!reused_review);
    let acceptance_steps = usize::from(state.contracts.as_ref()?.acceptance.is_some());
    let total = review_steps + acceptance_steps + 1;
    let number = match state.stage {
        Stage::Acceptance => review_steps + 1,
        Stage::FinalCommit => total,
        _ => 1,
    };
    Some((number, total))
}

impl<U: Ui> App<U> {
    pub(super) fn run_assignment(
        &self,
        repo: &Path,
        planner: &dyn AgentBackend,
        state: &mut RunState,
    ) -> Result<()> {
        if state.verify_retries > self.cli.max_verify_retries && state.pending_direction.is_none() {
            return self.stop_bootstrap_repair(repo, state);
        }
        let subject = format!(
            "{}\n\n{}\n\n{}\n\n{}",
            crate::bootstrap::assignment_instructions(self.cli.frontier_worker),
            state.pending_direction.as_deref().unwrap_or(""),
            state.pending_repair.as_deref().unwrap_or(""),
            state.contracts.as_ref().unwrap().guidance(),
        );
        let Some(result) = worker_result(
            invoke_fresh(
                planner,
                "assign-supplied-work",
                &stage_prompt("", &subject, StageProtocol::Worker),
                "Planner is assigning supplied work",
                StageProtocol::Worker,
            ),
            repo,
            state,
            &self.ui,
        )?
        else {
            return Ok(());
        };
        state.review.record_work("Assignment", &result);
        if !handle_worker_result(
            repo,
            state,
            "assignment",
            &result.text,
            result.signal,
            &self.ui,
        )? {
            return Ok(());
        }
        state.maintainer_correction = false;
        if !self.check_bootstrap_for_verify(repo, state)? {
            return Ok(());
        }
        state.pending_repair = None;
        state.consume_direction();
        state.stage = if state.contracts.as_ref().unwrap().delivery_count(repo)? == 1
            && !state.review.has_open_findings()
            && !state.review.has_unfinished_areas()
        {
            Stage::FinalCommit
        } else {
            Stage::Verify
        };
        persist_state(repo, state, &self.ui)
    }

    pub(super) fn prepare_direct_acceptance(
        &self,
        repo: &Path,
        state: &mut RunState,
    ) -> Result<()> {
        let contracts = state.contracts.as_ref().unwrap();
        let reused = !state.maintainer_correction
            && contracts.delivery_count(repo)? == 1
            && state
                .verification
                .as_ref()
                .is_some_and(|v| v.delivery_unchanged(repo, &self.ui));
        if reused {
            state.terminal_review_complete = true;
            self.ui.info("Using the single delivery's independent whole-project review; final acceptance still runs independently");
            if let Some(gate) = state.contracts.as_mut().unwrap().acceptance.as_mut() {
                gate.next_stage = Stage::FinalCommit;
                state.stage = Stage::Acceptance;
            } else {
                state.stage = Stage::FinalCommit;
            }
        } else {
            state.stage = Stage::Verify;
        }
        Ok(())
    }

    pub(super) fn plan_corrected_terminal(
        &self,
        repo: &Path,
        planner: &dyn AgentBackend,
        state: &mut RunState,
    ) -> Result<()> {
        let subject = with_contract_context(
            state,
            &format!(
                "{}\n\nThe maintainer corrected campaign inputs after earlier delivery. Plan the remaining corrections, including work needed in completed slices. Read the introduction and current originals. If code or reports need changes, add bounded delivery/remediation briefs before 9999 and update assignments.json contributions and acceptance routes, preserving prior accountability and unrelated work. Render the changed views. Keep the terminal brief at its existing path. Do not implement or create terminal OpenSpec documents. Return REPLANNED after arranging work, or READY when no implementation work remains. Do not approve or reject the maintainer's authority to change inputs. Existing unrelated findings remain open.\n{}",
                state.pending_direction.as_deref().unwrap_or(""),
                state.review.repair_findings().unwrap_or_default(),
            ),
        );
        let Some(result) = worker_result(
            invoke_fresh(
                planner,
                "plan-maintainer-correction",
                &stage_prompt("", &subject, StageProtocol::TerminalReview),
                "Planner is arranging work after the maintainer correction",
                StageProtocol::TerminalReview,
            ),
            repo,
            state,
            &self.ui,
        )?
        else {
            return Ok(());
        };
        match result.signal {
            StageSignal::Blocked => return blocked("correction planning", &result.text),
            StageSignal::Ready | StageSignal::Replanned => {}
            _ => bail!("correction planning must return READY, REPLANNED or BLOCKED"),
        }
        state.contracts.as_ref().unwrap().check_agenda(repo)?;
        state.before_changes = openspec_snapshot(repo, &self.ui)?;
        state.maintainer_correction = false;
        state.consume_direction();
        let remaining_delivery = matches!(discover_agenda(repo, &state.before_changes)?, AgendaSelection::Next(ref assignment) if !is_terminal_assignment(assignment));
        if remaining_delivery {
            self.assign_agenda(repo, state)?;
            if is_terminal_gate(state) {
                bail!(
                    "correction planner reported REPLANNED without arranging unfinished delivery work"
                );
            }
            arm_slice_baseline(repo, state, &self.ui)?;
        } else if result.signal == StageSignal::Replanned {
            bail!(
                "correction planner reported REPLANNED without arranging unfinished delivery work"
            );
        } else {
            state.stage = Stage::Verify;
        }
        persist_state(repo, state, &self.ui)
    }
}
