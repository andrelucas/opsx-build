use console::Style;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};

use crate::stream::StreamItem;

#[derive(Clone, Copy)]
pub(crate) enum TextFormat {
    Plain,
    Markdown,
    Tool,
}

impl TextFormat {
    pub(crate) fn for_item(item: &StreamItem) -> Self {
        match item {
            StreamItem::Assistant(_)
            | StreamItem::OpenCode(_)
            | StreamItem::Codex(_)
            | StreamItem::Reasoning(_)
            | StreamItem::Subagent(_) => Self::Markdown,
            StreamItem::Tool(_) => Self::Tool,
            _ => Self::Plain,
        }
    }
}

/// Format terminal presentation only; redirected text keeps its source syntax.
pub(crate) fn render(text: &str, format: TextFormat, styled: bool) -> String {
    if !styled || matches!(format, TextFormat::Plain) {
        return text.to_owned();
    }
    match format {
        TextFormat::Markdown => markdown(text),
        TextFormat::Tool => {
            let text = safe_text(text);
            let Some((name, detail)) = text.split_once(": ") else {
                return text;
            };
            format!(
                "{} {}",
                style().yellow().bold().apply_to(format!("{name}:")),
                style().cyan().apply_to(detail)
            )
        }
        TextFormat::Plain => unreachable!(),
    }
}

fn style() -> Style {
    Style::new().force_styling(true)
}

fn safe_text(text: &str) -> String {
    text.chars()
        .filter_map(|character| match character {
            '\t' => Some(' '),
            '\n' => Some('\n'),
            character if character.is_control() => None,
            character => Some(character),
        })
        .collect()
}

/// Keep source layout and unsupported syntax, replacing only recognized markup.
fn markdown(text: &str) -> String {
    let mut output = String::new();
    let mut cursor = 0;
    let mut current = style();
    let mut parents = Vec::new();
    for (event, range) in Parser::new(text).into_offset_iter() {
        match event {
            Event::Start(Tag::Strong | Tag::Emphasis) => {
                append(&mut output, &text[cursor..range.start], &current);
                parents.push(current.clone());
                let strong = matches!(event, Event::Start(Tag::Strong));
                current = if strong {
                    current.bold()
                } else {
                    current.italic()
                };
                cursor = range.start + if strong { 2 } else { 1 };
            }
            Event::End(TagEnd::Strong | TagEnd::Emphasis) => {
                let strong = matches!(event, Event::End(TagEnd::Strong));
                let end = range.end - if strong { 2 } else { 1 };
                append(&mut output, &text[cursor..end], &current);
                cursor = range.end;
                current = parents.pop().unwrap();
            }
            Event::Start(Tag::Heading { .. }) => {
                append(&mut output, &text[cursor..range.start], &current);
                parents.push(current.clone());
                current = current.bold();
                let source = &text[range.clone()];
                let content = source.trim_start_matches('#').trim_start_matches(' ');
                cursor = range.start + source.len() - content.len();
            }
            Event::End(TagEnd::Heading(_)) => {
                append(&mut output, &text[cursor..range.end], &current);
                cursor = range.end;
                current = parents.pop().unwrap();
            }
            Event::Text(ref value) | Event::Code(ref value) => {
                let mut gap = &text[cursor..range.start];
                // The parser's range for an escaped punctuation character starts
                // after its backslash; that backslash is markup, too.
                if matches!(event, Event::Text(_))
                    && gap.ends_with('\\')
                    && value.starts_with(|character: char| character.is_ascii_punctuation())
                {
                    gap = &gap[..gap.len() - 1];
                }
                append(&mut output, gap, &current);
                let value_style = if matches!(event, Event::Code(_)) {
                    current.clone().cyan()
                } else {
                    current.clone()
                };
                append(&mut output, value, &value_style);
                cursor = range.end;
            }
            _ => {}
        }
    }
    append(&mut output, &text[cursor..], &current);
    output
}

