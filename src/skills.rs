use std::{fs, path::Path};

use anyhow::{Context, Result};

const BUNDLED_SKILLS: &[BundledSkill] = &[
    BundledSkill {
        name: "explore-unattended",
        contents: include_str!("../assets/claude-skills/explore-unattended/SKILL.md"),
    },
    BundledSkill {
        name: "propose-unattended",
        contents: include_str!("../assets/claude-skills/propose-unattended/SKILL.md"),
    },
];

struct BundledSkill {
    name: &'static str,
    contents: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillInstallAction {
    Installed,
    Updated,
    WouldInstall,
    WouldUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInstall {
    pub name: &'static str,
    pub action: SkillInstallAction,
}

pub fn ensure_unattended_skills(repo: &Path, dry_run: bool) -> Result<Vec<SkillInstall>> {
    let mut changes = Vec::new();
    for skill in BUNDLED_SKILLS {
        let path = repo
            .join(".claude/skills")
            .join(skill.name)
            .join("SKILL.md");
        let existing = match fs::read(&path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not read Claude skill `{}`", path.display()));
            }
        };
        if existing.as_deref() == Some(skill.contents.as_bytes()) {
            continue;
        }

        let action = match (dry_run, existing.is_some()) {
            (true, false) => SkillInstallAction::WouldInstall,
            (true, true) => SkillInstallAction::WouldUpdate,
            (false, false) => SkillInstallAction::Installed,
            (false, true) => SkillInstallAction::Updated,
        };
        if !dry_run {
            let parent = path.parent().expect("skill path has a parent");
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "could not create Claude skill directory `{}`",
                    parent.display()
                )
            })?;
            fs::write(&path, skill.contents)
                .with_context(|| format!("could not install Claude skill `{}`", path.display()))?;
        }
        changes.push(SkillInstall {
            name: skill.name,
            action,
        });
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temporary_repo(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ospx-skills-{label}-{}", Uuid::new_v4()))
    }

    #[test]
    fn bundled_skills_have_valid_frontmatter() {
        for skill in BUNDLED_SKILLS {
            assert!(skill.contents.starts_with("---\nname: "));
            assert!(
                skill
                    .contents
                    .lines()
                    .any(|line| line == format!("name: {}", skill.name))
            );
            assert!(skill.contents.contains("\ndescription: "));
            assert!(!skill.contents.starts_with("```"));
        }
        let propose = BUNDLED_SKILLS
            .iter()
            .find(|skill| skill.name == "propose-unattended")
            .unwrap();
        assert!(propose.contents.contains("DONE:"));
        assert!(propose.contents.contains("finish with DONE"));
        assert!(propose.contents.contains("Return DONE only before"));
    }

    #[test]
    fn installs_repairs_and_then_leaves_skills_unchanged() {
        let repo = temporary_repo("install");
        fs::create_dir_all(&repo).unwrap();

        let installed = ensure_unattended_skills(&repo, false).unwrap();
        assert_eq!(installed.len(), 2);
        assert!(
            installed
                .iter()
                .all(|change| change.action == SkillInstallAction::Installed)
        );

        let explore = repo.join(".claude/skills/explore-unattended/SKILL.md");
        fs::write(&explore, "stale").unwrap();
        let repaired = ensure_unattended_skills(&repo, false).unwrap();
        assert_eq!(
            repaired,
            [SkillInstall {
                name: "explore-unattended",
                action: SkillInstallAction::Updated,
            }]
        );
        assert!(ensure_unattended_skills(&repo, false).unwrap().is_empty());

        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn dry_run_reports_without_installing() {
        let repo = temporary_repo("dry-run");
        fs::create_dir_all(&repo).unwrap();

        let changes = ensure_unattended_skills(&repo, true).unwrap();
        assert_eq!(changes.len(), 2);
        assert!(
            changes
                .iter()
                .all(|change| change.action == SkillInstallAction::WouldInstall)
        );
        assert!(!repo.join(".claude").exists());

        fs::remove_dir_all(repo).unwrap();
    }
}
