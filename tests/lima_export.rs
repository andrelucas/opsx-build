#![cfg(unix)]
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn git(root: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap().trim().to_owned()
}
struct Fixture {
    root: PathBuf,
    host: PathBuf,
    guest: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("opsx-collect-{}", uuid::Uuid::new_v4()));
        let host = root.join("host");
        let guest = root.join("guest");
        fs::create_dir_all(&host).unwrap();
        git(&host, &["init", "-q"]);
        git(&host, &["config", "user.name", "Fixture"]);
        git(&host, &["config", "user.email", "test@localhost"]);
        fs::write(host.join("input"), "original").unwrap();
        fs::write(host.join("extra"), "original").unwrap();
        fs::write(host.join(".gitignore"), "*.log\n").unwrap();
        git(&host, &["add", "."]);
        git(&host, &["commit", "-qm", "input"]);
        fs::create_dir(&guest).unwrap();
        assert!(
            Command::new("rsync")
                .arg("-a")
                .arg(host.join("."))
                .arg(&guest)
                .status()
                .unwrap()
                .success()
        );
        let bin = root.join("limactl");
        fs::write(
            &bin,
            r#"#!/bin/sh
case "$1" in
 shell) exit 0 ;;
 copy) exec rsync -a "$TEST_GUEST/" ./ ;;
 *) exit 3 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(bin, fs::Permissions::from_mode(0o755)).unwrap();
        let key = Sha256::digest(host.canonicalize().unwrap().as_os_str().as_encoded_bytes());
        let state = root.join("state/opsx-build/lima");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join(format!("{key:x}.json")),r#"{"instance":"opsx-test","base":"base","resources":{},"guest_dir":"/guest/campaign"}"#).unwrap();
        Self { root, host, guest }
    }
    fn export(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_opsx-build"))
            .current_dir(&self.host)
            .args(["lima", "export"])
            .args(args)
            .env("TEST_GUEST", &self.guest)
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env(
                "PATH",
                format!("{}:{}", self.root.display(), std::env::var("PATH").unwrap()),
            )
            .output()
            .unwrap()
    }
    fn ok(&self) {
        let o = self.export(&[]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn existing_clone_collects_history_index_dirty_files_and_checkpoints_repeatedly() {
    let f = Fixture::new();
    git(
        &f.host,
        &["remote", "add", "origin", "https://example.test/host-only"],
    );
    fs::write(f.host.join("extra"), "host note").unwrap();
    fs::write(f.host.join("host-only.log"), "host log").unwrap();
    fs::write(f.guest.join("input"), "completed").unwrap();
    git(&f.guest, &["commit", "-qam", "campaign result"]);
    git(&f.guest, &["tag", "post-bootstrap"]);
    fs::write(f.guest.join("input"), "staged").unwrap();
    git(&f.guest, &["add", "input"]);
    fs::write(f.guest.join("input"), "working").unwrap();
    fs::write(f.guest.join("build.log"), "one").unwrap();
    symlink("input", f.guest.join("result-link")).unwrap();
    fs::create_dir_all(f.guest.join(".git/opsx-build")).unwrap();
    fs::write(
        f.guest.join(".git/opsx-build/last-run.json"),
        r#"{"stage":"done"}"#,
    )
    .unwrap();
    for _ in 0..2 {
        f.ok();
    }
    assert_eq!(
        git(&f.host, &["rev-parse", "HEAD"]),
        git(&f.guest, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        git(&f.host, &["rev-parse", "post-bootstrap"]),
        git(&f.host, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        git(&f.host, &["remote", "get-url", "origin"]),
        "https://example.test/host-only"
    );
    assert_eq!(git(&f.host, &["show", ":input"]), "staged");
    for (p, v) in [
        ("input", "working"),
        ("extra", "host note"),
        ("host-only.log", "host log"),
        ("build.log", "one"),
        (".git/opsx-build/last-run.json", r#"{"stage":"done"}"#),
    ] {
        assert_eq!(fs::read_to_string(f.host.join(p)).unwrap(), v);
    }
    assert_eq!(
        fs::read_link(f.host.join("result-link")).unwrap(),
        Path::new("input")
    );
    fs::write(f.guest.join("build.log"), "two").unwrap();
    fs::remove_file(f.guest.join("result-link")).unwrap();
    f.ok();
    assert_eq!(fs::read_to_string(f.host.join("build.log")).unwrap(), "two");
    assert!(!f.host.join("result-link").exists());
}
#[test]
fn first_export_from_an_older_clone_collects_the_guest_preflight_report() {
    let f = Fixture::new();
    let report = Path::new(".git/opsx-build/contract-preflight.json");
    let host_report = r#"{"inputs":{},"ready":false,"summary":"host preflight"}"#;
    let guest_report = r#"{"inputs":{},"ready":true,"summary":"guest preflight"}"#;
    for (checkout, contents) in [(&f.host, host_report), (&f.guest, guest_report)] {
        fs::create_dir_all(checkout.join(report.parent().unwrap())).unwrap();
        fs::write(checkout.join(report), contents).unwrap();
    }
    let checkpoint = ".git/opsx-build/last-run.json";
    fs::write(f.guest.join(checkpoint), r#"{"stage":"done"}"#).unwrap();
    fs::write(f.host.join(checkpoint), r#"{"stage":"paused"}"#).unwrap();
    let blocked = f.export(&[]);
    assert!(!blocked.status.success());
    let message = String::from_utf8_lossy(&blocked.stderr);
    assert!(message.contains(checkpoint), "{message}");
    assert!(!message.contains(report.to_str().unwrap()), "{message}");
    assert_eq!(
        fs::read_to_string(f.host.join(report)).unwrap(),
        host_report
    );

    fs::remove_file(f.host.join(checkpoint)).unwrap();
    for _ in 0..2 {
        f.ok();
        assert_eq!(
            fs::read_to_string(f.host.join(report)).unwrap(),
            guest_report
        );
        assert_eq!(
            fs::read_to_string(f.guest.join(report)).unwrap(),
            guest_report
        );
        assert_eq!(
            fs::read_to_string(f.host.join(checkpoint)).unwrap(),
            r#"{"stage":"done"}"#
        );
    }
}

#[test]
fn conflicts_leave_both_working_copies_and_host_ref_intact() {
    let f = Fixture::new();
    let original = git(&f.host, &["rev-parse", "HEAD"]);
    fs::write(f.host.join("input"), "host correction").unwrap();
    fs::write(f.guest.join("input"), "VM change").unwrap();
    git(&f.guest, &["commit", "-qam", "campaign result"]);
    fs::write(f.guest.join("artifact"), "result").unwrap();
    let output = f.export(&[]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("conflicts at input"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(git(&f.host, &["rev-parse", "HEAD"]), original);
    assert_eq!(
        fs::read_to_string(f.host.join("input")).unwrap(),
        "host correction"
    );
    assert!(!f.host.join("artifact").exists());
    assert_eq!(
        fs::read_to_string(f.guest.join("input")).unwrap(),
        "VM change"
    );
    let snapshot = f.root.join("separate");
    let explicit = f.export(&[snapshot.to_str().unwrap()]);
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    assert_eq!(
        fs::read_to_string(snapshot.join("artifact")).unwrap(),
        "result"
    );
}
#[test]
fn host_commits_and_staged_corrections_are_protected() {
    let f = Fixture::new();
    fs::write(f.host.join("extra"), "local staging").unwrap();
    git(&f.host, &["add", "extra"]);
    fs::write(f.guest.join("input"), "guest commit").unwrap();
    git(&f.guest, &["commit", "-qam", "guest"]);
    assert!(!f.export(&[]).status.success());
    assert_eq!(git(&f.host, &["show", ":extra"]), "local staging");
    git(&f.host, &["commit", "-qm", "host commit"]);
    let local = git(&f.host, &["rev-parse", "HEAD"]);
    assert!(!f.export(&[]).status.success());
    assert_eq!(git(&f.host, &["rev-parse", "HEAD"]), local);
}
