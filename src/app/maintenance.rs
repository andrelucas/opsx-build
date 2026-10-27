use super::*;

impl<U: Ui> App<U> {
    pub(super) fn accept_changes(&self, product: &Path) -> Result<()> {
        let association = load_association(product, &self.ui)?;
        let repo = association
            .as_ref()
            .map_or(product, |a| a.planning_root.as_path());
        let mut state = load_state(repo, &self.ui)?;
        // Acquiring the existing run lock also rejects a still-running campaign.
        let guard = if self.cli.dry_run {
            None
        } else {
            state
                .operating
                .as_ref()
                .map(|run| run.open(None))
                .transpose()?
        };
        let configured = self.cli.campaign_config.is_some();
        if let Some(previous) = &state.contracts {
            let (old_contracts, old_tests) = previous.paths();
            let contracts = if configured || !self.cli.contract_files.is_empty() {
                self.cli.contract_files.clone()
            } else {
                old_contracts
            };
            let tests = if configured || !self.cli.acceptance_files.is_empty() {
                self.cli.acceptance_files.clone()
            } else {
                old_tests
            };
            let inventory = if configured {
                self.cli.obligation_inventory.as_deref()
            } else {
                self.cli
                    .obligation_inventory
                    .as_deref()
                    .or(previous.obligation_inventory.as_deref())
            };
            if previous.obligation_inventory.is_some() && inventory.is_none() {
                bail!(
                    "this campaign uses obligation assignments; select its corrected or replacement inventory rather than removing it"
                );
            }
            let mut corrected = crate::contracts::SuppliedContracts::capture_with_inventory(
                repo, &contracts, &tests, inventory,
            )?;
            corrected.compact = previous.compact;
            corrected.runner = previous.runner.clone();
            corrected.check_schema(repo)?;
            let commands = if configured
                || self.cli.acceptance_disabled
                || !self.cli.acceptance_commands.is_empty()
            {
                self.cli.acceptance_commands.clone()
            } else {
                previous
                    .acceptance
                    .as_ref()
                    .map(|g| g.commands.clone())
                    .unwrap_or_default()
            };
            let timeout = self
                .cli
                .acceptance_timeout_seconds
                .or_else(|| previous.acceptance.as_ref().map(|g| g.timeout_seconds));
            if commands.iter().any(|c| c.trim().is_empty()) || timeout == Some(0) {
                bail!("acceptance commands must be nonempty and their timeout positive");
            }
            if !commands.is_empty() && tests.is_empty() {
                bail!("register the maintainer-owned acceptance files used by the final commands");
            }
            corrected.acceptance = crate::acceptance::AcceptanceGate::new(commands, timeout);
            state.contracts = Some(corrected);
        } else if self.cli.supplied_contracts
            || self.cli.obligation_inventory.is_some()
            || !self.cli.contract_files.is_empty()
            || !self.cli.acceptance_files.is_empty()
            || !self.cli.acceptance_commands.is_empty()
        {
            bail!(
                "accept-changes preserves the campaign's workflow; it cannot switch an ordinary campaign to supplied-contract mode"
            );
        }

        let context = self
            .cli
            .bootstrap_context
            .clone()
            .map(|p| repo.join(p))
            .or_else(|| state.context_path.clone());
        let mut config_update = None;
        if let Some(path) = context {
            let text = fs::read_to_string(&path).with_context(|| {
                format!("cannot read project introduction `{}`", path.display())
            })?;
            if text.trim().is_empty() {
                bail!("project introduction `{}` is empty", path.display());
            }
            let mut definitions = state.context_definitions.clone();
            definitions.extend(self.cli.bootstrap_defines.clone());
            // A maintainer may remove a placeholder while correcting the brief.
            definitions.retain(|name, _| {
                text.split("{{")
                    .skip(1)
                    .filter_map(|part| part.split_once("}}").map(|(key, _)| key.trim()))
                    .any(|key| key == name)
            });
            let text = crate::bootstrap::render_context_template(&text, &definitions)?;
            state.context_definitions = definitions;
            let config = repo.join("openspec/config.yaml");
            let contents = fs::read_to_string(&config)?;
            // Bootstrap writes context as a top-level block. Replace only that
            // block, preserving the project's schema, rules and other settings.
            let lines = contents.lines().collect::<Vec<_>>();
            if let Some(start) = lines.iter().position(|line| *line == "context: |") {
                let end = (start + 1..lines.len())
                    .find(|&i| {
                        !lines[i].is_empty()
                            && !lines[i].starts_with(' ')
                            && !lines[i].starts_with('#')
                    })
                    .unwrap_or(lines.len());
                let mut updated = lines[..start].join("\n");
                updated.push_str("\ncontext: |\n");
                for line in text.lines() {
                    updated.push_str(&format!("  {line}\n"));
                }
                if end < lines.len() {
                    updated.push_str(&lines[end..].join("\n"));
                    updated.push('\n');
                }
                if updated != contents {
                    config_update = Some((config, updated));
                }
            }
            state.context_path = Some(path);
        }

        // This does not close findings or discard prior requests and receipts.
        state.verify_retries = 0;
        state.frontier_replans = 0;
        state.terminal_remediations = 0;
        state.too_large = None;
        state.verification = None;
        state.archive = None;
        state.planning_session = None;
        state.planning_backend = None;
        state.terminal_review_complete = false;
        state.checks.independent_check_failed();
        state.maintainer_correction = true;
        state.pending_direction = Some("The maintainer corrected the campaign inputs. Read the current introduction, contracts, inventory and acceptance configuration. Revise only the work required by the correction, including requirements affecting already completed slices. Preserve implementation and unrelated open findings. An old pass does not complete changed requirements. Update affected assignments and tasks; do not ask to approve the maintainer's correction or audit every old result. Required delivery reports remain implementation work.".into());
        state.pending_repair = state.review.repair_findings();

        if !state.bootstrap {
            state.before_changes = openspec_snapshot(repo, &self.ui)?;
            let active = state
                .change
                .as_ref()
                .is_some_and(|name| state.before_changes.changes.contains_key(name));
            if !active {
                if state.agenda.is_some() || has_assignments(&state) {
                    state.request = "advance".into();
                    state.agenda = None;
                    state.change = None;
                    self.assign_agenda(repo, &mut state)?;
                    if state.change.is_none() {
                        state.agenda = None;
                        state.change = Some(format!(
                            "maintainer-correction-{}",
                            &Uuid::new_v4().simple().to_string()[..8]
                        ));
                    }
                } else {
                    state.change = Some(format!(
                        "maintainer-correction-{}",
                        &Uuid::new_v4().simple().to_string()[..8]
                    ));
                }
            }
        }
        state.stage = Stage::Propose;

        if self.cli.dry_run {
            self.ui.info("DRY RUN — would accept the current campaign inputs and configuration, preserve findings and work, and continue at planning");
            return Ok(());
        }
        if let Some(guard) = guard.as_ref() {
            guard.accept_changes()?;
        }
        if let Some((path, contents)) = config_update {
            fs::write(path, contents)?;
        }
        // Recovery must never roll back across a maintainer correction.
        release_state_baseline(repo, &mut state, &self.ui);
        if !state.bootstrap {
            arm_slice_baseline(repo, &mut state, &self.ui)?;
        }
        persist_state(repo, &state, &self.ui)?;
        self.ui.success("Accepted the maintainer's current campaign inputs and configuration. Implementation, findings and usage history are preserved.");
        self.ui.info("Continue with: opsx-build --resume");
        Ok(())
    }
}
