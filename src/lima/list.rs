//! Host inventory. A stopped clone must stay stopped during inspection.
use super::*;
use std::{collections::BTreeMap, os::unix::fs::symlink};

// The registration filename hashes the canonical checkout path. Keep its
// reverse lookup separately so older launchers cannot erase it on save.
pub(super) fn remember(state: &Path, repo: &Path) -> Result<()> {
    let link = state.with_extension("checkout");
    match symlink(repo, &link) {
        Err(error) if error.kind() == ErrorKind::AlreadyExists && fs::read_link(&link)? == repo => {
            Ok(())
        }
        result => {
            result.with_context(|| format!("recording the campaign checkout at {}", link.display()))
        }
    }
}

const PROBE: &str = r#"import json, os, subprocess
from pathlib import Path

repo = Path.cwd()
active = False
for line in subprocess.check_output(["ps", "-e", "-o", "pid=,comm="], text=True).splitlines():
    pid, name = line.split(None, 1)
    if name != "opsx-build":
        continue
    proc = Path("/proc") / pid
    try:
        cwd = (proc / "cwd").readlink()
        args = os.fsdecode((proc / "cmdline").read_bytes()).split("\0")
    except (FileNotFoundError, ProcessLookupError):
        continue
    for i, arg in enumerate(args):
        if arg == "--repo" and i + 1 < len(args):
            cwd = (cwd / args[i + 1]).resolve()
            break
        if arg.startswith("--repo="):
            cwd = (cwd / arg[7:]).resolve()
            break
    if cwd.is_relative_to(repo):
        active = True
        break

stage = None
for directory in ("opsx-build", "ospx-build"):
    try:
        stage = json.loads((repo / ".git" / directory / "last-run.json").read_bytes()).get("stage")
        break
    except FileNotFoundError:
        continue
    except (ValueError, AttributeError):
        break
print(json.dumps([active, stage]))
"#;

fn probe(instance: &str, dir: &str) -> Result<(bool, String)> {
    let reply = opsx_build::process::probe_output(
        limactl().args([
            "shell",
            "--tty=false",
            "--workdir",
            dir,
            instance,
            "python3",
            "-I",
            "-c",
            PROBE,
        ]),
        std::time::Duration::from_secs(4),
    )?;
    ensure!(
        reply.status.success(),
        "guest probe failed: {}",
        String::from_utf8_lossy(&reply.stderr).trim()
    );
    let (active, stage): (bool, Option<String>) = serde_json::from_slice(&reply.stdout)?;
    let stage = stage
        .as_deref()
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'));
    let text = match (active, stage) {
        (true, Some(s)) => format!("Running ({s})"),
        (true, None) => "Running".into(),
        (false, Some("done")) => "Done".into(),
        (false, Some(s)) => format!("Idle (last: {s})"),
        (false, None) => "Idle".into(),
    };
    Ok((active, text))
}

struct Row {
    checkout: Option<PathBuf>,
    name: String,
    instance: String,
    vm: String,
    active: bool,
    activity: String,
}

pub(super) fn show(directory: &Path) -> Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            println!("No registered campaign VMs.");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    let instances = list_instances()?;
    let mut rows = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let data = match fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == ErrorKind::NotFound => continue, // concurrent cleanup
            Err(e) => return Err(e.into()),
        };
        let campaign: Campaign = match serde_json::from_slice(&data) {
            Ok(record) => record,
            Err(e) => {
                eprintln!("Skipping invalid campaign record {}: {e}", path.display());
                continue;
            }
        };
        let Some(vm) = instance_status(&instances, &campaign.instance) else {
            continue;
        };
        let checkout = match fs::read_link(path.with_extension("checkout")) {
            Ok(repo) => Some(repo),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let name = match &checkout {
            Some(repo) => repo
                .file_name()
                .unwrap_or(repo.as_os_str())
                .to_string_lossy()
                .into_owned(),
            None => campaign.guest_dir.as_deref().map_or_else(
                || "(unknown checkout)".into(),
                |dir| format!("{dir} (guest)"),
            ),
        };
        let (active, activity) = match (vm, campaign.guest_dir.as_deref()) {
            ("Stopped", _) => (false, "Not running".into()),
            (_, None) => (false, "Import incomplete".into()),
            ("Running", Some(dir)) => match probe(&campaign.instance, dir) {
                Ok(activity) => activity,
                Err(e) => {
                    eprintln!("{}: {e}", campaign.instance);
                    (false, "Unknown".into())
                }
            },
            _ => (false, "Unknown".into()),
        };
        rows.push(Row {
            checkout,
            name,
            instance: campaign.instance,
            vm: vm.into(),
            active,
            activity,
        });
    }
    if rows.is_empty() {
        println!("No registered campaign VMs.");
        return Ok(());
    }
    let mut counts = BTreeMap::new();
    for row in &rows {
        *counts.entry(row.name.clone()).or_insert(0) += 1;
    }
    for row in &mut rows {
        if counts[&row.name] > 1
            && let Some(repo) = &row.checkout
        {
            row.name = repo.display().to_string();
        }
    }
    rows.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then(a.name.cmp(&b.name))
            .then(a.instance.cmp(&b.instance))
    });
    let width = rows
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap()
        .max("CAMPAIGN".len());
    println!(
        "{:<width$}  {:21}  {:8}  RUN STATUS",
        "CAMPAIGN", "INSTANCE", "VM"
    );
    for row in rows {
        println!(
            "{:<width$}  {:21}  {:8}  {}",
            row.name, row.instance, row.vm, row.activity
        );
    }
    Ok(())
}
