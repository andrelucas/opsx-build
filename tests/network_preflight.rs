#![cfg(unix)]

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};

struct Campaign(PathBuf);

impl Campaign {
    fn new() -> Self {
        let root = env::temp_dir().join(format!("opsx-network-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        assert!(
            Command::new("git")
                .current_dir(&root)
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        Self(root)
    }

    fn write(&self, name: &str, contents: &str) {
        fs::write(self.0.join(name), contents).unwrap();
    }

    fn curl(&self, script: &str) {
        self.write("curl", script);
        fs::set_permissions(self.0.join("curl"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_opsx-build"))
            .current_dir(&self.0)
            .args(["--no-config", "--no-campaign-config"])
            .args(args)
            .env("OPSX_BUILD_APPROVED_NETWORK", self.0.join("prepared.json"))
            .env(
                "PATH",
                format!("{}:{}", self.0.display(), env::var("PATH").unwrap()),
            )
            .output()
            .unwrap()
    }
}

impl Drop for Campaign {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn preflight_launch_and_resume_check_the_current_endpoint_before_dispatch() {
    let repo = Campaign::new();
    repo.write(
        "opsx-build.network.toml",
        "[http_hosts]\n'example.org' = ['https://example.org:6443/readyz']",
    );
    repo.write(
        "prepared.json",
        r#"{"http_hosts":{"example.org":["https://example.org:6443/readyz"]}}"#,
    );
    repo.curl("#!/bin/sh\nprintf '%s\\n' \"$@\" > curl-args\nprintf 204");
    let ready = repo.run(&["preflight"]);
    assert!(
        ready.status.success(),
        "{}",
        String::from_utf8_lossy(&ready.stderr)
    );
    assert!(
        fs::read_to_string(repo.0.join("curl-args"))
            .unwrap()
            .ends_with("--\nhttps://example.org:6443/readyz\n")
    );
    repo.curl("#!/bin/sh\nprintf 403");
    for args in [&["preflight"][..], &["execute"], &["--resume"]] {
        let blocked = repo.run(args);
        assert!(!blocked.status.success());
        assert!(
            String::from_utf8_lossy(&blocked.stderr).contains("HTTP 403"),
            "{blocked:?}"
        );
        assert!(!repo.0.join(".git/opsx-build/last-run.json").exists());
    }
}

#[test]
fn guest_only_changes_and_invalid_urls_fail_before_probing() {
    let repo = Campaign::new();
    repo.curl("#!/bin/sh\ntouch called");
    repo.write(
        "opsx-build.network.toml",
        "[http_hosts]\n'unapproved.example' = ['https://unapproved.example/']",
    );
    let mismatch = repo.run(&["preflight"]);
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("lima sync"));
    for url in [
        "file:///tmp/f",
        "http:bad",
        "https://@host/",
        "https://user:secret@example.org/",
        "https://*.example.org/",
        "https://example.org/?token=x",
        "https://example.org/#fragment",
    ] {
        repo.write(
            "opsx-build.network.toml",
            &format!("[http_hosts]\n'example.org' = ['{url}']"),
        );
        let blocked = repo.run(&["preflight"]);
        assert!(!blocked.status.success());
        assert!(
            String::from_utf8_lossy(&blocked.stderr).contains("literal HTTP(S)"),
            "{blocked:?}"
        );
    }
    for (manifest, error) in [
        (
            "[http_hosts]\n'example.org' = ['https://other.example/']",
            "must name its declared HTTP host",
        ),
        ("[http_hosts]\n'example.org' = []", "needs a probe URL"),
        (
            "[http_host]\n'example.org' = []",
            "invalid opsx-build.network.toml",
        ),
    ] {
        repo.write("opsx-build.network.toml", manifest);
        let blocked = repo.run(&["preflight"]);
        assert!(!blocked.status.success());
        assert!(
            String::from_utf8_lossy(&blocked.stderr).contains(error),
            "{blocked:?}"
        );
    }
    assert!(!repo.0.join("called").exists());
}
