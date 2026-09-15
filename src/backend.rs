use std::fmt;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
pub enum SessionMode {
    New { id: SessionId, name: Option<String> },
    Resume { id: SessionId },
}

impl SessionMode {
    pub fn id(&self) -> &SessionId {
        match self {
            Self::New { id, .. } | Self::Resume { id } => id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageSignal {
    Ready,
    Done,
    TooLarge,
    Verified,
    Retry,
    Blocked,
    Replanned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageProtocol {
    Ready,
    Worker,
    Propose,
    Verify,
    Frontier,
    TerminalReview,
}

impl StageProtocol {
    pub fn terminal_values(self) -> &'static str {
        match self {
            Self::Ready => "READY or BLOCKED",
            Self::Worker => "READY, TOO_LARGE, or BLOCKED",
            Self::Propose => "READY, DONE, TOO_LARGE, or BLOCKED",
            Self::Verify => "VERIFIED, RETRY, or BLOCKED",
            Self::Frontier => "REPLANNED or BLOCKED",
            Self::TerminalReview => "READY, REPLANNED, or BLOCKED",
        }
    }

    pub(crate) fn json_schema(self) -> &'static str {
        match self {
            Self::Ready => {
                r#"{"type":"object","properties":{"opsx_status":{"type":"string","enum":["READY","BLOCKED"]},"summary":{"type":"string","description":"Concise stage result. For BLOCKED, include the exact blocker and evidence needed for a human decision."}},"required":["opsx_status","summary"],"additionalProperties":false}"#
            }
            Self::Worker => {
                r#"{"type":"object","properties":{"opsx_status":{"type":"string","enum":["READY","TOO_LARGE","BLOCKED"]},"summary":{"type":"string","description":"Concise worker-stage result. TOO_LARGE means the assigned slice cannot reliably fit one bounded worker-model change; explain why and propose an ordered decomposition. For BLOCKED, include the exact external decision or unavailable input."}},"required":["opsx_status","summary"],"additionalProperties":false}"#
            }
            Self::Propose => {
                r#"{"type":"object","properties":{"opsx_status":{"type":"string","enum":["READY","DONE","TOO_LARGE","BLOCKED"]},"summary":{"type":"string","description":"Concise proposal result. DONE means the requested objective is already satisfied. TOO_LARGE means the assigned slice requires decomposition before a local worker can reliably implement it. Neither outcome may create or modify OpenSpec artifacts. Include decomposition advice for TOO_LARGE and the exact external blocker for BLOCKED."}},"required":["opsx_status","summary"],"additionalProperties":false}"#
            }
            Self::Verify => {
                r#"{"type":"object","properties":{"opsx_status":{"type":"string","enum":["VERIFIED","RETRY","BLOCKED"]},"summary":{"type":"string","description":"Concise stage result. For RETRY, include every actionable verification finding needed by the repair stage. For BLOCKED, include the exact blocker."}},"required":["opsx_status","summary"],"additionalProperties":false}"#
            }
            Self::Frontier => {
                r#"{"type":"object","properties":{"opsx_status":{"type":"string","enum":["REPLANNED","BLOCKED"]},"summary":{"type":"string","description":"Concise frontier-planning result. REPLANNED means the oversized agenda slice was replaced by a smaller first slice plus one or more ordered hierarchical descendants and committed. BLOCKED means safe subdivision requires a genuine external decision."}},"required":["opsx_status","summary"],"additionalProperties":false}"#
            }
            Self::TerminalReview => {
                r#"{"type":"object","properties":{"opsx_status":{"type":"string","enum":["READY","REPLANNED","BLOCKED"]},"summary":{"type":"string","description":"Concise terminal acceptance review. READY means the project is ready for its final acceptance slice and no repository state changed. REPLANNED means one or more bounded remediation slices were inserted before the unchanged terminal gate and committed. BLOCKED means safe review or remediation requires a genuine external decision."}},"required":["opsx_status","summary"],"additionalProperties":false}"#
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct StageResult {
    pub text: String,
    pub session_id: Option<String>,
    pub signal: StageSignal,
}

pub trait AgentBackend {
    fn name(&self) -> &'static str;

    fn invoke(
        &self,
        session: SessionMode,
        prompt: &str,
        activity: &str,
        protocol: StageProtocol,
    ) -> Result<StageResult>;

    fn rename_session(&self, session_id: &SessionId, name: &str) -> Result<()>;

    fn compact_session(&self, session_id: &SessionId, completed_phase: &str) -> Result<()>;
}
