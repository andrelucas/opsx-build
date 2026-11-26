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
 list)
   test ! -f "$TEST_GUEST/../fail-list" || exit 1
   echo 'base Stopped'
   if test ! -f "$TEST_GUEST/../deleted"; then
     if test -f "$TEST_GUEST/../stopped"; then echo 'opsx-deadbeef01234567 Stopped'; else echo 'opsx-deadbeef01234567 Running'; fi
   fi ;;
 stop|delete)
   test "$2" = --tty=false && test "$3" = opsx-deadbeef01234567 || exit 1
   echo "$1" >> "$TEST_GUEST/../lifecycle"
   test ! -f "$TEST_GUEST/../fail-$1" || exit 1
   if test "$1" = stop; then touch "$TEST_GUEST/../stopped"; else test -f "$TEST_GUEST/../stopped" && touch "$TEST_GUEST/../deleted"; fi ;;
 shell)
   shift 5
   cd "$TEST_GUEST" || exit 1
   case "$1" in
     ps) test ! -f ../busy || echo opsx-build; exit 0 ;;
     mise) shift; echo "$*" >> ../calls; test ! -f "../fail-${4}"; exit $? ;;
   esac
   exec "$@" ;;
 copy)
   case "$4" in
     opsx-deadbeef01234567:/tmp/opsx-sync.*) exec rsync -a ./ "${4#opsx-deadbeef01234567:}" ;;
     *) exec rsync -a "$TEST_GUEST/" ./ ;;
   esac ;;
 *) exit 3 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(bin, fs::Permissions::from_mode(0o755)).unwrap();
        let key = Sha256::digest(host.canonicalize().unwrap().as_os_str().as_encoded_bytes());
        let state = root.join("state/opsx-build/lima");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join(format!("{key:x}.json")),r#"{"instance":"opsx-deadbeef01234567","base":"base","resources":{},"guest_dir":"/guest/campaign"}"#).unwrap();
        Self { root, host, guest }
    }
    fn export(&self, args: &[&str]) -> Output {
        self.lima("export", args)
    }
    fn lima(&self, action: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_opsx-build"))
            .current_dir(&self.host)
            .args(["lima", action])
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

#[test]
fn sync_preserves_guest_work_across_edits_resume_and_export() {
    let f = Fixture::new();
    fs::write(f.guest.join("input"), "VM implementation").unwrap();
    git(&f.guest, &["commit", "-qam", "guest progress"]);
    fs::create_dir_all(f.guest.join(".git/opsx-build")).unwrap();
    fs::write(
        f.guest.join(".git/opsx-build/last-run.json"),
        "guest checkpoint",
    )
    .unwrap();
    fs::write(f.guest.join("guest.log"), "only in VM").unwrap();

    fs::write(f.host.join("extra"), "maintainer correction").unwrap();
    git(&f.host, &["commit", "-qam", "maintainer"]);
    let correction = git(&f.host, &["rev-parse", "HEAD"]);
    fs::create_dir(f.host.join("new inputs")).unwrap();
    fs::write(f.host.join("new inputs/check"), [0u8, 255, 39]).unwrap();
    fs::set_permissions(
        f.host.join("new inputs/check"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("check", f.host.join("new inputs/link")).unwrap();
    fs::write(f.host.join("private.log"), "host only").unwrap();
    fs::create_dir_all(f.host.join(".git/opsx-build")).unwrap();
    fs::write(f.host.join(".git/opsx-build/last-run.json"), "host copy").unwrap();
    let sync = f.lima("sync", &["--resume"]);
    assert!(
        sync.status.success(),
        "{}",
        String::from_utf8_lossy(&sync.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.root.join("calls")).unwrap(),
        "exec -- opsx-campaign accept-changes\nexec -- opsx-campaign --resume\n"
    );
    assert_eq!(
        git(&f.guest, &["merge-base", &correction, "HEAD"]),
        correction
    );
    assert_eq!(
        fs::read_to_string(f.guest.join("input")).unwrap(),
        "VM implementation"
    );
    assert_eq!(
        fs::read_to_string(f.guest.join("extra")).unwrap(),
        "maintainer correction"
    );
    assert_eq!(
        fs::read_to_string(f.guest.join(".git/opsx-build/last-run.json")).unwrap(),
        "guest checkpoint"
    );
    assert_eq!(
        fs::read_to_string(f.guest.join("guest.log")).unwrap(),
        "only in VM"
    );
    assert!(!f.guest.join("private.log").exists());
    assert_eq!(
        fs::read(f.guest.join("new inputs/check")).unwrap(),
        [0u8, 255, 39]
    );
    assert_ne!(
        fs::metadata(f.guest.join("new inputs/check"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert_eq!(
        fs::read_link(f.guest.join("new inputs/link")).unwrap(),
        Path::new("check")
    );
    let head = git(&f.guest, &["rev-parse", "HEAD"]);
    let repeat = f.lima("sync", &[]);
    assert!(
        repeat.status.success(),
        "{}",
        String::from_utf8_lossy(&repeat.stderr)
    );
    assert_eq!(git(&f.guest, &["rev-parse", "HEAD"]), head);
    fs::remove_file(f.host.join("new inputs/link")).unwrap();
    fs::write(f.host.join("extra"), "staged").unwrap();
    git(&f.host, &["add", "extra"]);
    fs::write(f.host.join("extra"), "working").unwrap();
    let edit = f.lima("sync", &[]);
    assert!(
        edit.status.success(),
        "{}",
        String::from_utf8_lossy(&edit.stderr)
    );
    assert!(!f.guest.join("new inputs/link").exists());
    assert_eq!(git(&f.guest, &["show", ":extra"]), "staged");
    assert_eq!(
        fs::read_to_string(f.guest.join("extra")).unwrap(),
        "working"
    );
    fs::remove_file(f.host.join(".git/opsx-build/last-run.json")).unwrap();
    f.ok();
    assert_eq!(git(&f.host, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        fs::read_to_string(f.host.join("input")).unwrap(),
        "VM implementation"
    );
    assert_eq!(git(&f.host, &["show", ":extra"]), "staged");
}

#[test]
fn sync_conflict_or_active_campaign_prevents_changes_and_resume() {
    let f = Fixture::new();
    f.ok();
    fs::write(f.host.join("input"), "host correction").unwrap();
    fs::remove_file(f.host.join("extra")).unwrap();
    fs::write(f.guest.join("input"), "guest correction").unwrap();
    for busy in [true, false] {
        if busy {
            fs::write(f.root.join("busy"), "").unwrap();
        } else {
            fs::remove_file(f.root.join("busy")).unwrap();
        }
        let sync = f.lima("sync", &["--resume"]);
        let message = String::from_utf8_lossy(&sync.stderr);
        assert!(!sync.status.success(), "{message}");
        assert!(
            message.contains(if busy {
                "active guest campaign"
            } else {
                "sync conflicts at input"
            }),
            "{message}"
        );
        assert_eq!(
            fs::read_to_string(f.guest.join("extra")).unwrap(),
            "original"
        );
        assert_eq!(
            fs::read_to_string(f.guest.join("input")).unwrap(),
            "guest correction"
        );
        assert!(!f.root.join("calls").exists());
    }
    fs::write(f.guest.join("input"), "original").unwrap();
    fs::write(f.root.join("fail-accept-changes"), "").unwrap();
    let rejected = f.lima("sync", &["--resume"]);
    assert!(!rejected.status.success());
    assert!(!f.guest.join("extra").exists());
    assert_eq!(
        fs::read_to_string(f.root.join("calls")).unwrap(),
        "exec -- opsx-campaign accept-changes\n"
    );
}

#[test]
fn sync_handles_file_directory_transitions_without_following_guest_links() {
    let f = Fixture::new();
    f.ok();
    fs::remove_file(f.host.join("input")).unwrap();
    fs::create_dir(f.host.join("input")).unwrap();
    fs::write(f.host.join("input/nested"), "new").unwrap();
    let first = f.lima("sync", &[]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.guest.join("input/nested")).unwrap(),
        "new"
    );
    fs::write(f.guest.join("input/local.log"), "guest only").unwrap();
    fs::remove_dir_all(f.host.join("input")).unwrap();
    symlink("extra", f.host.join("input")).unwrap();
    let blocked = f.lima("sync", &[]);
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("input/local.log"));
    assert_eq!(
        fs::read_to_string(f.guest.join("input/local.log")).unwrap(),
        "guest only"
    );
    fs::remove_file(f.guest.join("input/local.log")).unwrap();
    let lock = f.guest.join(".git/index.lock");
    fs::write(&lock, "another process").unwrap();
    let busy = f.lima("sync", &[]);
    assert!(!busy.status.success());
    assert_eq!(fs::read_to_string(&lock).unwrap(), "another process");
    fs::write(&lock, "opsx-build lima sync\n").unwrap();
    let retry = f.lima("sync", &[]);
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert_eq!(
        fs::read_link(f.guest.join("input")).unwrap(),
        Path::new("extra")
    );
    assert_eq!(
        fs::read_to_string(f.guest.join("extra")).unwrap(),
        "original"
    );
}

#[test]
fn sync_from_an_older_export_respects_ignored_files_and_untracked_deletions() {
    let f = Fixture::new();
    for name in ["packet-notes", "cache.log"] {
        fs::write(f.guest.join(name), name).unwrap();
    }
    f.ok();
    let anchor = fs::read_dir(f.root.join("state/opsx-build/lima"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension() == Some(std::ffi::OsStr::new("export-base")))
        .unwrap();
    let mut old: serde_json::Value = serde_json::from_slice(&fs::read(&anchor).unwrap()).unwrap();
    old.as_object_mut().unwrap().remove("inputs");
    old.as_object_mut().unwrap().remove("index_tree");
    fs::write(anchor, serde_json::to_vec(&old).unwrap()).unwrap();
    for name in ["packet-notes", "cache.log"] {
        fs::remove_file(f.host.join(name)).unwrap();
    }
    let sent = f.lima("sync", &[]);
    assert!(
        sent.status.success(),
        "{}",
        String::from_utf8_lossy(&sent.stderr)
    );
    assert!(!f.guest.join("packet-notes").exists());
    assert_eq!(
        fs::read_to_string(f.guest.join("cache.log")).unwrap(),
        "cache.log"
    );
}

#[test]
fn a_failed_guest_ref_update_rolls_back_work_and_can_be_retried() {
    let f = Fixture::new();
    f.ok();
    fs::write(f.host.join("input"), "accepted change").unwrap();
    fs::remove_file(f.host.join("extra")).unwrap();
    fs::write(f.host.join("new"), "added").unwrap();
    git(&f.host, &["add", "-A"]);
    git(&f.host, &["commit", "-qm", "correction"]);
    let reference = git(&f.guest, &["symbolic-ref", "HEAD"]);
    let lock = f.guest.join(".git").join(format!("{reference}.lock"));
    fs::write(&lock, "other git operation").unwrap();
    let original = git(&f.guest, &["rev-parse", "HEAD"]);
    let failed = f.lima("sync", &["--resume"]);
    assert!(!failed.status.success());
    assert_eq!(
        fs::read_to_string(f.guest.join("input")).unwrap(),
        "original"
    );
    assert_eq!(
        fs::read_to_string(f.guest.join("extra")).unwrap(),
        "original"
    );
    assert!(!f.guest.join("new").exists());
    assert!(!f.root.join("calls").exists());
    assert_eq!(git(&f.guest, &["rev-parse", "HEAD"]), original);
    assert_eq!(fs::read_to_string(&lock).unwrap(), "other git operation");
    fs::remove_file(lock).unwrap();
    let retry = f.lima("sync", &[]);
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert_eq!(
        git(&f.guest, &["rev-parse", "HEAD"]),
        git(&f.host, &["rev-parse", "HEAD"])
    );
    assert_eq!(git(&f.guest, &["status", "--porcelain"]), "");
}

#[test]
fn cleanup_requires_a_successful_export_and_a_current_guest_and_host_copy() {
    let f = Fixture::new();
    let cleanup = || f.lima("cleanup", &[]);
    assert!(!cleanup().status.success());
    assert!(!f.root.join("lifecycle").exists());
    fs::create_dir(f.guest.join("a")).unwrap();
    fs::write(f.guest.join("a/nested"), "result").unwrap();
    symlink("a/nested", f.guest.join("a-link")).unwrap();
    f.ok();
    fs::write(f.guest.join("result.log"), "new ignored receipt").unwrap();
    let stale = cleanup();
    assert!(
        String::from_utf8_lossy(&stale.stderr).contains("guest has changed"),
        "{stale:?}"
    );
    assert!(!f.root.join("lifecycle").exists());
    f.ok();
    fs::remove_file(f.host.join("result.log")).unwrap();
    let missing = cleanup();
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("exported checkout has changed"),
        "{missing:?}"
    );
    fs::write(f.host.join("result.log"), "new ignored receipt").unwrap();
    fs::write(f.root.join("busy"), "").unwrap();
    assert!(!cleanup().status.success());
    assert!(!f.root.join("lifecycle").exists());
    fs::remove_file(f.root.join("busy")).unwrap();
    let cleaned = cleanup();
    assert!(cleaned.status.success(), "{cleaned:?}");
    assert_eq!(
        fs::read_to_string(f.root.join("lifecycle")).unwrap(),
        "stop\ndelete\n"
    );
    assert_eq!(
        fs::read_to_string(f.host.join("result.log")).unwrap(),
        "new ignored receipt"
    );
    assert!(cleanup().status.success());
    assert!(!f.lima("shell", &[]).status.success());
    assert!(
        !fs::read_dir(f.root.join("state/opsx-build/lima"))
            .unwrap()
            .any(|e| matches!(
                e.unwrap().path().extension().and_then(|s| s.to_str()),
                Some("json" | "export-base" | "checkout")
            ))
    );
}

#[test]
fn export_cleanup_supports_both_destinations_and_stops_on_export_failure() {
    let f = Fixture::new();
    f.ok();
    fs::write(f.host.join("input"), "host change").unwrap();
    fs::write(f.guest.join("input"), "guest change").unwrap();
    assert!(!f.export(&["--cleanup"]).status.success());
    assert!(!f.lima("cleanup", &[]).status.success());
    assert!(!f.root.join("lifecycle").exists());
    let path = f.root.join("snapshot");
    let separate = f.export(&[path.to_str().unwrap(), "--cleanup"]);
    assert!(separate.status.success(), "{separate:?}");
    assert_eq!(
        fs::read_to_string(path.join("input")).unwrap(),
        "guest change"
    );
    assert_eq!(
        fs::read_to_string(f.host.join("input")).unwrap(),
        "host change"
    );

    let g = Fixture::new();
    let done = g.export(&["--cleanup"]);
    assert!(done.status.success(), "{done:?}");
    assert!(g.root.join("deleted").exists());
}

#[test]
fn cleanup_failures_are_retryable_and_never_target_the_builder() {
    let f = Fixture::new();
    f.ok();
    let record = fs::read_dir(f.root.join("state/opsx-build/lima"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|ext| ext == "json"))
        .unwrap();
    let original = fs::read(&record).unwrap();
    let mut damaged: serde_json::Value = serde_json::from_slice(&original).unwrap();
    damaged["base"] = damaged["instance"].clone();
    fs::write(&record, serde_json::to_vec(&damaged).unwrap()).unwrap();
    assert!(!f.lima("cleanup", &[]).status.success());
    assert!(!f.root.join("lifecycle").exists());
    fs::write(&record, &original).unwrap();
    for failure in ["fail-list", "fail-stop", "fail-delete"] {
        fs::write(f.root.join(failure), "").unwrap();
        assert!(!f.lima("cleanup", &[]).status.success());
        assert_eq!(fs::read(&record).unwrap(), original);
        assert!(!f.root.join("deleted").exists());
        fs::remove_file(f.root.join(failure)).unwrap();
    }
    assert_eq!(
        fs::read_to_string(f.root.join("lifecycle")).unwrap(),
        "stop\nstop\ndelete\n"
    );
    assert!(f.lima("cleanup", &[]).status.success());
    assert!(!record.exists());
}

#[test]
fn cleanup_protects_unexported_guest_refs_and_missing_snapshots() {
    let f = Fixture::new();
    git(&f.guest, &["branch", "guest-only"]);
    let inplace = f.export(&["--cleanup"]);
    assert!(!inplace.status.success());
    assert!(
        String::from_utf8_lossy(&inplace.stderr).contains("refs/heads/guest-only"),
        "{inplace:?}"
    );
    assert!(!f.root.join("lifecycle").exists());
    let path = f.root.join("snapshot");
    assert!(f.export(&[path.to_str().unwrap()]).status.success());
    assert_eq!(
        git(&path, &["rev-parse", "guest-only"]),
        git(&f.guest, &["rev-parse", "HEAD"])
    );
    let elsewhere = f.root.join("moved");
    fs::rename(&path, &elsewhere).unwrap();
    assert!(!f.lima("cleanup", &[]).status.success());
    assert!(!f.root.join("lifecycle").exists());
    fs::rename(elsewhere, path).unwrap();
    assert!(f.lima("cleanup", &[]).status.success());
}

#[test]
fn an_open_campaign_shell_excludes_both_cleanup_forms() {
    let f = Fixture::new();
    f.ok();
    let dir = f.root.join("state/opsx-build/lima");
    let state = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|ext| ext == "json"))
        .unwrap();
    let shell = fs::File::create(state.with_extension("shell-lock")).unwrap();
    shell.try_lock_shared().unwrap();
    for (cmd, args) in [("cleanup", &[][..]), ("export", &["--cleanup"][..])] {
        let out = f.lima(cmd, args);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("close the campaign shell"));
    }
    assert!(!f.root.join("lifecycle").exists());
    drop(shell);
    assert!(f.lima("cleanup", &[]).status.success());
}
