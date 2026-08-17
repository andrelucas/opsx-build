use std::{
    collections::VecDeque,
    io::{self, Write},
    time::{Duration, Instant},
};

use console::truncate_str;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};

use crate::stream::StreamItem;

const DISCLOSURE_ROW: u16 = 3;
const MAX_DISPLAY_LINES: usize = 20_000;
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageView {
    pub current: usize,
    pub total: usize,
    pub title: String,
}

#[derive(Debug, Clone)]
struct DisplayLine {
    text: String,
    color: Color,
}

pub(crate) struct StreamDashboard {
    repo: String,
    stage: Option<StageView>,
    activity: String,
    lines: VecDeque<DisplayLine>,
    total_lines: usize,
    dropped_lines: usize,
    expanded: bool,
    scroll_from_bottom: usize,
    spinner_index: usize,
    last_tick: Instant,
    last_draw: Instant,
    dirty: bool,
    active: bool,
}

impl StreamDashboard {
    pub(crate) fn enter(
        repo: String,
        stage: Option<StageView>,
        activity: String,
    ) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut output = io::stderr().lock();
        if let Err(error) = execute!(output, EnterAlternateScreen, EnableMouseCapture, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }

        let now = Instant::now();
        let mut dashboard = Self {
            repo,
            stage,
            activity,
            lines: VecDeque::new(),
            total_lines: 0,
            dropped_lines: 0,
            expanded: true,
            scroll_from_bottom: 0,
            spinner_index: 0,
            last_tick: now,
            last_draw: now,
            dirty: true,
            active: true,
        };
        dashboard.draw()?;
        Ok(dashboard)
    }

    pub(crate) fn push(&mut self, item: &StreamItem) {
        let (label, color, text) = match item {
            StreamItem::Assistant(text) => ("claude", Color::Cyan, text),
            StreamItem::Subagent(text) => ("agent", Color::Blue, text),
            StreamItem::Tool(text) => ("tool", Color::Yellow, text),
            StreamItem::ToolResult(text) => ("result", Color::DarkGrey, text),
            StreamItem::Lifecycle(text) => ("event", Color::Magenta, text),
            StreamItem::Raw(text) => ("json", Color::DarkGrey, text),
        };
        for (index, line) in text.lines().enumerate() {
            let prefix = if index == 0 {
                format!("{label:>7} ")
            } else {
                "        ".to_owned()
            };
            self.push_line(DisplayLine {
                text: format!("{prefix}{}", sanitize(line)),
                color,
            });
        }
    }

    fn push_line(&mut self, line: DisplayLine) {
        self.total_lines += 1;
        if self.scroll_from_bottom > 0 {
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(1);
        }
        self.lines.push_back(line);
        if self.lines.len() > MAX_DISPLAY_LINES {
            self.lines.pop_front();
            self.dropped_lines += 1;
        }
        self.dirty = true;
    }

    pub(crate) fn poll(&mut self) -> bool {
        let mut cancel = false;
        for _ in 0..32 {
            match event::poll(Duration::ZERO) {
                Ok(true) => match event::read() {
                    Ok(event) => {
                        if self.handle_event(event) {
                            cancel = true;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) | Err(_) => break,
            }
        }

        let now = Instant::now();
        if now.duration_since(self.last_tick) >= Duration::from_millis(90) {
            self.spinner_index = (self.spinner_index + 1) % SPINNER.len();
            self.last_tick = now;
            self.dirty = true;
        }
        if self.dirty && now.duration_since(self.last_draw) >= Duration::from_millis(30) {
            let _ = self.draw();
        }
        cancel
    }

    fn handle_event(&mut self, event: Event) -> bool {
        match event {
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('c') =>
            {
                return true;
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('o') => {
                    self.expanded = !self.expanded;
                    self.dirty = true;
                }
                KeyCode::Esc => {
                    self.expanded = false;
                    self.dirty = true;
                }
                KeyCode::Up => self.scroll_up(1),
                KeyCode::Down => self.scroll_down(1),
                KeyCode::PageUp => self.scroll_up(self.body_height().max(1)),
                KeyCode::PageDown => self.scroll_down(self.body_height().max(1)),
                KeyCode::Home => {
                    self.scroll_from_bottom = usize::MAX;
                    self.dirty = true;
                }
                KeyCode::End => {
                    self.scroll_from_bottom = 0;
                    self.dirty = true;
                }
                _ => {}
            },
            Event::Mouse(mouse)
                if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                    && mouse.row == DISCLOSURE_ROW =>
            {
                self.expanded = !self.expanded;
                self.dirty = true;
            }
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollUp => self.scroll_up(3),
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => self.scroll_down(3),
            Event::Resize(_, _) => self.dirty = true,
            _ => {}
        }
        false
    }

    fn scroll_up(&mut self, amount: usize) {
        if self.expanded {
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(amount);
            self.dirty = true;
        }
    }

    fn scroll_down(&mut self, amount: usize) {
        if self.expanded {
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(amount);
            self.dirty = true;
        }
    }

    fn body_height(&self) -> usize {
        terminal::size()
            .map(|(_, height)| height.saturating_sub(5) as usize)
            .unwrap_or_default()
    }

    fn draw(&mut self) -> io::Result<()> {
        let (width, height) = terminal::size()?;
        if width == 0 || height == 0 {
            return Ok(());
        }
        let mut output = io::stderr().lock();
        queue!(output, MoveTo(0, 0), Clear(ClearType::All))?;

        draw_row(
            &mut output,
            0,
            width,
            &format!("ospx-build  {}", self.repo),
            Color::Cyan,
            true,
        )?;
        if height > 1 {
            let stage = self
                .stage
                .as_ref()
                .map(|stage| format!("[{}/{}] {}", stage.current, stage.total, stage.title))
                .unwrap_or_else(|| "[workflow]".to_owned());
            draw_row(
                &mut output,
                1,
                width,
                &format!("{stage}  {} {}", SPINNER[self.spinner_index], self.activity),
                Color::White,
                true,
            )?;
        }
        if height > 2 {
            draw_row(
                &mut output,
                2,
                width,
                &"─".repeat(width as usize),
                Color::DarkGrey,
                false,
            )?;
        }
        if height > DISCLOSURE_ROW {
            let arrow = if self.expanded { "▼" } else { "▶" };
            let line_label = if self.total_lines == 1 {
                "line"
            } else {
                "lines"
            };
            let history = if self.dropped_lines == 0 {
                format!("{} {line_label}", self.total_lines)
            } else {
                format!(
                    "{} {line_label} · {} older hidden",
                    self.total_lines, self.dropped_lines
                )
            };
            let scroll = if self.expanded && self.scroll_from_bottom > 0 {
                format!(" · {} from latest", self.scroll_from_bottom)
            } else {
                String::new()
            };
            draw_row(
                &mut output,
                DISCLOSURE_ROW,
                width,
                &format!("{arrow} Claude output · {history}{scroll} · Enter/Space/click to toggle"),
                Color::Green,
                true,
            )?;
        }

        if self.expanded && height > 5 {
            let body_height = height.saturating_sub(5) as usize;
            let maximum_scroll = self.lines.len().saturating_sub(body_height);
            self.scroll_from_bottom = self.scroll_from_bottom.min(maximum_scroll);
            let start = self
                .lines
                .len()
                .saturating_sub(body_height + self.scroll_from_bottom);
            for (index, line) in self.lines.iter().skip(start).take(body_height).enumerate() {
                draw_row(
                    &mut output,
                    4 + index as u16,
                    width,
                    &line.text,
                    line.color,
                    false,
                )?;
            }
        }

        if height > 4 {
            draw_row(
                &mut output,
                height - 1,
                width,
                "↑↓/PgUp/PgDn scroll · End follows latest · Ctrl-C stops current command",
                Color::DarkGrey,
                false,
            )?;
        }
        output.flush()?;
        self.last_draw = Instant::now();
        self.dirty = false;
        Ok(())
    }

    pub(crate) fn leave(&mut self) {
        if !self.active {
            return;
        }
        let mut output = io::stderr().lock();
        let _ = execute!(output, Show, DisableMouseCapture, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
        self.active = false;
    }
}

impl Drop for StreamDashboard {
    fn drop(&mut self) {
        self.leave();
    }
}

fn draw_row<W: Write>(
    output: &mut W,
    row: u16,
    width: u16,
    text: &str,
    color: Color,
    bold: bool,
) -> io::Result<()> {
    let text = truncate_str(text, width as usize, "…");
    queue!(
        output,
        MoveTo(0, row),
        Clear(ClearType::CurrentLine),
        SetForegroundColor(color)
    )?;
    if bold {
        queue!(output, SetAttribute(Attribute::Bold))?;
    }
    queue!(
        output,
        Print(text),
        SetAttribute(Attribute::Reset),
        ResetColor
    )?;
    Ok(())
}

fn sanitize(text: &str) -> String {
    text.chars()
        .filter_map(|character| match character {
            '\t' => Some(' '),
            character if character.is_control() => None,
            character => Some(character),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::IsTerminal;

    use super::*;

    fn dashboard() -> StreamDashboard {
        let now = Instant::now();
        StreamDashboard {
            repo: "/repo".to_owned(),
            stage: None,
            activity: "working".to_owned(),
            lines: VecDeque::new(),
            total_lines: 0,
            dropped_lines: 0,
            expanded: true,
            scroll_from_bottom: 0,
            spinner_index: 0,
            last_tick: now,
            last_draw: now,
            dirty: false,
            active: false,
        }
    }

    #[test]
    fn stream_items_become_label_prefixed_lines() {
        let mut dashboard = dashboard();
        dashboard.push(&StreamItem::Assistant("first\nsecond".to_owned()));
        assert_eq!(dashboard.total_lines, 2);
        assert_eq!(dashboard.lines[0].text, " claude first");
        assert_eq!(dashboard.lines[1].text, "        second");
    }

    #[test]
    fn appending_while_scrolled_preserves_view_position() {
        let mut dashboard = dashboard();
        dashboard.scroll_from_bottom = 4;
        dashboard.push(&StreamItem::Tool("cargo test".to_owned()));
        assert_eq!(dashboard.scroll_from_bottom, 5);
    }

    #[test]
    fn sanitizes_terminal_control_characters() {
        assert_eq!(sanitize("safe\u{1b}[2J\ttext"), "safe[2J text");
    }

    #[test]
    fn disclosure_controls_toggle_and_ctrl_c_cancels() {
        let mut dashboard = dashboard();
        assert!(!dashboard.handle_event(Event::Key(event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))));
        assert!(!dashboard.expanded);
        assert!(!dashboard.handle_event(Event::Mouse(event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: DISCLOSURE_ROW,
            modifiers: KeyModifiers::NONE,
        })));
        assert!(dashboard.expanded);
        assert!(dashboard.handle_event(Event::Key(event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        ))));
    }

    #[test]
    fn terminal_lifecycle_smoke_test_when_available() {
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return;
        }
        let mut dashboard = StreamDashboard::enter(
            "/tmp/example".to_owned(),
            Some(StageView {
                current: 4,
                total: 7,
                title: "Apply".to_owned(),
            }),
            "Claude is applying the OpenSpec change".to_owned(),
        )
        .unwrap();
        dashboard.push(&StreamItem::Assistant("Dashboard smoke test".to_owned()));
        std::thread::sleep(Duration::from_millis(40));
        assert!(!dashboard.poll());
        dashboard.leave();
        assert!(!dashboard.active);
    }
}
