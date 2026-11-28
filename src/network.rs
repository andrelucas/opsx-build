//! Maintainer-declared network requirements, checked in the runner's own network context.
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::ErrorKind,
    path::Path,
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{process::probe_output, ui::Ui};

pub const FILE_NAME: &str = "opsx-build.network.toml";

#[derive(Default, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Requirements {
    pub http_hosts: BTreeMap<String, BTreeSet<String>>,
}

impl Requirements {
    pub fn read(repo: &Path) -> Result<Self> {
        let path = repo.join(FILE_NAME);
        match fs::symlink_metadata(&path) {
            Ok(meta) => ensure!(meta.is_file(), "{FILE_NAME} must be a regular file"),
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        }
        let mut req: Self = toml::from_str(&fs::read_to_string(&path)?)
            .with_context(|| format!("invalid {FILE_NAME}"))?;
        for (host, probes) in &mut req.http_hosts {
            ensure!(
                !probes.is_empty(),
                "every declared HTTP host needs a probe URL"
            );
            *probes = probes
                .iter()
                .map(|raw| {
                    let url = endpoint(raw)?;
                    ensure!(
                        url.host_str() == Some(host.as_str()),
                        "probe URL must name its declared HTTP host {host}"
                    );
                    Ok(url.to_string())
                })
                .collect::<Result<_>>()?;
        }
        Ok(req)
    }

    pub fn is_empty(&self) -> bool {
        self.http_hosts.is_empty()
    }
}

fn endpoint(raw: &str) -> Result<Url> {
    let invalid = "network URLs must be literal HTTP(S) endpoints without credentials, wildcards, queries or fragments";
    ensure!(
        (raw.starts_with("https://") || raw.starts_with("http://"))
            && !raw.contains(['*', '\\'])
            && !raw.chars().any(|c| c.is_whitespace() || c.is_control()),
        "{invalid}"
    );
    let authority = raw.split_once("://").unwrap().1.split('/').next().unwrap();
    ensure!(
        !authority.is_empty() && !authority.contains('@'),
        "{invalid}"
    );
    let url = Url::parse(raw).context(invalid)?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "{invalid}"
    );
    Ok(url)
}

pub(crate) fn preflight(repo: &Path, ui: &dyn Ui, dry_run: bool) -> Result<bool> {
    let req = Requirements::read(repo)?;
    if let Some(path) = env::var_os("OPSX_BUILD_APPROVED_NETWORK") {
        let approved: Requirements = match fs::read(path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("invalid prepared network requirements")?
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Requirements::default(),
            Err(e) => return Err(e.into()),
        };
        ensure!(
            req == approved,
            "campaign network requirements differ from the prepared copy; run 'opsx-build lima sync' from the maintainer checkout"
        );
    }
    if req.is_empty() {
        return Ok(false);
    }
    for url in req.http_hosts.values().flatten() {
        ui.info(&format!("Network preflight · GET {url}"));
        if !dry_run {
            let result = probe_output(
                Command::new("curl").current_dir(repo).args([
                    "-q",
                    "--silent",
                    "--show-error",
                    "--globoff",
                    "--connect-timeout",
                    "4",
                    "--max-time",
                    "10",
                    "--output",
                    "/dev/null",
                    "--write-out",
                    "%{http_code}",
                    "--",
                    url,
                ]),
                Duration::from_secs(12),
            )
            .context("running the curl network probe")?;
            ensure!(
                result.status.success(),
                "network preflight failed for {url}: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            );
            let status = String::from_utf8(result.stdout)?;
            ensure!(
                status.starts_with('2'),
                "network preflight failed for {url}: HTTP {status} (expected 2xx)"
            );
        }
    }
    if !dry_run {
        ui.success("Campaign network preflight passed");
    }
    Ok(true)
}
