use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{bootstrap::render_project_config, git::metadata_dir, process::CommandSpec, ui::Ui};

const ASSOCIATION_SCHEMA: u32 = 1;
const ASSOCIATION_FILE: &str = "sidecar.json";
const TARGET_FILE: &str = ".opsx-build/target.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarAssociation {
    schema_version: u32,
    pub store_id: String,
    pub planning_root: PathBuf,
    pub product_root: PathBuf,
}

#[derive(Debug)]
pub struct SidecarPlan {
    pub association: SidecarAssociation,
    context: String,
    needs_setup: bool,
}

#[derive(Debug, Deserialize)]
struct StoreSetupOutput {
    store: Option<StoreSetupEntry>,
    #[serde(default)]
    status: Vec<StoreStatus>,
}

#[derive(Debug, Deserialize)]
struct StoreSetupEntry {
    id: String,
    root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct StoreStatus {
    message: String,
}

impl SidecarPlan {
    pub fn new(product_root: &Path, context_path: &Path, sidecar_root: &Path) -> Result<Self> {
        if !sidecar_root.is_absolute() {
            bail!(
                "sidecar root `{}` is not absolute; configure or pass an absolute path",
                sidecar_root.display()
            );
        }
        let context = fs::read_to_string(context_path).with_context(|| {
            format!(
                "could not read sidecar project context `{}`",
                context_path.display()
            )
        })?;
        if context.trim().is_empty() {
            bail!(
                "sidecar project context `{}` is empty",
                context_path.display()
            );
        }
        let store_id = store_id(product_root);
        let planning_root = sidecar_root.join(&store_id);
        let needs_setup = if planning_root.exists() {
            validate_recoverable_store(&planning_root, &store_id)?;
            false
        } else {
            true
        };
        Ok(Self {
            association: SidecarAssociation {
                schema_version: ASSOCIATION_SCHEMA,
                store_id,
                planning_root,
                product_root: product_root.to_path_buf(),
            },
            context,
            needs_setup,
        })
    }

    pub fn needs_setup(&self) -> bool {
        self.needs_setup
    }

    pub fn setup_command(&self) -> CommandSpec {
        CommandSpec::new("openspec", &self.association.product_root).args([
            "store".to_owned(),
            "setup".to_owned(),
            self.association.store_id.clone(),
            "--path".to_owned(),
            self.association.planning_root.display().to_string(),
            "--init-git".to_owned(),
            "--json".to_owned(),
        ])
    }

    pub fn init_command(&self) -> CommandSpec {
        CommandSpec::new("openspec", &self.association.product_root).args([
            "init".to_owned(),
            "--tools".to_owned(),
            "claude".to_owned(),
            "--no-animation".to_owned(),
            self.association.planning_root.display().to_string(),
        ])
    }

    pub fn validate_setup_output(&self, json: &str) -> Result<()> {
        let output: StoreSetupOutput = serde_json::from_str(json)
            .context("invalid JSON from `openspec store setup --json`")?;
        let Some(store) = output.store else {
            let details = output
                .status
                .iter()
                .map(|status| status.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            bail!(
                "OpenSpec did not create the sidecar store{}",
                if details.is_empty() {
                    String::new()
                } else {
                    format!(": {details}")
                }
            );
        };
        if store.id != self.association.store_id || store.root != self.association.planning_root {
            bail!(
                "OpenSpec created unexpected store `{}` at `{}` instead of `{}` at `{}`",
                store.id,
                store.root.display(),
                self.association.store_id,
                self.association.planning_root.display()
            );
        }
        Ok(())
    }

    pub fn finish<U: Ui>(&self, ui: &U) -> Result<()> {
        let config_path = self.association.planning_root.join("openspec/config.yaml");
        fs::write(&config_path, render_project_config(&self.context))
            .with_context(|| format!("could not write `{}`", config_path.display()))?;

        let target_path = self.association.planning_root.join(TARGET_FILE);
        fs::create_dir_all(target_path.parent().expect("target metadata has a parent"))?;
        fs::write(&target_path, serde_json::to_vec_pretty(&self.association)?)
            .with_context(|| format!("could not write `{}`", target_path.display()))?;

        write_association(&self.association.product_root, &self.association, ui)
    }
}

fn validate_recoverable_store(planning_root: &Path, expected_id: &str) -> Result<()> {
    let metadata_path = planning_root.join(".openspec-store/store.yaml");
    let metadata = fs::read_to_string(&metadata_path).with_context(|| {
        format!(
            "planned sidecar path `{}` already exists but is not an identifiable native OpenSpec store; preserve it and pass a different `--sidecar-root`",
            planning_root.display()
        )
    })?;
    let actual_id = metadata.lines().find_map(|line| {
        line.trim()
            .strip_prefix("id:")
            .map(str::trim)
            .filter(|value| !value.is_empty())
    });
    if actual_id != Some(expected_id) {
        bail!(
            "planned sidecar path `{}` belongs to OpenSpec store `{}` rather than expected store `{expected_id}`",
            planning_root.display(),
            actual_id.unwrap_or("<unknown>")
        );
    }
    Ok(())
}

pub fn default_sidecar_root() -> Result<PathBuf> {
    if let Some(root) = env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(root).join("opsx-build/sidecars"));
    }
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .context("cannot choose a sidecar location because HOME is unset")?;
    Ok(PathBuf::from(home).join(".local/share/opsx-build/sidecars"))
}

