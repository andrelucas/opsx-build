use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    Explore,
    Propose,
    ProposalCommit,
    Apply,
    Verify,
    Repair,
    Archive,
    FinalCommit,
    Complete,
}

impl Stage {
    pub fn number(self) -> usize {
        match self {
            Self::Explore => 1,
            Self::Propose => 2,
            Self::ProposalCommit => 3,
            Self::Apply => 4,
            Self::Verify | Self::Repair => 5,
            Self::Archive => 6,
            Self::FinalCommit | Self::Complete => 7,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Explore => "Explore",
            Self::Propose => "Propose",
            Self::ProposalCommit => "Proposal commit",
            Self::Apply => "Apply",
            Self::Verify => "Verify",
            Self::Repair => "Repair",
            Self::Archive => "Archive",
            Self::FinalCommit => "Completion commit",
            Self::Complete => "Complete",
        }
    }

    pub fn after_ready(self) -> Result<Self> {
        match self {
            Self::Explore => Ok(Self::Propose),
            Self::Propose => Ok(Self::ProposalCommit),
            Self::ProposalCommit => Ok(Self::Apply),
            Self::Apply | Self::Repair => Ok(Self::Verify),
            Self::Archive => Ok(Self::FinalCommit),
            Self::FinalCommit => Ok(Self::Complete),
            _ => bail!("stage {self:?} does not accept READY"),
        }
    }

    pub fn after_verified(self) -> Result<Self> {
        if self == Self::Verify {
            Ok(Self::Archive)
        } else {
            bail!("stage {self:?} does not accept VERIFIED")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_has_seven_presented_stages() {
        let mut stage = Stage::Explore;
        stage = stage.after_ready().unwrap();
        stage = stage.after_ready().unwrap();
        stage = stage.after_ready().unwrap();
        stage = stage.after_ready().unwrap();
        stage = stage.after_verified().unwrap();
        stage = stage.after_ready().unwrap();
        stage = stage.after_ready().unwrap();
        assert_eq!(stage, Stage::Complete);
        assert_eq!(Stage::Repair.number(), 5);
        assert_eq!(Stage::FinalCommit.number(), 7);
    }

    #[test]
    fn rejects_wrong_terminal_signal_for_stage() {
        assert!(Stage::Verify.after_ready().is_err());
        assert!(Stage::Apply.after_verified().is_err());
    }
}
