use clap::ValueEnum;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum StreamControl {
    #[default]
    None,
    Compact,
    Context,
    Inject(String),
    Interrupt,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum StreamFilter {
    #[default]
    Activity,
    Full,
    Raw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamItem {
    Assistant(String),
    Subagent(String),
    Tool(String),
    ToolResult(String),
    Lifecycle(String),
    Raw(String),
}

pub fn filter_line(line: &str, filter: StreamFilter) -> Vec<StreamItem> {
    if filter == StreamFilter::Raw {
        return vec![StreamItem::Raw(line.to_owned())];
    }

    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return vec![StreamItem::Lifecycle(format!(
            "Unparsed Claude output: {line}"
        ))];
    };

    match value.get("type").and_then(Value::as_str) {
        Some("assistant") => assistant_items(&value),
        Some("user") if filter == StreamFilter::Full => tool_result_items(&value),
        Some("system")
            if value.get("subtype").and_then(Value::as_str) == Some("compact_boundary") =>
        {
            vec![StreamItem::Lifecycle(compaction_summary(&value))]
        }
        Some("system") if filter == StreamFilter::Full => {
            let subtype = value
                .get("subtype")
                .and_then(Value::as_str)
                .unwrap_or("event");
            vec![StreamItem::Lifecycle(format!("Claude system: {subtype}"))]
        }
        Some("result") if filter == StreamFilter::Full => {
            let subtype = value
                .get("subtype")
                .and_then(Value::as_str)
                .unwrap_or("complete");
            vec![StreamItem::Lifecycle(format!("Claude result: {subtype}"))]
        }
        Some(kind) if filter == StreamFilter::Full => {
            vec![StreamItem::Lifecycle(format!("Claude event: {kind}"))]
        }
        _ => Vec::new(),
    }
}

/// Keep `/context` useful when ordinary Claude activity is hidden.
pub fn context_report_items(line: &str) -> Vec<StreamItem> {
    filter_line(line, StreamFilter::Activity)
        .into_iter()
        .filter(|item| {
            matches!(item, StreamItem::Assistant(text) if text.trim_start().starts_with("## Context Usage"))
        })
        .collect()
}

fn compaction_summary(value: &Value) -> String {
    let metadata = value.get("compact_metadata").unwrap_or(value);
    let before = metadata.get("pre_tokens").and_then(Value::as_u64);
    let after = metadata.get("post_tokens").and_then(Value::as_u64);
    let trigger = metadata.get("trigger").and_then(Value::as_str);
    match (before, after) {
        (Some(before), Some(after)) => {
            format!("Claude compacted context: {before} → {after} tokens")
        }
        (Some(before), None) => match trigger {
            Some(trigger) => format!("Claude compacted context at {before} tokens ({trigger})"),
            None => format!("Claude compacted context at {before} tokens"),
        },
        _ => "Claude compacted context".to_owned(),
    }
}

fn assistant_items(value: &Value) -> Vec<StreamItem> {
    let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    let subagent = value
        .get("parent_tool_use_id")
        .is_some_and(|parent| !parent.is_null());
    content
        .iter()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text") => block.get("text").and_then(Value::as_str).map(|text| {
                if subagent {
                    StreamItem::Subagent(text.to_owned())
                } else {
                    StreamItem::Assistant(text.to_owned())
                }
            }),
            Some("tool_use") => {
                let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                let summary = block
                    .get("input")
                    .map(tool_input_summary)
                    .unwrap_or_default();
                let text = if summary.is_empty() {
                    name.to_owned()
                } else {
                    format!("{name}: {summary}")
                };
                Some(StreamItem::Tool(text))
            }
            _ => None,
        })
        .collect()
}

fn tool_result_items(value: &Value) -> Vec<StreamItem> {
    let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|block| {
            block
                .get("content")
                .and_then(content_text)
                .map(StreamItem::ToolResult)
        })
        .collect()
}

fn content_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_owned());
    }
    value.as_array().map(|blocks| {
        blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn tool_input_summary(input: &Value) -> String {
    for key in [
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "skill",
        "description",
    ] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            return one_line(value);
        }
    }
    input
        .as_object()
        .filter(|object| !object.is_empty())
        .map(|_| one_line(&input.to_string()))
        .unwrap_or_default()
}

fn one_line(value: &str) -> String {
    const LIMIT: usize = 240;
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= LIMIT {
        return compact;
    }
    let prefix: String = compact.chars().take(LIMIT - 1).collect();
    format!("{prefix}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_keeps_assistant_text_and_concise_tool_calls() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"I found it."},{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#;
        assert_eq!(
            filter_line(line, StreamFilter::Activity),
            [
                StreamItem::Assistant("I found it.".to_owned()),
                StreamItem::Tool("Bash: cargo test".to_owned())
            ]
        );
    }

    #[test]
    fn tool_results_are_full_only() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"tests passed"}]}}"#;
        assert!(filter_line(line, StreamFilter::Activity).is_empty());
        assert_eq!(
            filter_line(line, StreamFilter::Full),
            [StreamItem::ToolResult("tests passed".to_owned())]
        );
    }

    #[test]
    fn raw_returns_the_original_jsonl() {
        let line = r#"{"type":"system","subtype":"init"}"#;
        assert_eq!(
            filter_line(line, StreamFilter::Raw),
            [StreamItem::Raw(line.to_owned())]
        );
    }

    #[test]
    fn forwarded_assistant_text_is_identified_as_subagent_output() {
        let line = r#"{"type":"assistant","parent_tool_use_id":"tool-1","message":{"content":[{"type":"text","text":"Subagent finding"}]}}"#;
        assert_eq!(
            filter_line(line, StreamFilter::Activity),
            [StreamItem::Subagent("Subagent finding".to_owned())]
        );
    }

    #[test]
    fn compaction_boundary_is_visible_in_activity_mode() {
        let line = r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"pre_tokens":21583,"post_tokens":842}}"#;
        assert_eq!(
            filter_line(line, StreamFilter::Activity),
            [StreamItem::Lifecycle(
                "Claude compacted context: 21583 → 842 tokens".to_owned()
            )]
        );
    }

    #[test]
    fn context_report_remains_visible_without_general_activity() {
        let line = r###"{"type":"assistant","message":{"model":"<synthetic>","content":[{"type":"text","text":"## Context Usage\n\n**Tokens:** 21k / 262k"}]},"parent_tool_use_id":null}"###;
        assert_eq!(
            context_report_items(line),
            [StreamItem::Assistant(
                "## Context Usage\n\n**Tokens:** 21k / 262k".to_owned()
            )]
        );

        let ordinary = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Working on it"}]},"parent_tool_use_id":null}"#;
        assert!(context_report_items(ordinary).is_empty());
    }
}
