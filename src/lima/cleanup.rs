//! Delete only the registered campaign clone covered by a successful export.
use super::*;

#[derive(Serialize, Deserialize)]
pub(super) struct Receipt {
    instance: String,
    destination: PathBuf,
    full: bool,
    guest_signature: String,
    host_signature: String,
}

impl Receipt {
    pub(super) fn new(
        instance: &str,
        destination: &Path,
        full: bool,
        (guest_signature, host_signature): (String, String),
    ) -> Result<Self> {
        let destination = destination.canonicalize()?;
        Ok(Self {
            instance: instance.to_owned(),
            destination,
            full,
            guest_signature,
            host_signature,
        })
    }
}

pub(super) fn remove(campaign: &Campaign, state: &Path) -> Result<()> {
    let receipt = campaign
        .exported
        .as_ref()
        .context("no verified export for this clone; run 'opsx-build lima export --cleanup'")?;
    let instance = &campaign.instance;
    ensure!(
        instance == &receipt.instance
            && instance != &campaign.base
            && instance
                .strip_prefix("opsx-")
                .is_some_and(|id| id.len() == 16 && id.bytes().all(|c| c.is_ascii_hexdigit())),
        "refusing to delete an instance that is not the recorded campaign clone"
    );
    ensure!(
        !state.with_extension("export-staging").try_exists()?,
        "finish the pending in-place export before cleanup"
    );

    if let Some(status) = instance_status(&list_instances()?, instance) {
        ensure!(
            matches!(status, "Running" | "Stopped"),
            "cannot clean up a clone in state {status}"
        );
        let dir = campaign
            .guest_dir
            .as_deref()
            .context("campaign import is incomplete")?;
        let host = collect::signature(&receipt.destination, receipt.full).with_context(|| {
            format!(
                "cannot verify the export at {}; export again before cleanup",
                receipt.destination.display()
            )
        })?;
        ensure!(
            host == receipt.host_signature,
            "the exported checkout has changed; export again before cleanup"
        );
        ensure_guest_idle(instance, dir)?;
        let guest = output(
            shell(instance, dir).args([
                "python3",
                "-c",
                include_str!("checkout_signature.py"),
                if receipt.full { "full" } else { "campaign" },
            ]),
            "verifying the guest checkout before cleanup",
        )?;
        ensure!(
            guest == receipt.guest_signature,
            "the guest has changed since export; export again before cleanup"
        );
        if !receipt.full {
            // In-place export keeps host Git administration. A full snapshot is
            // needed if the guest has an additional branch, stash or other ref.
            let refs = shell(instance, dir)
                .args(["git", "for-each-ref", "--format=%(refname) %(objectname)"])
                .output()?;
            ensure!(refs.status.success(), "could not verify guest Git refs");
            collect::check_exported_refs(&receipt.destination, &refs.stdout)?;
        }
        ensure_guest_idle(instance, dir)?;
        checked(
            limactl().args(["stop", "--tty=false", instance]),
            "stopping the exported campaign clone",
        )?;
        checked(
            limactl().args(["delete", "--tty=false", instance]),
            "deleting the stopped campaign clone",
        )?;
        ensure!(
            instance_status(&list_instances()?, instance).is_none(),
            "Lima still lists {instance}; kept its campaign registration"
        );
    }

    // Keep the record until deletion succeeds. If it succeeded just before an
    // interruption, a retry finds the instance absent and finishes unregistering.
    let anchor = collect::baseline(state, instance);
    if anchor.try_exists()? {
        fs::remove_file(anchor)?;
    }
    fs::remove_file(state)?;
    println!(
        "Cleaned up {instance}. Export: {}",
        receipt.destination.display()
    );
    Ok(())
}
