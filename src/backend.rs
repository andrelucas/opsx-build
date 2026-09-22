use std::{error::Error, fmt};

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
                r#"{"type":"object","properties":{"opsx_status":{"type":"string","enum":["READY","TOO_LARGE","BLOCKED"]},"summary":{"type":"string","description":"Concise worker-stage result. For TOO_LARGE, cite a concrete capacity or scope constraint, what was inspected or attempted, why ordered tasks within the assigned change cannot resolve it, and the minimum necessary decomposition. Multiple implementation steps or tests alone are insufficient. For BLOCKED, include the exact external decision or unavailable input."}},"required":["opsx_status","summary"],"additionalProperties":false}"#
            }
            Self::Propose => {
                r#"{"type":"object","properties":{"opsx_status":{"type":"string","enum":["READY","DONE","TOO_LARGE","BLOCKED"]},"summary":{"type":"string","description":"Concise proposal result. DONE means the requested objective is already satisfied. For TOO_LARGE, cite repository evidence of a concrete capacity or scope constraint, why ordered tasks within the assigned change cannot resolve it, and the minimum necessary decomposition. Multiple implementation steps or tests alone are insufficient. Neither outcome may create or modify OpenSpec artifacts. Include the exact external blocker for BLOCKED."}},"required":["opsx_status","summary"],"additionalProperties":false}"#
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

#[derive(Debug)]
pub struct MissingTerminalResult(String);

impl MissingTerminalResult {
    pub fn new(message: &str, last_response: Option<&str>) -> Self {
        const EXCERPT_CHARS: usize = 320;

        let mut diagnostic = message.to_owned();
        if let Some(response) = last_response.map(str::trim).filter(|text| !text.is_empty()) {
            let mut excerpt = response.chars().take(EXCERPT_CHARS).collect::<String>();
            if response.chars().count() > EXCERPT_CHARS {
                excerpt.push('…');
            }
            diagnostic.push_str(". Last agent response: ");
            diagnostic.push_str(&excerpt.replace('\n', " "));
        }
        Self(diagnostic)
    }
}

impl fmt::Display for MissingTerminalResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for MissingTerminalResult {}

pub fn is_missing_terminal_result(error: &anyhow::Error) -> bool {
    error.downcast_ref::<MissingTerminalResult>().is_some()
}

pub fn parse_terminal_signal(text: &str) -> Option<StageSignal> {
    text.lines().rev().find_map(|line| {
        line.trim()
            .strip_prefix("OPSX_STATUS:")
            .or_else(|| line.trim().strip_prefix("OSPX_STATUS:"))
            .and_then(parse_status_value)
    })
}

pub fn parse_status_value(status: &str) -> Option<StageSignal> {
    match status.trim().to_ascii_uppercase().as_str() {
        "READY" => Some(StageSignal::Ready),
        "DONE" => Some(StageSignal::Done),
        "TOO_LARGE" => Some(StageSignal::TooLarge),
        "VERIFIED" => Some(StageSignal::Verified),
        "RETRY" => Some(StageSignal::Retry),
        "BLOCKED" => Some(StageSignal::Blocked),
        "REPLANNED" => Some(StageSignal::Replanned),
        _ => None,
    }
}

pub(crate) fn stage_result_from_text(
    text: &str,
    session_id: Option<String>,
    backend: &str,
) -> Result<StageResult> {
    let structured = serde_json::from_str::<serde_json::Value>(text.trim()).ok();
    let structured_signal = structured.as_ref().and_then(|value| {
        value
            .get("opsx_status")
            .or_else(|| value.get("ospx_status"))
            .and_then(serde_json::Value::as_str)
            .and_then(parse_status_value)
    });
    let signal = structured_signal
        .or_else(|| parse_terminal_signal(text))
        .ok_or_else(|| {
            MissingTerminalResult::new(
                &format!(
                    "{backend} response contained neither structured `opsx_status` output nor an `OPSX_STATUS` terminal marker"
                ),
                Some(text),
            )
        })?;
    let result_text = structured
        .as_ref()
        .and_then(|value| value.get("summary"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(text)
        .to_owned();
    Ok(StageResult {
        text: result_text,
        session_id,
        signal,
    })
}

pub trait AgentBackend {
    fn name(&self) -> &'static str;

    /// Resolve a server-assigned session ID before checkpointing planning work.
    fn prepare_session(&self, session: SessionMode) -> Result<SessionMode> {
        Ok(session)
    }

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
