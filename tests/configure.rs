use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "opsx-configure-integration-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("campaign")).unwrap();
        let claude = root.join("bin/claude");
        fs::write(&claude, r#"#!/bin/sh
printf '%s\n' "$@" > "$CONFIGURE_TEST_ROOT/launcher.txt"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --add-dir) shift; scratch="$1" ;;
    --model|--effort|--permission-mode|--print|--plugin-dir) exit 81 ;;
  esac
  shift
done
cp "$scratch/reference.md" "$CONFIGURE_TEST_ROOT/reference.md"
case "$CONFIGURE_TEST_MODE" in
  cancel) exit 0 ;;
  fail) exit 23 ;;
  invalid) printf '%s' '{"accepted":true,"intent":"Intent","notes":"Agreed","arguments":["--forget"]}' > "$scratch/proposal.json" ;;
  *) cp "$CONFIGURE_TEST_ROOT/proposal.json" "$scratch/proposal.json" ;;
esac
"#).unwrap();
        fs::set_permissions(claude, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            root.join("global.toml"),
            r#"
max_provider_retries = 7
max_terminal_remediations = 5
worker_connection = "remote"
frontier_connection = "remote"
[connections.remote]
backend = "codex"
command = "codex -c model_reasoning_effort=max"
model = "frontier-default"
environment = "remote"
[environments.remote.env]
OPENROUTER_API_KEY = "secret-do-not-copy"
"#,
        )
        .unwrap();
        fs::write(root.join("proposal.json"), serde_json::to_string(&serde_json::json!({
            "accepted":true, "intent":"Test transactions with a bounded worker.",
            "notes":"Accepted provider retry default of seven from global configuration; chose a preset for experiments.",
            "arguments":["--worker-connection=remote", "--worker-model=@preset/motd", "--worker-structured-output=false", "--frontier-worker", "--loop"]
        })).unwrap()).unwrap();
        Self { root }
    }

    fn command(&self, mode: &str, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_opsx-build"));
        command
            .current_dir(self.root.join("campaign"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("CONFIGURE_TEST_ROOT", &self.root)
            .env("CONFIGURE_TEST_MODE", mode)
            .env_remove("OPSX_BUILD_CONFIG")
            .env_remove("OPSX_BUILD_HARNESS_SANDBOX")
            .arg(format!(
                "--config={}",
                self.root.join("global.toml").display()
            ))
            .args(args);
        command
    }

    fn run(&self, mode: &str, args: &[&str]) -> Output {
        self.command(mode, args).output().unwrap()
    }

    fn markdown(&self) -> String {
        fs::read_to_string(self.root.join("campaign/opsx-build.md")).unwrap()
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(self.root.join("campaign"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn init_git(&self) {
        self.git(&["init", "-q"]);
        self.git(&["config", "user.name", "Fixture"]);
        self.git(&["config", "user.email", "fixture@example.invalid"]);
        self.git(&["config", "commit.gpgsign", "false"]);
        self.git(&["config", "core.hooksPath", ".git/hooks"]);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn configure_uses_plain_claude_and_saves_only_valid_accepted_settings() {
    let fixture = Fixture::new();
    let result = fixture.run("accept", &["configure"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let saved = fixture.markdown();
    assert!(String::from_utf8_lossy(&result.stderr).contains("was not committed"));
    assert!(!fixture.root.join("campaign/.git").exists());
    let launcher = fs::read_to_string(fixture.root.join("launcher.txt")).unwrap();
    assert!(launcher.contains("suggest --supplied-contracts"));
    assert!(launcher.contains("never silently switch an existing choice"));
    assert!(launcher.contains("contract_test.go is an acceptance input"));
    assert!(launcher.contains("Never run builds or checks during configure"));
    assert!(saved.contains("--no-supplied-contracts"));
    assert!(saved.contains("--no-acceptance-checks"));
    assert!(saved.contains("--max-provider-retries=7"));
    assert!(saved.contains("--max-terminal-remediations=5"));
    assert!(saved.contains("--claude-model=@preset/motd"));
    assert!(saved.contains("This preset is managed externally"));
    assert!(saved.contains("Test transactions"));
    assert!(!saved.contains("secret-do-not-copy"));
    let feedback = String::from_utf8_lossy(&result.stderr);
    assert!(feedback.contains("Workflow: ordinary context"));
    assert!(feedback.contains("Worker: `remote` (codex, @preset/motd)"));
    assert!(feedback.contains("Planning assumes a frontier-capable worker"));
    assert!(feedback.contains("no final commands configured"));
    assert!(!feedback.contains("secret-do-not-copy"));
    let reference = fs::read_to_string(fixture.root.join("reference.md")).unwrap();
    assert!(reference.contains("--max-terminal-remediations=5"));
    assert!(!reference.contains("secret-do-not-copy"));
    assert!(reference.contains("Effective defaults"));
    assert!(reference.contains("Built-in defaults"));
    assert!(reference.contains("frontier-default"));
    for mode in ["cancel", "fail", "invalid"] {
        let result = fixture.run(mode, &["configure"]);
        assert_eq!(result.status.success(), mode == "cancel");
        assert_eq!(
            fixture.markdown(),
            saved,
            "{mode} must preserve the last accepted configuration"
        );
    }
}

#[test]
fn configure_commits_only_its_file_and_preserves_staged_and_unstaged_work() {
    for existing_head in [false, true] {
        let fixture = Fixture::new();
        fixture.init_git();
        let other = fixture.root.join("campaign/other.txt");
        if existing_head {
            fs::write(&other, "original\n").unwrap();
            fixture.git(&["add", "other.txt"]);
            fixture.git(&["commit", "-qm", "Original work"]);
        }
        fs::write(&other, "staged work\n").unwrap();
        fixture.git(&["add", "other.txt"]);
        fs::write(&other, "unstaged work\n").unwrap();
        fs::write(
            fixture.root.join("campaign/untracked.txt"),
            "untracked work\n",
        )
        .unwrap();
        let staged = fixture.git(&["diff", "--cached", "--binary"]);
        let unstaged = fixture.git(&["diff", "--binary"]);
        let untracked = fixture.git(&["ls-files", "--others", "--exclude-standard"]);

        for intent in ["First configuration", "Updated configuration"] {
            let path = fixture.root.join("proposal.json");
            let mut proposal: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            proposal["intent"] = intent.into();
            fs::write(&path, serde_json::to_vec(&proposal).unwrap()).unwrap();
            let result = fixture.run("accept", &["configure"]);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(
                fixture.git(&["show", "HEAD:opsx-build.md"]),
                fixture.markdown()
            );
            assert_eq!(
                fixture.git(&[
                    "diff-tree",
                    "--root",
                    "--no-commit-id",
                    "--name-only",
                    "-r",
                    "HEAD"
                ]),
                "opsx-build.md\n"
            );
            assert_eq!(
                fixture.git(&["log", "-1", "--format=%s"]),
                "opsx: configure campaign\n"
            );
            assert_eq!(fixture.git(&["diff", "--cached", "--binary"]), staged);
            assert_eq!(fixture.git(&["diff", "--binary"]), unstaged);
            assert_eq!(
                fixture.git(&["ls-files", "--others", "--exclude-standard"]),
                untracked
            );
        }
        let head = fixture.git(&["rev-parse", "HEAD"]);
        for mode in ["cancel", "fail", "invalid"] {
            fixture.run(mode, &["configure"]);
            assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
        }
    }
}

#[test]
fn configure_keeps_saved_configuration_when_a_commit_hook_rejects_it() {
    let fixture = Fixture::new();
    fixture.init_git();
    fs::write(fixture.root.join("campaign/other.txt"), "staged work\n").unwrap();
    fixture.git(&["add", "other.txt"]);
    let staged = fixture.git(&["diff", "--cached", "--binary", "--", "other.txt"]);
    let hook = fixture.root.join("campaign/.git/hooks/pre-commit");
    fs::write(
        &hook,
        "#!/bin/sh\necho 'Fixture rejects commit' >&2\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let result = fixture.run("accept", &["configure"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(fixture.markdown().contains("Test transactions"));
    let message = String::from_utf8_lossy(&result.stderr);
    assert!(message.contains("was not committed"), "{message}");
    assert!(message.contains("Fixture rejects commit"), "{message}");
    assert_eq!(
        fixture.git(&["diff", "--cached", "--binary", "--", "other.txt"]),
        staged
    );
    assert!(!fixture.root.join("campaign/.git/index.lock").exists());
}

#[test]
fn configure_persists_an_accepted_supplied_contract_workflow_and_validates_inputs() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("campaign/contract.md"),
        "# Contract\nREQ-1: Required behaviour.\n",
    )
    .unwrap();
    fs::write(
        fixture.root.join("campaign/check.sh"),
        "touch should-not-run\n",
    )
    .unwrap();
    fs::write(fixture.root.join("proposal.json"), serde_json::to_string(&serde_json::json!({
        "accepted": true, "intent": "Implement the supplied contract.", "notes": "Accepted the suggested workflow.",
        "arguments": ["--supplied-contracts", "--contract=contract.md", "--acceptance-file=check.sh", "--acceptance-command=sh check.sh", "--acceptance-timeout-seconds=45"]
    })).unwrap()).unwrap();
    let result = fixture.run("accept", &["configure"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let saved = fixture.markdown();
    assert!(saved.contains("--supplied-contracts"));
    assert!(saved.contains("--contract=contract.md"));
    assert!(saved.contains("--acceptance-file=check.sh"));
    assert!(saved.contains("--acceptance-command=sh check.sh"));
    assert!(saved.contains("--acceptance-timeout-seconds=45"));
    assert!(!fixture.root.join("campaign/should-not-run").exists());
    fs::remove_file(fixture.root.join("campaign/contract.md")).unwrap();
    let result = fixture.run("accept", &["configure"]);
    assert!(!result.status.success());
    assert_eq!(fixture.markdown(), saved);
}

#[test]
fn configure_dry_run_does_not_start_claude_or_create_markdown() {
    let fixture = Fixture::new();
    let result = fixture.run("accept", &["configure", "--dry-run"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!fixture.root.join("launcher.txt").exists());
    assert!(!fixture.root.join("campaign/opsx-build.md").exists());
}

#[test]
fn configure_honors_and_persists_the_harness_sandbox_override() {
    let fixture = Fixture::new();
    let result = fixture.run("accept", &["configure", "--no-harness-sandbox"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let launch = fs::read_to_string(fixture.root.join("launcher.txt")).unwrap();
    assert!(launch.contains("--settings\n{\"sandbox\":{\"enabled\":false}}\n"));
    assert!(!launch.contains("--permission-mode"));
    assert!(fixture.markdown().contains("--no-harness-sandbox"));

    let result = fixture.run("accept", &["configure"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        fs::read_to_string(fixture.root.join("launcher.txt"))
            .unwrap()
            .contains("--settings")
    );

    let result = fixture.run("accept", &["configure", "--harness-sandbox"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !fs::read_to_string(fixture.root.join("launcher.txt"))
            .unwrap()
            .contains("--settings")
    );
    assert!(!fixture.markdown().contains("--no-harness-sandbox"));
}

#[test]
fn harness_sandbox_environment_can_be_overridden_on_the_command_line() {
    let fixture = Fixture::new();
    for (extra, disabled) in [(vec![], true), (vec!["--harness-sandbox"], false)] {
        let result = Command::new(env!("CARGO_BIN_EXE_opsx-build"))
            .current_dir(fixture.root.join("campaign"))
            .env("OPSX_BUILD_HARNESS_SANDBOX", "false")
            .args(["--no-config", "configure", "--dry-run"])
            .args(extra)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&result.stderr).contains("--no-harness-sandbox"),
            disabled
        );
    }
}

#[test]
fn configuring_does_not_require_campaign_credentials() {
    let fixture = Fixture::new();
    let path = fixture.root.join("global.toml");
    let config = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        config.replace(
            "\"secret-do-not-copy\"",
            "{ from_env = \"OPSX_TEST_UNSET_PROVIDER_CREDENTIAL_781AC\" }",
        ),
    )
    .unwrap();
    let result = fixture.run("accept", &["configure"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(fixture.markdown().contains("--worker-environment=remote"));
    assert!(
        !fixture
            .markdown()
            .contains("OPSX_TEST_UNSET_PROVIDER_CREDENTIAL_781AC")
    );
}

#[test]
fn reconfigure_preserves_existing_choices_last_used_and_cli_defaults() {
    let fixture = Fixture::new();
    let result = fixture.run("accept", &["configure", "--max-provider-retries=14"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let previous = fixture.markdown().replace(
        "Not run with this configuration yet.",
        "Previous run record retained.",
    );
    fs::write(fixture.root.join("campaign/opsx-build.md"), previous).unwrap();
    // An accepted empty override list means keep the settings shown by configure.
    fs::write(fixture.root.join("proposal.json"), serde_json::to_string(&serde_json::json!({
        "accepted":true, "intent":"Keep the campaign settings.", "notes":"Accepted current settings.", "arguments":[]
    })).unwrap()).unwrap();
    let global = fs::read_to_string(fixture.root.join("global.toml")).unwrap();
    fs::write(
        fixture.root.join("global.toml"),
        global.replace("max_provider_retries = 7", "max_provider_retries = 20"),
    )
    .unwrap();
    let result = fixture.run("accept", &["configure"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let saved = fixture.markdown();
    assert!(saved.contains("--max-provider-retries=14"));
    assert!(saved.contains("--claude-model=@preset/motd"));
    assert!(saved.contains("Previous run record retained."));
    assert!(saved.contains("default_sources"));
}

fn automatic_fixture() -> Fixture {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("global.toml"), "worker_connection = 'automatic'\n[connections.automatic]\nbackend = 'claude'\ncommand = 'claude'\nmodel = 'configured-model'\n").unwrap();
    fs::write(fixture.root.join("bin/claude"), r#"#!/bin/sh
printf '%s\n' "$@" > "$CONFIGURE_TEST_ROOT/launcher.txt"
while [ "$#" -gt 0 ]; do
 case "$1" in --add-dir) shift; scratch="$1";; esac
 shift
done
cp "$scratch/reference.md" "$CONFIGURE_TEST_ROOT/reference.md"
if [ "$CONFIGURE_TEST_MODE" = blocked ]; then
 echo '{"type":"result","subtype":"success","result":"Missing authoritative contract","structured_output":{"opsx_status":"BLOCKED","summary":"Missing authoritative contract"}}'
 exit 0
fi
if [ "$CONFIGURE_TEST_MODE" != missing ]; then
 cp "$CONFIGURE_TEST_ROOT/proposal.json" "$scratch/proposal.json"
fi
echo '{"type":"result","subtype":"success","result":"Proposal ready","structured_output":{"opsx_status":"READY","summary":"Proposal ready"}}'
"#).unwrap();
    fs::write(
        fixture.root.join("campaign/contract.md"),
        "Exact supplied behaviour.",
    )
    .unwrap();
    fs::write(fixture.root.join("proposal.json"), serde_json::to_string(&serde_json::json!({
        "accepted":true, "intent":"Implement supplied behaviour.", "notes":"Accepted supplied contract defaults.",
        "arguments":["--supplied-contracts", "--contract=contract.md"]
    })).unwrap()).unwrap();
    fixture
}

#[test]
fn configuration_accepts_paragraph_notes_without_changing_settings() {
    for action in ["configure", "autoconfigure"] {
        let fixture = if action == "autoconfigure" {
            automatic_fixture()
        } else {
            Fixture::new()
        };
        fixture.init_git();
        let path = fixture.root.join("proposal.json");
        let mut proposal: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        proposal["notes"] = serde_json::json!([
            "Accepted the configured defaults.",
            "## Coverage gaps\nNo complete coverage is claimed."
        ]);
        fs::write(&path, serde_json::to_vec(&proposal).unwrap()).unwrap();
        let result = fixture.run("accept", &[action]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(fixture.markdown().contains(
            "Accepted the configured defaults.\n\n## Coverage gaps\nNo complete coverage is claimed."
        ));
        assert_eq!(
            fixture.git(&["log", "-1", "--format=%s"]).trim(),
            "opsx: configure campaign"
        );
        if action == "autoconfigure" {
            assert!(fixture.markdown().contains("--contract=contract.md"));
            assert!(
                fixture
                    .markdown()
                    .contains("--claude-model=configured-model")
            );
        }
    }
}

#[test]
fn autoconfigure_rejects_nontext_notes_without_saving() {
    for notes in [
        serde_json::json!(["Valid paragraph", 42]),
        serde_json::json!({"unexpected": "shape"}),
    ] {
        let fixture = automatic_fixture();
        let path = fixture.root.join("proposal.json");
        let mut proposal: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        proposal["notes"] = notes;
        fs::write(&path, serde_json::to_vec(&proposal).unwrap()).unwrap();
        let result = fixture.run("accept", &["autoconfigure"]);
        assert!(!result.status.success());
        assert!(!fixture.root.join("campaign/opsx-build.md").exists());
        assert!(String::from_utf8_lossy(&result.stderr).contains("proposal retained"));
    }
}

#[test]
fn autoconfigure_uses_effective_model_and_saves_contract_defaults_without_interaction() {
    let fixture = automatic_fixture();
    fixture.init_git();
    let result = fixture.run(
        "accept",
        &["autoconfigure", "--worker-model=explicit-model"],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let launcher = fs::read_to_string(fixture.root.join("launcher.txt")).unwrap();
    assert!(launcher.contains("--print\n"));
    assert!(launcher.contains("--model\nexplicit-model\n"));
    assert!(launcher.contains("without further confirmation"));
    assert!(fixture.markdown().contains("--supplied-contracts"));
    assert!(fixture.markdown().contains("--contract=contract.md"));
    assert!(fixture.markdown().contains("--claude-model=explicit-model"));
    let feedback = String::from_utf8_lossy(&result.stderr);
    assert!(feedback.contains("Workflow: supplied contracts (contract files: 1)"));
    assert!(feedback.contains("Worker: `automatic` (claude, explicit-model)"));
    assert_eq!(
        fixture.git(&["log", "-1", "--format=%s"]).trim(),
        "opsx: configure campaign"
    );
}

#[test]
fn autoconfigure_summary_survives_the_terminal_dashboard() {
    use std::{
        io::Read,
        os::{fd::FromRawFd, unix::process::CommandExt},
        process::Stdio,
    };

    for commit in [false, true] {
        let fixture = automatic_fixture();
        if commit {
            fixture.init_git();
        }
        let (mut master, mut slave) = (-1, -1);
        let mut size = libc::winsize {
            ws_row: 30,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // openpty initializes both descriptors on success; each File owns one.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut size,
                )
            },
            0
        );
        let mut master = unsafe { fs::File::from_raw_fd(master) };
        let slave = unsafe { fs::File::from_raw_fd(slave) };
        let mut command = fixture.command("accept", &["autoconfigure", "--stream-agent=reasoning"]);
        command
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // Give the child its own controlling terminal for dashboard raw mode.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        drop(command);
        let reader = std::thread::spawn(move || {
            let mut output = Vec::new();
            // Linux reports EIO after the last slave closes; macOS returns EOF.
            if let Err(error) = master.read_to_end(&mut output) {
                assert_eq!(error.raw_os_error(), Some(libc::EIO));
            }
            String::from_utf8(output).unwrap()
        });
        let status = child.wait().unwrap();
        let output = reader.join().unwrap();
        assert!(status.success(), "{output}");
        assert!(
            output.contains("\x1b[?1049h"),
            "dashboard did not open: {output}"
        );
        let (_, permanent) = output
            .rsplit_once("\x1b[?1049l")
            .expect("dashboard must close");
        for expected in [
            "Saved campaign configuration",
            "Workflow: supplied contracts (contract files: 1)",
            "Worker: `automatic` (claude, configured-model)",
            "Frontier:",
            "Planning assumes",
            "Acceptance:",
            if commit {
                "Committing campaign configuration"
            } else {
                "was not committed"
            },
        ] {
            assert!(
                permanent.contains(expected),
                "missing {expected} after dashboard exit: {output}"
            );
        }
    }
}

#[test]
fn autoconfigure_blocked_missing_or_invalid_proposals_do_not_save() {
    for mode in ["blocked", "missing", "invalid", "model-change"] {
        let fixture = automatic_fixture();
        if mode == "invalid" {
            fs::write(fixture.root.join("proposal.json"), "{}").unwrap();
        } else if mode == "model-change" {
            fs::write(fixture.root.join("proposal.json"), serde_json::to_string(&serde_json::json!({
                "accepted":true,"intent":"Intent","notes":"Notes","arguments":["--worker-model=different"]
            })).unwrap()).unwrap();
        }
        let result = fixture.run(mode, &["autoconfigure"]);
        assert!(
            !result.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(!fixture.root.join("campaign/opsx-build.md").exists());
    }
}

#[test]
fn autoconfigure_dry_run_does_not_launch_an_agent() {
    let fixture = automatic_fixture();
    let result = fixture.run("accept", &["autoconfigure", "--dry-run"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!fixture.root.join("launcher.txt").exists());
    assert!(!fixture.root.join("campaign/opsx-build.md").exists());
}

#[test]
fn autoconfigure_defaults_and_existing_acceptance_gate_are_preserved() {
    let fixture = automatic_fixture();
    fs::write(fixture.root.join("campaign/check.sh"), "exit 0\n").unwrap();
    let result = fixture.run(
        "accept",
        &[
            "autoconfigure",
            "--supplied-contracts",
            "--contract=contract.md",
            "--acceptance-command=true",
            "--acceptance-file=check.sh",
        ],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let saved = fixture.markdown();
    assert!(saved.contains("--claude-model=configured-model"));
    assert!(saved.contains("--acceptance-command=true"));
    assert!(String::from_utf8_lossy(&result.stderr).contains(
        "Acceptance: protected files: 1; final commands: 1; timeout: 600s each (not run)"
    ));
    fs::write(
        fixture.root.join("proposal.json"),
        serde_json::to_string(&serde_json::json!({
            "accepted":true,"intent":"Intent","notes":"Notes","arguments":["--no-acceptance-checks"]
        }))
        .unwrap(),
    )
    .unwrap();
    let result = fixture.run("accept", &["autoconfigure"]);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("cannot change an existing acceptance gate")
    );
    assert_eq!(fixture.markdown(), saved);
}
