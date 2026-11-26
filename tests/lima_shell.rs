#![cfg(unix)]

use std::{env, fs, os::unix::fs::PermissionsExt, process::Command};

use sha2::{Digest, Sha256};

#[test]
fn shell_opens_the_recorded_checkout_while_run_holds_the_lock() {
    let root = env::temp_dir().join(format!("opsx-lima-shell-{}", uuid::Uuid::new_v4()));
    let repo = root.join("repo");
    fs::create_dir_all(repo.join("subdir")).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );

    let bin = root.join("limactl");
    fs::write(&bin, "#!/bin/sh\nprintf '%s\\n' \"$@\"\nexit 23\n").unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let invoke = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_opsx-build"))
            .arg("lima")
            .args(args)
            .current_dir(repo.join("subdir"))
            .env("XDG_STATE_HOME", &root)
            .env(
                "PATH",
                format!("{}:{}", root.display(), env::var("PATH").unwrap()),
            )
            .output()
            .unwrap()
    };

    let state_dir = root.join("opsx-build/lima");
    fs::create_dir_all(&state_dir).unwrap();
    let key = Sha256::digest(repo.canonicalize().unwrap().as_os_str().as_encoded_bytes());
    let state = state_dir.join(format!("{key:x}.json"));
    let lock = fs::File::create(state.with_extension("lock")).unwrap();
    lock.try_lock().unwrap();
    fs::write(&state, r#"{"instance":"test-vm","base":"base","resources":{},"guest_dir":"/guest/campaign with spaces"}"#).unwrap();

    let shell = invoke(&["shell"]);
    assert!(String::from_utf8_lossy(&shell.stderr).contains(&format!(
        "Lima campaign: {} → test-vm:/guest/campaign with spaces",
        repo.canonicalize().unwrap().display()
    )));
    assert_eq!(
        shell.status.code(),
        Some(23),
        "{}",
        String::from_utf8_lossy(&shell.stderr)
    );
    assert_eq!(
        String::from_utf8(shell.stdout).unwrap(),
        "shell\n--start\n--workdir\n/guest/campaign with spaces\ntest-vm\n"
    );
    let second_run = invoke(&["run", "--", "execute"]);
    assert!(!second_run.status.success());
    assert!(String::from_utf8_lossy(&second_run.stderr).contains("another Lima command is active"));
    let resize = invoke(&["resize", "--cpus", "4"]);
    assert!(!resize.status.success());
    assert!(String::from_utf8_lossy(&resize.stderr).contains("pause the run before resizing"));
    fs::remove_dir_all(root).unwrap();
}