pub fn load_association<U: Ui>(product_root: &Path, ui: &U) -> Result<Option<SidecarAssociation>> {
    let path = metadata_dir(product_root, ui)?.join(ASSOCIATION_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("could not read sidecar association `{}`", path.display())
            });
        }
    };
    let association: SidecarAssociation = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid sidecar association `{}`", path.display()))?;
    if association.schema_version != ASSOCIATION_SCHEMA {
        bail!(
            "unsupported sidecar association schema {} in `{}`",
            association.schema_version,
            path.display()
        );
    }
    if association.product_root != product_root {
        bail!(
            "sidecar association targets `{}` rather than this checkout `{}`",
            association.product_root.display(),
            product_root.display()
        );
    }
    let target_path = association.planning_root.join(TARGET_FILE);
    let target: SidecarAssociation =
        serde_json::from_slice(&fs::read(&target_path).with_context(|| {
            format!("could not read sidecar target `{}`", target_path.display())
        })?)
        .with_context(|| format!("invalid sidecar target `{}`", target_path.display()))?;
    if target != association {
        bail!(
            "sidecar identity mismatch between `{}` and `{}`",
            path.display(),
            target_path.display()
        );
    }
    Ok(Some(association))
}

fn write_association<U: Ui>(
    product_root: &Path,
    association: &SidecarAssociation,
    ui: &U,
) -> Result<()> {
    let path = metadata_dir(product_root, ui)?.join(ASSOCIATION_FILE);
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(association)?)
        .with_context(|| format!("could not write `{}`", temporary.display()))?;
    fs::rename(&temporary, &path).with_context(|| format!("could not replace `{}`", path.display()))
}

fn store_id(product_root: &Path) -> String {
    let name = product_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    let slug = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned();
    let slug = if slug.is_empty() { "project" } else { &slug };
    let hash = product_root
        .to_string_lossy()
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    format!("opsx-{slug}-{:08x}", hash as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn plans_a_stable_external_native_store() {
        let root = env::temp_dir().join(format!("opsx-sidecar-test-{}", Uuid::new_v4()));
        let product = root.join("Ceph Checkout");
        let data = root.join("data");
        let context = root.join("context.md");
        fs::create_dir_all(&product).unwrap();
        fs::write(&context, "# Bounded Ceph change\n").unwrap();

        let plan = SidecarPlan::new(&product, &context, &data).unwrap();

        assert!(plan.association.store_id.starts_with("opsx-ceph-checkout-"));
        assert_eq!(
            plan.association.planning_root,
            data.join(&plan.association.store_id)
        );
        assert!(plan.setup_command().display().contains("store setup"));
        assert!(plan.setup_command().display().contains("--init-git"));
        assert!(plan.init_command().display().contains("--tools claude"));
        assert!(plan.needs_setup());
        let setup_json = serde_json::json!({
            "store": {
                "id": plan.association.store_id,
                "root": plan.association.planning_root,
            },
            "status": [],
        });
        plan.validate_setup_output(&setup_json.to_string()).unwrap();

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovers_an_unassociated_native_store_after_partial_setup() {
        let root = env::temp_dir().join(format!("opsx-sidecar-test-{}", Uuid::new_v4()));
        let product = root.join("product");
        let data = root.join("data");
        let context = root.join("context.md");
        fs::create_dir_all(&product).unwrap();
        fs::write(&context, "bounded change").unwrap();
        let first = SidecarPlan::new(&product, &context, &data).unwrap();
        let metadata_dir = first.association.planning_root.join(".openspec-store");
        fs::create_dir_all(&metadata_dir).unwrap();
        fs::write(
            metadata_dir.join("store.yaml"),
            format!("version: 1\nid: {}\n", first.association.store_id),
        )
        .unwrap();

        let recovered = SidecarPlan::new(&product, &context, &data).unwrap();
        assert!(!recovered.needs_setup());
        assert_eq!(recovered.association, first.association);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_a_store_setup_response_without_a_store() {
        let root = env::temp_dir().join(format!("opsx-sidecar-test-{}", Uuid::new_v4()));
        let product = root.join("product");
        let data = root.join("data");
        let context = root.join("context.md");
        fs::create_dir_all(&product).unwrap();
        fs::write(&context, "bounded change").unwrap();
        let plan = SidecarPlan::new(&product, &context, &data).unwrap();

        let error = plan
            .validate_setup_output(
                r#"{"store":null,"status":[{"message":"Git identity is missing"}]}"#,
            )
            .unwrap_err();
        assert!(error.to_string().contains("Git identity is missing"));

        fs::remove_dir_all(root).unwrap();
    }
}
