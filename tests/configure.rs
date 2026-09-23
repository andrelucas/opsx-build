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

    fn run(&self, mode: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_opsx-build"))
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
            .arg(format!(
                "--config={}",
                self.root.join("global.toml").display()
            ))
            .args(args)
            .output()
            .unwrap()
    }

    fn markdown(&self) -> String {
        fs::read_to_string(self.root.join("campaign/opsx-build.md")).unwrap()
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
    assert!(saved.contains("--max-provider-retries=7"));
    assert!(saved.contains("--claude-model=@preset/motd"));
    assert!(saved.contains("This preset is managed externally"));
    assert!(saved.contains("Test transactions"));
    assert!(!saved.contains("secret-do-not-copy"));
    let reference = fs::read_to_string(fixture.root.join("reference.md")).unwrap();
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