fn append(output: &mut String, text: &str, style: &Style) {
    // Each physical line must reset styling before the next source label is drawn.
    for part in text.split_inclusive('\n') {
        let line = part.strip_suffix('\n').unwrap_or(part);
        if !line.is_empty() {
            // Sanitize after parsing as well: entities can decode into controls.
            output.push_str(&style.apply_to(safe_text(line)).to_string());
        }
        if part.ends_with('\n') {
            output.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use console::{measure_text_width, strip_ansi_codes, truncate_str};

    use super::*;

    #[test]
    fn renders_reasoning_headings_nested_emphasis_and_literal_code() {
        let rendered = render(
            "## Checking retries\n\n**Keep *this* and `**/*.rs` literal.**\n- _Next_: `cargo test`",
            TextFormat::Markdown,
            true,
        );
        assert_eq!(
            strip_ansi_codes(&rendered),
            "Checking retries\n\nKeep this and **/*.rs literal.\n- Next: cargo test"
        );
        assert!(rendered.contains("\x1b[1m"));
        assert!(rendered.contains("\x1b[3m"));
        assert!(rendered.contains("\x1b[36m"));
    }

    #[test]
    fn preserves_links_fences_escapes_identifiers_and_incomplete_markup() {
        let text = "[**Docs**](https://example.test/a_b)\n\n```sh\necho '**literal**'\n```\n\nfile_name.rs \\*literal\\* **incomplete…";
        let rendered = render(text, TextFormat::Markdown, true);
        assert_eq!(
            strip_ansi_codes(&rendered),
            "[Docs](https://example.test/a_b)\n\n```sh\necho '**literal**'\n```\n\nfile_name.rs *literal* **incomplete…"
        );
    }

    #[test]
    fn keeps_nested_links_and_multiline_emphasis_readable() {
        for (source, expected) in [
            ("***both***", "both"),
            (
                "**[Docs](https://example.test)**",
                "[Docs](https://example.test)",
            ),
            ("**first\nsecond**", "first\nsecond"),
            (
                "Use ``a ` b`` and foo_bar_baz.",
                "Use a ` b and foo_bar_baz.",
            ),
            ("\\**literal**", "*literal*"),
            ("_emphasis_ &amp; **bold**", "emphasis & bold"),
        ] {
            let rendered = render(source, TextFormat::Markdown, true);
            assert_eq!(strip_ansi_codes(&rendered), expected, "{source}");
            for line in rendered.lines().filter(|line| line.contains('\x1b')) {
                assert!(line.contains("\x1b[0m"), "missing reset: {line:?}");
            }
        }
    }

    #[test]
    fn tool_details_and_non_prose_events_are_literal() {
        for text in [
            "Shell: rg '**/*.rs' file_name `pwd`",
            "Bash: echo '*value*'",
            "Read: src/my_file.rs",
            "Shell exited with code 1",
        ] {
            assert_eq!(
                strip_ansi_codes(&render(text, TextFormat::Tool, true)),
                text
            );
        }
        for item in [
            StreamItem::ToolResult("**literal output**".into()),
            StreamItem::Raw("{\"text\":\"**raw**\"}".into()),
        ] {
            let text = match &item {
                StreamItem::ToolResult(text) | StreamItem::Raw(text) => text,
                _ => unreachable!(),
            };
            assert_eq!(render(text, TextFormat::for_item(&item), true), *text);
        }
    }

    #[test]
    fn redirected_output_keeps_original_markdown_without_ansi() {
        let text = "**Checking** `cargo test`";
        assert_eq!(render(text, TextFormat::Markdown, false), text);
        assert_eq!(render(text, TextFormat::Tool, false), text);
    }

    #[test]
    fn truncation_counts_visible_unicode_columns_and_keeps_resets() {
        let rendered = render("**检查结果** then `cargo test`", TextFormat::Markdown, true);
        let truncated = truncate_str(&rendered, 6, "…");
        assert_eq!(strip_ansi_codes(&truncated), "检查…");
        assert!(measure_text_width(&truncated) <= 6);
        assert!(truncated.contains("\x1b[0m"));
    }

    #[test]
    fn source_terminal_controls_are_not_interpreted_as_styling() {
        for source in ["**safe**\x1b[2J", "**safe**&#27;[2J"] {
            let rendered = render(source, TextFormat::Markdown, true);
            assert_eq!(strip_ansi_codes(&rendered), "safe[2J");
            assert!(!rendered.contains("\x1b[2J"));
        }
    }
}
