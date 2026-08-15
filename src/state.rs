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
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Ready,
    Verified,
    Retry,
    Committed,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowState {
    pub stage: Stage,
    pub verify_retries: u32,
    pub max_verify_retries: u32,
}

impl WorkflowState {
    pub fn new(max_verify_retries: u32) -> Self {
        Self {
            stage: Stage::Explore,
            verify_retries: 0,
            max_verify_retries,
        }
    }

    pub fn resume(stage: Stage, verify_retries: u32, max_verify_retries: u32) -> Result<Self> {
        if verify_retries > max_verify_retries {
            bail!(
                "resume metadata records {verify_retries} verification retries, exceeding the configured limit of {max_verify_retries}"
            );
        }
        Ok(Self {
            stage,
            verify_retries,
            max_verify_retries,
        })
    }

    pub fn advance(&mut self, event: Event) -> Result<Stage> {
        if event == Event::Blocked {
            self.stage = Stage::Blocked;
            return Ok(self.stage);
        }

        self.stage = match (self.stage, event) {
            (Stage::Explore, Event::Ready) => Stage::Propose,
            (Stage::Propose, Event::Ready) => Stage::ProposalCommit,
            (Stage::ProposalCommit, Event::Committed) => Stage::Apply,
            (Stage::Apply, Event::Ready) => Stage::Verify,
            (Stage::Verify, Event::Verified) => Stage::Archive,
            (Stage::Verify, Event::Retry) if self.verify_retries < self.max_verify_retries => {
                self.verify_retries += 1;
                Stage::Repair
            }
            (Stage::Verify, Event::Retry) => {
                bail!(
                    "BLOCKED: verification still requires repair after {} configured retries",
                    self.max_verify_retries
                )
            }
            (Stage::Repair, Event::Ready) => Stage::Verify,
            (Stage::Archive, Event::Ready) => Stage::FinalCommit,
            (Stage::FinalCommit, Event::Committed) => Stage::Complete,
            (stage, event) => bail!("invalid workflow transition: {stage:?} + {event:?}"),
        };
        Ok(self.stage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_happy_path() {
        let mut state = WorkflowState::new(3);
        let events = [
            Event::Ready,
            Event::Ready,
            Event::Committed,
            Event::Ready,
            Event::Verified,
            Event::Ready,
            Event::Committed,
        ];
        for event in events {
            state.advance(event).unwrap();
        }
        assert_eq!(state.stage, Stage::Complete);
        assert_eq!(state.verify_retries, 0);
    }

    #[test]
    fn loops_through_repair_then_verification() {
        let mut state = WorkflowState {
            stage: Stage::Verify,
            verify_retries: 0,
            max_verify_retries: 2,
        };
        assert_eq!(state.advance(Event::Retry).unwrap(), Stage::Repair);
        assert_eq!(state.advance(Event::Ready).unwrap(), Stage::Verify);
        assert_eq!(state.advance(Event::Retry).unwrap(), Stage::Repair);
        assert_eq!(state.advance(Event::Ready).unwrap(), Stage::Verify);
        assert!(state.advance(Event::Retry).is_err());
    }

    #[test]
    fn blocks_from_any_active_stage() {
        let mut state = WorkflowState::new(1);
        assert_eq!(state.advance(Event::Blocked).unwrap(), Stage::Blocked);
    }

    #[test]
    fn resumes_from_planning_or_implementation_stages() {
        let resumed = WorkflowState::resume(Stage::Apply, 1, 3).unwrap();
        assert_eq!(resumed.stage, Stage::Apply);
        assert_eq!(resumed.verify_retries, 1);
        assert_eq!(
            WorkflowState::resume(Stage::Propose, 0, 3).unwrap().stage,
            Stage::Propose
        );
        assert!(WorkflowState::resume(Stage::Verify, 4, 3).is_err());
    }
}
