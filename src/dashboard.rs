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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhaseStatus {
    Running,
    Complete,
    Failed,
}

#[derive(Debug, Clone)]
struct PhasePanel {
    stage: StageView,
    occurrence: usize,
    status: PhaseStatus,
    lines: VecDeque<DisplayLine>,
    total_lines: usize,
    dropped_lines: usize,
    expanded: bool,
}

#[derive(Debug, Clone)]
struct RenderLine {
    text: String,
    color: Color,
    bold: bool,
    panel: Option<usize>,
}

pub(crate) struct StreamDashboard {
    repo: String,
    activity: String,
    panels: Vec<PhasePanel>,
    selected_panel: usize,
    viewport_start: usize,
    follow_latest: bool,
    ensure_selected: bool,
    heading_rows: Vec<(u16, usize)>,
    retained_lines: usize,
    spinner_index: usize,
    last_tick: Instant,
    last_draw: Instant,
    dirty: bool,
    active: bool,
}

impl StreamDashboard {
    pub(crate) fn enter(repo: String) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut output = io::stderr().lock();
        if let Err(error) = execute!(output, EnterAlternateScreen, EnableMouseCapture, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }

        let now = Instant::now();
        let mut dashboard = Self {
            repo,
            activity: "Preparing workflow".to_owned(),
            panels: Vec::new(),
            selected_panel: 0,
            viewport_start: 0,
            follow_latest: true,
            ensure_selected: false,
            heading_rows: Vec::new(),
            retained_lines: 0,
            spinner_index: 0,
            last_tick: now,
            last_draw: now,
            dirty: true,
            active: true,
        };
        dashboard.draw()?;
        Ok(dashboard)
    }

    pub(crate) fn set_stage(&mut self, stage: StageView) {
        if let Some(previous) = self.panels.last_mut() {
            if previous.status == PhaseStatus::Running {
                previous.status = PhaseStatus::Complete;
            }
            previous.expanded = false;
        }
        let occurrence = self
            .panels
            .iter()
            .filter(|panel| {
                panel.stage.current == stage.current && panel.stage.title == stage.title
            })
            .count()
            + 1;
        self.panels.push(PhasePanel {
            stage,
            occurrence,
            status: PhaseStatus::Running,
            lines: VecDeque::new(),
            total_lines: 0,
            dropped_lines: 0,
            expanded: true,
        });
        self.selected_panel = self.panels.len() - 1;
        self.activity = "Preparing phase".to_owned();
        self.follow_latest = true;
        self.ensure_selected = true;
        self.dirty = true;
        if self.active {
            let _ = self.draw();
        }
    }

    pub(crate) fn start_stream(&mut self, message: &str) {
        self.activity = message.to_owned();
        if let Some(panel) = self.panels.last_mut() {
            panel.status = PhaseStatus::Running;
        }
        self.dirty = true;
    }

    pub(crate) fn finish_stream(&mut self, success: bool, message: &str) {
        self.activity = message.to_owned();
        if !success && let Some(panel) = self.panels.last_mut() {
            panel.status = PhaseStatus::Failed;
        }
        self.dirty = true;
        if self.active {
            let _ = self.draw();
        }
    }

    pub(crate) fn start_activity(&mut self, message: &str) {
        self.activity = message.to_owned();
        self.dirty = true;
    }

    pub(crate) fn finish_activity(&mut self, success: bool, message: &str) {
        let marker = if success { "✓" } else { "✗" };
        let color = if success { Color::Green } else { Color::Red };
        self.push_line(DisplayLine {
            text: format!("  event {marker} {}", sanitize(message)),
            color,
        });
        self.activity = message.to_owned();
        if !success && let Some(panel) = self.panels.last_mut() {
            panel.status = PhaseStatus::Failed;
        }
    }

    pub(crate) fn push_message(&mut self, label: &str, color: Color, text: &str) {
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

    pub(crate) fn push(&mut self, item: &StreamItem) {
        let (label, color, text) = match item {
            StreamItem::Assistant(text) => ("claude", Color::Cyan, text),
            StreamItem::Subagent(text) => ("agent", Color::Blue, text),
            StreamItem::Tool(text) => ("tool", Color::Yellow, text),
            StreamItem::ToolResult(text) => ("result", Color::DarkGrey, text),
            StreamItem::Lifecycle(text) => ("event", Color::Magenta, text),
            StreamItem::Raw(text) => ("json", Color::DarkGrey, text),
        };
        self.push_message(label, color, text);
    }

    fn push_line(&mut self, line: DisplayLine) {
        let Some(panel) = self.panels.last_mut() else {
            return;
        };
        panel.total_lines += 1;
        panel.lines.push_back(line);
        self.retained_lines += 1;
        self.trim_oldest_lines();
        self.dirty = true;
    }

    fn trim_oldest_lines(&mut self) {
        while self.retained_lines > MAX_DISPLAY_LINES {
            let Some(panel) = self.panels.iter_mut().find(|panel| !panel.lines.is_empty()) else {
                break;
            };
            panel.lines.pop_front();
            panel.dropped_lines += 1;
            self.retained_lines -= 1;
        }
    }

    pub(crate) fn poll(&mut self) -> bool {
        let mut cancel = false;
        for _ in 0..32 {
            match event::poll(Duration::ZERO) {
                Ok(true) => match event::read() {
                    Ok(event) => cancel |= self.handle_event(event),
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
                KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('o') => self.toggle_selected(),
                KeyCode::Tab | KeyCode::Right => self.select_next(),
                KeyCode::BackTab | KeyCode::Left => self.select_previous(),
                KeyCode::Esc => self.collapse_selected(),
                KeyCode::Up => self.scroll_up(1),
                KeyCode::Down => self.scroll_down(1),
                KeyCode::PageUp => self.scroll_up(self.body_height().max(1)),
                KeyCode::PageDown => self.scroll_down(self.body_height().max(1)),
                KeyCode::Home => {
                    self.viewport_start = 0;
                    self.follow_latest = false;
                    self.dirty = true;
                }
                KeyCode::End => {
                    self.follow_latest = true;
                    self.dirty = true;
                }
                _ => {}
            },
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, panel)) = self
                    .heading_rows
                    .iter()
                    .find(|(row, _)| *row == mouse.row)
                    .copied()
                {
                    self.selected_panel = panel;
                    self.toggle_selected();
                }
            }
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollUp => self.scroll_up(3),
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => self.scroll_down(3),
            Event::Resize(_, _) => self.dirty = true,
            _ => {}
        }
        false
    }

    fn toggle_selected(&mut self) {
        if let Some(panel) = self.panels.get_mut(self.selected_panel) {
            panel.expanded = !panel.expanded;
            self.ensure_selected = true;
            self.follow_latest = false;
            self.dirty = true;
        }
    }

    fn collapse_selected(&mut self) {
        if let Some(panel) = self.panels.get_mut(self.selected_panel) {
            panel.expanded = false;
            self.ensure_selected = true;
            self.follow_latest = false;
            self.dirty = true;
        }
    }

    fn select_next(&mut self) {
        if !self.panels.is_empty() {
            self.selected_panel = (self.selected_panel + 1) % self.panels.len();
            self.ensure_selected = true;
            self.follow_latest = false;
            self.dirty = true;
        }
    }

    fn select_previous(&mut self) {
        if !self.panels.is_empty() {
            self.selected_panel = self
                .selected_panel
                .checked_sub(1)
                .unwrap_or(self.panels.len() - 1);
            self.ensure_selected = true;
            self.follow_latest = false;
            self.dirty = true;
        }
    }

    fn scroll_up(&mut self, amount: usize) {
        self.viewport_start = self.viewport_start.saturating_sub(amount);
        self.follow_latest = false;
        self.dirty = true;
    }

    fn scroll_down(&mut self, amount: usize) {
        self.viewport_start = self.viewport_start.saturating_add(amount);
        self.follow_latest = false;
        self.dirty = true;
    }

    fn body_height(&self) -> usize {
        terminal::size()
            .map(|(_, height)| height.saturating_sub(4) as usize)
            .unwrap_or_default()
    }

    fn render_lines(&self) -> Vec<RenderLine> {
        let mut lines = Vec::new();
        for (index, panel) in self.panels.iter().enumerate() {
            let selected = if index == self.selected_panel {
                "›"
            } else {
                " "
            };
            let arrow = if panel.expanded { "▼" } else { "▶" };
            let (status, color) = match panel.status {
                PhaseStatus::Running => (SPINNER[self.spinner_index], Color::Yellow),
                PhaseStatus::Complete => ("✓", Color::Green),
                PhaseStatus::Failed => ("✗", Color::Red),
            };
            let occurrence = if panel.occurrence > 1 {
                format!(" · pass {}", panel.occurrence)
            } else {
                String::new()
            };
            let line_label = if panel.total_lines == 1 {
                "line"
            } else {
                "lines"
            };
            let dropped = if panel.dropped_lines > 0 {
                format!(" · {} older hidden", panel.dropped_lines)
            } else {
                String::new()
            };
            lines.push(RenderLine {
                text: format!(
                    "{selected} {arrow} {status} [{}/{}] {}{occurrence} · {} {line_label}{dropped}",
                    panel.stage.current, panel.stage.total, panel.stage.title, panel.total_lines
                ),
                color,
                bold: true,
                panel: Some(index),
            });
            if panel.expanded {
                lines.extend(panel.lines.iter().cloned().map(|line| RenderLine {
                    text: line.text,
                    color: line.color,
                    bold: false,
                    panel: None,
                }));
            }
        }
        lines
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
            let phase = self
                .panels
                .last()
                .map(|panel| {
                    let occurrence = if panel.occurrence > 1 {
                        format!(" · pass {}", panel.occurrence)
                    } else {
                        String::new()
                    };
                    format!(
                        "[{}/{}] {}{occurrence}",
                        panel.stage.current, panel.stage.total, panel.stage.title
                    )
                })
                .unwrap_or_else(|| "[workflow]".to_owned());
            draw_row(
                &mut output,
                1,
                width,
                &format!("{phase}  {} {}", SPINNER[self.spinner_index], self.activity),
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

        let lines = self.render_lines();
        let body_height = height.saturating_sub(4) as usize;
        let maximum_start = lines.len().saturating_sub(body_height);
        if self.follow_latest {
            self.viewport_start = maximum_start;
        } else {
            self.viewport_start = self.viewport_start.min(maximum_start);
        }
        if self.ensure_selected
            && let Some(position) = lines
                .iter()
                .position(|line| line.panel == Some(self.selected_panel))
        {
            if position < self.viewport_start {
                self.viewport_start = position;
            } else if position >= self.viewport_start + body_height.max(1) {
                self.viewport_start = position.saturating_sub(body_height.saturating_sub(1));
            }
            self.ensure_selected = false;
        }

        self.heading_rows.clear();
        for (index, line) in lines
            .iter()
            .skip(self.viewport_start)
            .take(body_height)
            .enumerate()
        {
            let row = 3 + index as u16;
            draw_row(&mut output, row, width, &line.text, line.color, line.bold)?;
            if let Some(panel) = line.panel {
                self.heading_rows.push((row, panel));
            }
        }

        if height > 3 {
            draw_row(
                &mut output,
                height - 1,
                width,
                "click/Enter/Space toggle · Tab/←→ select · ↑↓/Pg scroll · End latest · Ctrl-C stop",
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
        if let Some(panel) = self.panels.last_mut()
            && panel.status == PhaseStatus::Running
        {
            panel.status = PhaseStatus::Complete;
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
        let mut dashboard = StreamDashboard {
            repo: "/repo".to_owned(),
            activity: "working".to_owned(),
            panels: Vec::new(),
            selected_panel: 0,
            viewport_start: 0,
            follow_latest: true,
            ensure_selected: false,
            heading_rows: Vec::new(),
            retained_lines: 0,
            spinner_index: 0,
            last_tick: now,
            last_draw: now,
            dirty: false,
            active: false,
        };
        dashboard.set_stage(StageView {
            current: 1,
            total: 7,
            title: "Explore".to_owned(),
        });
        dashboard
    }

    #[test]
    fn stages_have_separate_collapsible_panels() {
        let mut dashboard = dashboard();
        dashboard.push(&StreamItem::Assistant("exploring".to_owned()));
        dashboard.set_stage(StageView {
            current: 2,
            total: 7,
            title: "Propose".to_owned(),
        });
        dashboard.push(&StreamItem::Assistant("proposing".to_owned()));

        assert_eq!(dashboard.panels.len(), 2);
        assert!(!dashboard.panels[0].expanded);
        assert_eq!(dashboard.panels[0].status, PhaseStatus::Complete);
        assert!(dashboard.panels[1].expanded);
        assert_eq!(dashboard.panels[0].lines[0].text, " claude exploring");
        assert_eq!(dashboard.panels[1].lines[0].text, " claude proposing");
    }

    #[test]
    fn repeated_stage_headings_get_pass_numbers() {
        let mut dashboard = dashboard();
        let verify = StageView {
            current: 5,
            total: 7,
            title: "Verify".to_owned(),
        };
        dashboard.set_stage(verify.clone());
        dashboard.set_stage(verify);
        assert_eq!(dashboard.panels[1].occurrence, 1);
        assert_eq!(dashboard.panels[2].occurrence, 2);
        assert!(dashboard.render_lines()[2].text.contains("pass 2"));
    }

    #[test]
    fn disclosure_controls_toggle_selected_panel_and_ctrl_c_cancels() {
        let mut dashboard = dashboard();
        assert!(!dashboard.handle_event(Event::Key(event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))));
        assert!(!dashboard.panels[0].expanded);
        dashboard.heading_rows = vec![(5, 0)];
        assert!(!dashboard.handle_event(Event::Mouse(event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 5,
            modifiers: KeyModifiers::NONE,
        })));
        assert!(dashboard.panels[0].expanded);
        assert!(dashboard.handle_event(Event::Key(event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        ))));
    }

    #[test]
    fn sanitizes_terminal_control_characters() {
        assert_eq!(sanitize("safe\u{1b}[2J\ttext"), "safe[2J text");
    }

    #[test]
    fn terminal_lifecycle_smoke_test_when_available() {
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return;
        }
        let mut dashboard = StreamDashboard::enter("/tmp/example".to_owned()).unwrap();
        dashboard.set_stage(StageView {
            current: 4,
            total: 7,
            title: "Apply".to_owned(),
        });
        dashboard.start_stream("Claude is applying the OpenSpec change");
        dashboard.push(&StreamItem::Assistant("Dashboard smoke test".to_owned()));
        dashboard.set_stage(StageView {
            current: 5,
            total: 7,
            title: "Verify".to_owned(),
        });
        dashboard.start_stream("Claude is verifying specification compliance");
        dashboard.push(&StreamItem::Assistant(
            "Second phase remains independently visible".to_owned(),
        ));
        std::thread::sleep(Duration::from_millis(40));
        assert!(!dashboard.poll());
        assert_eq!(dashboard.panels.len(), 2);
        dashboard.leave();
        assert!(!dashboard.active);
    }
}
