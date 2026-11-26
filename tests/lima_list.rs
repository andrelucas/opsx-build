#![cfg(unix)]
use serde_json::json;
use std::{
    env, fs,
    os::unix::fs::{PermissionsExt, symlink},
    process::Command,
};

#[test]
fn list_from_outside_git_shows_live_activity_without_waking_stopped_vms() {
    let root = env::temp_dir().join(format!("opsx-list-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_opsx-build"))
            .args(["lima", "list"])
            .current_dir(&root)
            .env("XDG_STATE_HOME", &root)
            .env(
                "PATH",
                format!("{}:{}", root.display(), env::var("PATH").unwrap()),
            )
            .output()
            .unwrap()
    };
    let empty = run();
    assert!(empty.status.success());
    assert!(
        String::from_utf8(empty.stdout)
            .unwrap()
            .contains("No registered campaign VMs")
    );
    let directory = root.join("opsx-build/lima");
    fs::create_dir_all(&directory).unwrap();
    for (instance, checkout) in [
        ("vm-active", Some("/first/reader")),
        ("vm-stopped", Some("/second/reader")),
        ("vm-done", Some("/packets/finished")),
        ("vm-idle", None),
        ("vm-offline", Some("/packets/offline")),
        ("vm-gone", Some("/packets/gone")),
    ] {
        let record = directory.join(format!("{instance}.json"));
        fs::write(&record, json!({"instance":instance, "base":"base", "resources":{}, "guest_dir":"/guest/packet"}).to_string()).unwrap();
        if let Some(repo) = checkout {
            symlink(repo, record.with_extension("checkout")).unwrap();
        }
    }
    let lima = root.join("limactl");
    fs::write(&lima, r#"#!/bin/sh
case "$1" in
list) printf '%s\n' 'base Stopped' 'vm-active Running' 'vm-stopped Stopped' 'vm-done Running' 'vm-idle Running' 'vm-offline Running';;
shell)
    echo "$5" >> probes
    test "$2 $3 $4" = "--tty=false --workdir /guest/packet" || exit 2
    case "$5" in
        vm-active) echo '[true,"verify"]';;
        vm-done) echo '[false,"done"]';;
        vm-idle) echo '[false,"apply"]';;
        vm-offline) sleep 20;;
        *) exit 1;;
    esac;;
*) exit 2;;
esac
"#).unwrap();
    fs::set_permissions(lima, fs::Permissions::from_mode(0o755)).unwrap();
    let result = run();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output = String::from_utf8(result.stdout).unwrap();
    assert!(String::from_utf8_lossy(&result.stderr).contains("vm-offline: status probe timed out"));
    let rows: Vec<_> = output
        .lines()
        .skip(1)
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    assert_eq!(rows[0], "/first/reader vm-active Running Running (verify)");
    assert_eq!(rows.len(), 5);
    for expected in [
        "/second/reader vm-stopped Stopped Not running",
        "finished vm-done Running Done",
        "/guest/packet (guest) vm-idle Running Idle (last: apply)",
        "offline vm-offline Running Unknown",
    ] {
        assert!(rows.iter().any(|row| row == expected), "{output}");
    }
    let probes = fs::read_to_string(root.join("probes")).unwrap();
    assert_eq!(probes.lines().count(), 4);
    assert!(!probes.contains("vm-stopped"));
    fs::remove_dir_all(root).unwrap();
}
