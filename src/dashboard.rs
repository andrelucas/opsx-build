use std::{
    collections::VecDeque,
    io::{self, Write},
    time::{Duration, Instant},
};

use console::truncate_str;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};

use crate::stream::{StreamControl, StreamItem};

const MAX_DISPLAY_LINES: usize = 20_000;
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageView {
    pub current: usize,
    pub total: usize,
    pub title: String,
    pub started_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CampaignDashboardView {
    pub iteration: u32,
    pub max_iterations: Option<u32>,
    pub completed: Vec<CampaignIterationDashboardView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CampaignIterationDashboardView {
    pub iteration: u32,
    pub change: String,
    pub final_head: Option<String>,
    pub elapsed_seconds: Option<u64>,
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
    finished_at: Option<Instant>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollbarGeometry {
    column: u16,
    top: u16,
    height: u16,
    thumb_top: u16,
    thumb_height: u16,
    max_position: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollbarDrag {
    grab_offset: u16,
}

pub(crate) struct StreamDashboard {
    repo: String,
    change_name: Option<String>,
    campaign: Option<CampaignDashboardView>,
    activity: String,
    panels: Vec<PhasePanel>,
    selected_panel: usize,
    viewport_start: usize,
    follow_latest: bool,
    ensure_selected: bool,
    heading_rows: Vec<(u16, usize)>,
    scrollbar: Option<ScrollbarGeometry>,
    scrollbar_drag: Option<ScrollbarDrag>,
    retained_lines: usize,
    spinner_index: usize,
    last_tick: Instant,
    last_draw: Instant,
    dirty: bool,
    active: bool,
    compact_requested: bool,
    context_requested: bool,
    injection_input: Option<String>,
    stop_after_iteration: bool,
}

impl StreamDashboard {
    pub(crate) fn enter(
        repo: String,
        change_name: Option<String>,
        campaign: Option<CampaignDashboardView>,
    ) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut output = io::stderr().lock();
        if let Err(error) = execute!(
            output,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            Hide
        ) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }

        let now = Instant::now();
        let mut dashboard = Self {
            repo,
            change_name,
            campaign,
            activity: "Preparing workflow".to_owned(),
            panels: Vec::new(),
            selected_panel: 0,
            viewport_start: 0,
            follow_latest: true,
            ensure_selected: false,
            heading_rows: Vec::new(),
            scrollbar: None,
            scrollbar_drag: None,
            retained_lines: 0,
            spinner_index: 0,
            last_tick: now,
            last_draw: now,
            dirty: true,
            active: true,
            compact_requested: false,
            context_requested: false,
            injection_input: None,
            stop_after_iteration: false,
        };
        dashboard.draw()?;
        Ok(dashboard)
    }

    pub(crate) fn set_campaign(&mut self, campaign: Option<CampaignDashboardView>) {
        let changed_iteration = self.campaign.as_ref().map(|campaign| campaign.iteration)
            != campaign.as_ref().map(|campaign| campaign.iteration);
        self.campaign = campaign;
        if changed_iteration {
            self.panels.clear();
            self.selected_panel = 0;
            self.viewport_start = 0;
            self.follow_latest = true;
            self.ensure_selected = false;
            self.heading_rows.clear();
            self.retained_lines = 0;
            self.change_name = None;
            self.stop_after_iteration = false;
        }
        self.dirty = true;
        if self.active {
            let _ = self.draw();
        }
    }

    pub(crate) fn stop_after_iteration_requested(&self) -> bool {
        self.stop_after_iteration
    }

    pub(crate) fn set_change_name(&mut self, change_name: Option<String>) {
        self.change_name = change_name;
        self.dirty = true;
        if self.active {
            let _ = self.draw();
        }
    }

    fn header_text(&self) -> String {
        let campaign = self.campaign.as_ref().map(|campaign| {
            campaign.max_iterations.map_or_else(
                || format!("iteration {}", campaign.iteration),
                |maximum| format!("iteration {}/{}", campaign.iteration, maximum),
            )
        });
        match (&campaign, &self.change_name) {
            (Some(campaign), Some(change)) => {
                format!("ospx-build  {campaign}  change: {change}  {}", self.repo)
            }
            (Some(campaign), None) => format!("ospx-build  {campaign}  {}", self.repo),
            (None, Some(change)) => format!("ospx-build  change: {change}  {}", self.repo),
            (None, None) => format!("ospx-build  {}", self.repo),
        }
    }

    pub(crate) fn set_stage(&mut self, stage: StageView) {
        let now = Instant::now();
        if let Some(previous) = self.panels.last_mut() {
            if previous.status == PhaseStatus::Running {
                previous.status = PhaseStatus::Complete;
            }
            previous.finished_at.get_or_insert(now);
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
            finished_at: None,
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
        self.compact_requested = false;
        self.context_requested = false;
        self.injection_input = None;
        if let Some(panel) = self.panels.last_mut() {
            panel.status = PhaseStatus::Running;
            panel.finished_at = None;
        }
        self.dirty = true;
    }

    pub(crate) fn finish_stream(&mut self, success: bool, message: &str) {
        self.activity = message.to_owned();
        if !success && let Some(panel) = self.panels.last_mut() {
            panel.status = PhaseStatus::Failed;
            panel.finished_at = Some(Instant::now());
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
            panel.finished_at = Some(Instant::now());
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

    pub(crate) fn poll(&mut self) -> StreamControl {
        let mut control = StreamControl::None;
        for _ in 0..32 {
            match event::poll(Duration::ZERO) {
                Ok(true) => match event::read() {
                    Ok(event) => {
                        control = self.handle_event(event);
                        if control != StreamControl::None {
                            break;
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
        control
    }

    fn handle_event(&mut self, event: Event) -> StreamControl {
        if let Event::Key(key) = &event
            && key.kind == KeyEventKind::Press
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return StreamControl::Interrupt;
        }

        if self.injection_input.is_some() {
            return self.handle_injection_event(event);
        }

        if let Event::Mouse(mouse) = &event
            && self.handle_scrollbar_mouse(*mouse)
        {
            return StreamControl::None;
        }

        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') if self.campaign.is_some() => {
                    self.stop_after_iteration = true;
                    self.push_message(
                        "campaign",
                        Color::Magenta,
                        "Will pause after the current OpenSpec change completes",
                    );
                }
                KeyCode::Char('c') if !self.compact_requested => {
                    self.compact_requested = true;
                    self.push_message(
                        "compact",
                        Color::Magenta,
                        "Requested mid-command compaction; interrupting Claude first",
                    );
                    return StreamControl::Compact;
                }
                KeyCode::Char('C') if !self.context_requested => {
                    self.context_requested = true;
                    self.push_message(
                        "context",
                        Color::Magenta,
                        "Requested context inspection; interrupting Claude first",
                    );
                    return StreamControl::Context;
                }
                KeyCode::Char('i') => {
                    self.injection_input = Some(String::new());
                    self.dirty = true;
                }
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
        StreamControl::None
    }

    fn handle_injection_event(&mut self, event: Event) -> StreamControl {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Enter => {
                    let message = self.injection_input.take().unwrap_or_default();
                    self.dirty = true;
                    if message.trim().is_empty() {
                        return StreamControl::None;
                    }
                    self.push_message("steer", Color::Magenta, &format!("Requested: {message}"));
                    return StreamControl::Inject(message);
                }
                KeyCode::Esc => {
                    self.injection_input = None;
                    self.dirty = true;
                }
                KeyCode::Backspace => {
                    if let Some(input) = self.injection_input.as_mut() {
                        input.pop();
                    }
                    self.dirty = true;
                }
                KeyCode::Char(character)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    if let Some(input) = self.injection_input.as_mut() {
                        input.push(character);
                    }
                    self.dirty = true;
                }
                _ => {}
            },
            Event::Paste(text) => {
                if let Some(input) = self.injection_input.as_mut() {
                    input.push_str(&text);
                }
                self.dirty = true;
            }
            Event::Resize(_, _) => self.dirty = true,
            _ => {}
        }
        StreamControl::None
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
        self.viewport_start = self
            .viewport_start
            .saturating_add(amount)
            .min(self.scrollbar.map_or(usize::MAX, |bar| bar.max_position));
        self.follow_latest = self
            .scrollbar
            .is_some_and(|bar| self.viewport_start == bar.max_position);
        self.dirty = true;
    }

    fn set_scrollbar_position(&mut self, position: usize) {
        let Some(scrollbar) = self.scrollbar else {
            return;
        };
        self.viewport_start = position.min(scrollbar.max_position);
        self.follow_latest = self.viewport_start == scrollbar.max_position;
        self.dirty = true;
    }

    fn handle_scrollbar_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> bool {
        let Some(scrollbar) = self.scrollbar else {
            self.scrollbar_drag = None;
            return false;
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if mouse.column == scrollbar.column
                    && mouse.row >= scrollbar.top
                    && mouse.row < scrollbar.top.saturating_add(scrollbar.height) =>
            {
                if mouse.row == scrollbar.top {
                    self.set_scrollbar_position(self.viewport_start.saturating_sub(1));
                } else if mouse.row == scrollbar.top.saturating_add(scrollbar.height - 1) {
                    self.set_scrollbar_position(self.viewport_start.saturating_add(1));
                } else if mouse.row >= scrollbar.thumb_top
                    && mouse.row < scrollbar.thumb_top.saturating_add(scrollbar.thumb_height)
                {
                    self.scrollbar_drag = Some(ScrollbarDrag {
                        grab_offset: mouse.row.saturating_sub(scrollbar.thumb_top),
                    });
                } else {
                    let grab_offset = scrollbar.thumb_height / 2;
                    self.scrollbar_drag = Some(ScrollbarDrag { grab_offset });
                    self.set_scrollbar_position(scrollbar_position_for_row(
                        scrollbar,
                        mouse.row,
                        grab_offset,
                    ));
                }
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(drag) = self.scrollbar_drag else {
                    return false;
                };
                self.set_scrollbar_position(scrollbar_position_for_row(
                    scrollbar,
                    mouse.row,
                    drag.grab_offset,
                ));
                true
            }
            MouseEventKind::Up(MouseButton::Left) if self.scrollbar_drag.is_some() => {
                self.scrollbar_drag = None;
                true
            }
            _ => false,
        }
    }

    fn body_height(&self) -> usize {
        terminal::size()
            .map(|(_, height)| height.saturating_sub(4) as usize)
            .unwrap_or_default()
    }

    fn render_lines_at(&self, now: Instant) -> Vec<RenderLine> {
        let mut lines = Vec::new();
        if let Some(campaign) = &self.campaign {
            lines.extend(campaign.completed.iter().map(|iteration| {
                let elapsed = iteration
                    .elapsed_seconds
                    .map(Duration::from_secs)
                    .map(format_duration)
                    .map_or_else(String::new, |elapsed| format!(" · {elapsed}"));
                let head = iteration
                    .final_head
                    .as_deref()
                    .map(short_hash)
                    .map_or_else(String::new, |head| format!(" · {head}"));
                RenderLine {
                    text: format!(
                        "  ✓ iteration {} · {}{elapsed}{head}",
                        iteration.iteration, iteration.change
                    ),
                    color: Color::Green,
                    bold: true,
                    panel: None,
                }
            }));
        }
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
            let elapsed = format_duration(panel.elapsed_at(now));
            lines.push(RenderLine {
                text: format!(
                    "{selected} {arrow} {status} [{}/{}] {}{occurrence} · {elapsed} · {} {line_label}{dropped}",
                    panel.stage.current, panel.stage.total, panel.stage.title, panel.total_lines,
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
        let now = Instant::now();
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
            &self.header_text(),
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
                        "[{}/{}] {}{occurrence} · {}",
                        panel.stage.current,
                        panel.stage.total,
                        panel.stage.title,
                        format_duration(panel.elapsed_at(now))
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

        let lines = self.render_lines_at(now);
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

        self.scrollbar = (maximum_start > 0 && body_height >= 3 && width > 0).then(|| {
            scrollbar_geometry(
                width,
                3,
                height.saturating_sub(4),
                maximum_start,
                self.viewport_start,
                body_height,
            )
        });
        let body_width = width.saturating_sub(u16::from(self.scrollbar.is_some()));

        self.heading_rows.clear();
        for (index, line) in lines
            .iter()
            .skip(self.viewport_start)
            .take(body_height)
            .enumerate()
        {
            let row = 3 + index as u16;
            draw_row(
                &mut output,
                row,
                body_width,
                &line.text,
                line.color,
                line.bold,
            )?;
            if let Some(panel) = line.panel {
                self.heading_rows.push((row, panel));
            }
        }
        if let Some(scrollbar) = self.scrollbar {
            draw_scrollbar(&mut output, scrollbar)?;
        }

        if height > 3 {
            let footer = self.injection_input.as_ref().map_or_else(
                || {
                    let campaign = if self.campaign.is_some() {
                        " · q pause after slice"
                    } else {
                        ""
                    };
                    format!("c compact · C context · i steer{campaign} · click/Enter/Space toggle · Tab/←→ select · ↑↓/Pg scroll · Ctrl-C stop")
                },
                |input| format!("steer> {input}█   Enter interrupt · Esc cancel · Ctrl-C stop"),
            );
            draw_row(
                &mut output,
                height - 1,
                width,
                &footer,
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
            panel.finished_at = Some(Instant::now());
        }
        let mut output = io::stderr().lock();
        let _ = execute!(
            output,
            Show,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
        self.active = false;
    }
}

impl PhasePanel {
    fn elapsed_at(&self, now: Instant) -> Duration {
        self.finished_at
            .unwrap_or(now)
            .saturating_duration_since(self.stage.started_at)
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

fn scrollbar_geometry(
    width: u16,
    top: u16,
    height: u16,
    max_position: usize,
    position: usize,
    viewport: usize,
) -> ScrollbarGeometry {
    let track_length = usize::from(height.saturating_sub(2));
    let denominator = max_position.saturating_add(viewport.max(1));
    let thumb_length = rounded_divide(viewport.saturating_mul(track_length), denominator)
        .clamp(1, track_length.max(1));
    let draggable = track_length.saturating_sub(thumb_length);
    let thumb_offset = rounded_divide(
        position.min(max_position).saturating_mul(draggable),
        max_position.max(1),
    )
    .min(draggable);
    ScrollbarGeometry {
        column: width.saturating_sub(1),
        top,
        height,
        thumb_top: top
            .saturating_add(1)
            .saturating_add(thumb_offset.min(usize::from(u16::MAX)) as u16),
        thumb_height: thumb_length.min(usize::from(u16::MAX)) as u16,
        max_position,
    }
}

fn scrollbar_position_for_row(scrollbar: ScrollbarGeometry, row: u16, grab_offset: u16) -> usize {
    let track_start = scrollbar.top.saturating_add(1);
    let track_length = scrollbar.height.saturating_sub(2);
    let draggable = track_length.saturating_sub(scrollbar.thumb_height);
    if draggable == 0 {
        return 0;
    }
    let thumb_offset = row
        .saturating_sub(track_start)
        .saturating_sub(grab_offset)
        .min(draggable);
    rounded_divide(
        usize::from(thumb_offset).saturating_mul(scrollbar.max_position),
        usize::from(draggable),
    )
}

fn rounded_divide(numerator: usize, denominator: usize) -> usize {
    numerator
        .saturating_add(denominator / 2)
        .checked_div(denominator)
        .unwrap_or(0)
}

fn draw_scrollbar<W: Write>(output: &mut W, scrollbar: ScrollbarGeometry) -> io::Result<()> {
    let bottom = scrollbar.top.saturating_add(scrollbar.height - 1);
    queue!(
        output,
        SetForegroundColor(Color::DarkGrey),
        MoveTo(scrollbar.column, scrollbar.top),
        Print("↑")
    )?;
    for row in scrollbar.top.saturating_add(1)..bottom {
        let symbol = if row >= scrollbar.thumb_top
            && row < scrollbar.thumb_top.saturating_add(scrollbar.thumb_height)
        {
            "█"
        } else {
            "│"
        };
        let color = if symbol == "█" {
            Color::Cyan
        } else {
            Color::DarkGrey
        };
        queue!(
            output,
            SetForegroundColor(color),
            MoveTo(scrollbar.column, row),
            Print(symbol)
        )?;
    }
    queue!(
        output,
        SetForegroundColor(Color::DarkGrey),
        MoveTo(scrollbar.column, bottom),
        Print("↓"),
        ResetColor
    )?;
    Ok(())
}

fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
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

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let minutes = seconds / 60;
    let hours = minutes / 60;
    match (hours, minutes % 60) {
        (0, 0) => format!("{seconds}s"),
        (0, minutes) => format!("{minutes}m {:02}s", seconds % 60),
        (hours, minutes) => format!("{hours}h {minutes:02}m {:02}s", seconds % 60),
    }
}

#[cfg(test)]
mod tests {
    use std::io::IsTerminal;

    use super::*;

    fn dashboard() -> StreamDashboard {
        let now = Instant::now();
        let mut dashboard = StreamDashboard {
            repo: "/repo".to_owned(),
            change_name: None,
            campaign: None,
            activity: "working".to_owned(),
            panels: Vec::new(),
            selected_panel: 0,
            viewport_start: 0,
            follow_latest: true,
            ensure_selected: false,
            heading_rows: Vec::new(),
            scrollbar: None,
            scrollbar_drag: None,
            retained_lines: 0,
            spinner_index: 0,
            last_tick: now,
            last_draw: now,
            dirty: false,
            active: false,
            compact_requested: false,
            context_requested: false,
            injection_input: None,
            stop_after_iteration: false,
        };
        dashboard.set_stage(StageView {
            current: 1,
            total: 7,
            title: "Explore".to_owned(),
            started_at: Instant::now(),
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
            started_at: Instant::now(),
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
    fn header_shows_change_name_when_known() {
        let mut dashboard = dashboard();
        assert_eq!(dashboard.header_text(), "ospx-build  /repo");

        dashboard.set_change_name(Some("slice-m-test-infrastructure".to_owned()));
        assert_eq!(
            dashboard.header_text(),
            "ospx-build  change: slice-m-test-infrastructure  /repo"
        );
    }

    #[test]
    fn campaign_header_and_summary_survive_iteration_reset() {
        let mut dashboard = dashboard();
        dashboard.set_campaign(Some(CampaignDashboardView {
            iteration: 2,
            max_iterations: Some(10),
            completed: vec![CampaignIterationDashboardView {
                iteration: 1,
                change: "slice-a".to_owned(),
                final_head: Some("1234567890abcdef".to_owned()),
                elapsed_seconds: Some(65),
            }],
        }));

        assert_eq!(dashboard.header_text(), "ospx-build  iteration 2/10  /repo");
        assert!(dashboard.panels.is_empty());
        let lines = dashboard.render_lines_at(Instant::now());
        assert!(lines[0].text.contains("iteration 1 · slice-a"));
        assert!(lines[0].text.contains("1m 05s"));
        assert!(lines[0].text.contains("1234567890ab"));
    }

    #[test]
    fn campaign_q_requests_a_stop_after_the_current_iteration() {
        let mut dashboard = dashboard();
        dashboard.campaign = Some(CampaignDashboardView {
            iteration: 1,
            max_iterations: None,
            completed: Vec::new(),
        });
        assert_eq!(
            dashboard.handle_event(Event::Key(event::KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
            ))),
            StreamControl::None
        );
        assert!(dashboard.stop_after_iteration_requested());
        assert!(
            dashboard.panels[0]
                .lines
                .back()
                .unwrap()
                .text
                .contains("pause after")
        );
    }

    #[test]
    fn scrollbar_buttons_track_and_drag_control_the_viewport() {
        let mut dashboard = dashboard();
        let scrollbar = scrollbar_geometry(80, 3, 12, 90, 0, 10);
        dashboard.scrollbar = Some(scrollbar);

        let mouse = |kind, row| {
            Event::Mouse(event::MouseEvent {
                kind,
                column: scrollbar.column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        dashboard.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            scrollbar.top + scrollbar.height - 1,
        ));
        assert_eq!(dashboard.viewport_start, 1);

        dashboard.handle_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            scrollbar.top + scrollbar.height - 2,
        ));
        dashboard.handle_event(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            scrollbar.top + scrollbar.height - 2,
        ));
        dashboard.handle_event(mouse(
            MouseEventKind::Up(MouseButton::Left),
            scrollbar.top + scrollbar.height - 2,
        ));
        assert_eq!(dashboard.viewport_start, scrollbar.max_position);
        assert!(dashboard.follow_latest);
        assert_eq!(dashboard.scrollbar_drag, None);
    }

    #[test]
    fn repeated_stage_headings_get_pass_numbers() {
        let mut dashboard = dashboard();
        let verify = StageView {
            current: 5,
            total: 7,
            title: "Verify".to_owned(),
            started_at: Instant::now(),
        };
        dashboard.set_stage(verify.clone());
        dashboard.set_stage(verify);
        assert_eq!(dashboard.panels[1].occurrence, 1);
        assert_eq!(dashboard.panels[2].occurrence, 2);
        assert!(
            dashboard.render_lines_at(Instant::now())[2]
                .text
                .contains("pass 2")
        );
    }

    #[test]
    fn stage_headings_show_live_and_frozen_elapsed_time() {
        let mut dashboard = dashboard();
        let now = Instant::now();
        dashboard.panels[0].stage.started_at = now - Duration::from_secs(65);

        assert!(dashboard.render_lines_at(now)[0].text.contains("1m 05s"));

        dashboard.panels[0].finished_at = Some(now);
        assert!(
            dashboard.render_lines_at(now + Duration::from_secs(60))[0]
                .text
                .contains("1m 05s")
        );
    }

    #[test]
    fn formats_stage_durations_compactly() {
        assert_eq!(format_duration(Duration::from_secs(9)), "9s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 05s");
        assert_eq!(format_duration(Duration::from_secs(7_445)), "2h 04m 05s");
    }

    #[test]
    fn disclosure_controls_toggle_selected_panel_and_ctrl_c_cancels() {
        let mut dashboard = dashboard();
        assert_eq!(
            dashboard.handle_event(Event::Key(event::KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            ))),
            StreamControl::None
        );
        assert!(!dashboard.panels[0].expanded);
        dashboard.heading_rows = vec![(5, 0)];
        assert_eq!(
            dashboard.handle_event(Event::Mouse(event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 5,
                modifiers: KeyModifiers::NONE,
            })),
            StreamControl::None
        );
        assert!(dashboard.panels[0].expanded);
        assert_eq!(
            dashboard.handle_event(Event::Key(event::KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            ))),
            StreamControl::Interrupt
        );
    }

    #[test]
    fn plain_c_queues_one_compaction_per_stream() {
        let mut dashboard = dashboard();
        let compact = Event::Key(event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(
            dashboard.handle_event(compact.clone()),
            StreamControl::Compact
        );
        assert_eq!(dashboard.handle_event(compact), StreamControl::None);
        assert!(
            dashboard.panels[0]
                .lines
                .back()
                .unwrap()
                .text
                .contains("Requested mid-command compaction")
        );

        dashboard.start_stream("next Claude invocation");
        assert_eq!(
            dashboard.handle_event(Event::Key(event::KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::NONE,
            ))),
            StreamControl::Compact
        );
    }

    #[test]
    fn capital_c_queues_one_context_inspection_per_stream() {
        let mut dashboard = dashboard();
        let context = Event::Key(event::KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::SHIFT,
        ));
        assert_eq!(
            dashboard.handle_event(context.clone()),
            StreamControl::Context
        );
        assert_eq!(dashboard.handle_event(context), StreamControl::None);
        assert!(
            dashboard.panels[0]
                .lines
                .back()
                .unwrap()
                .text
                .contains("Requested context inspection")
        );

        dashboard.start_stream("next Claude invocation");
        assert_eq!(
            dashboard.handle_event(Event::Key(event::KeyEvent::new(
                KeyCode::Char('C'),
                KeyModifiers::SHIFT,
            ))),
            StreamControl::Context
        );
    }

    #[test]
    fn i_opens_an_injection_prompt_and_enter_queues_the_message() {
        let mut dashboard = dashboard();
        assert_eq!(
            dashboard.handle_event(Event::Key(event::KeyEvent::new(
                KeyCode::Char('i'),
                KeyModifiers::NONE,
            ))),
            StreamControl::None
        );
        assert_eq!(dashboard.injection_input.as_deref(), Some(""));
        for character in "/compact".chars() {
            assert_eq!(
                dashboard.handle_event(Event::Key(event::KeyEvent::new(
                    KeyCode::Char(character),
                    KeyModifiers::NONE,
                ))),
                StreamControl::None
            );
        }
        assert_eq!(
            dashboard.handle_event(Event::Key(event::KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            ))),
            StreamControl::Inject("/compact".to_owned())
        );
        assert!(dashboard.injection_input.is_none());
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
        let mut dashboard = StreamDashboard::enter(
            "/tmp/example".to_owned(),
            Some("example-change".to_owned()),
            None,
        )
        .unwrap();
        dashboard.set_stage(StageView {
            current: 4,
            total: 7,
            title: "Apply".to_owned(),
            started_at: Instant::now(),
        });
        dashboard.start_stream("Claude is applying the OpenSpec change");
        for index in 0..200 {
            dashboard.push(&StreamItem::Assistant(format!(
                "Dashboard scrollbar smoke test line {index}"
            )));
        }
        dashboard.draw().unwrap();
        assert!(dashboard.scrollbar.is_some());
        dashboard.set_stage(StageView {
            current: 5,
            total: 7,
            title: "Verify".to_owned(),
            started_at: Instant::now(),
        });
        dashboard.start_stream("Claude is verifying specification compliance");
        dashboard.push(&StreamItem::Assistant(
            "Second phase remains independently visible".to_owned(),
        ));
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(dashboard.poll(), StreamControl::None);
        assert_eq!(dashboard.panels.len(), 2);
        dashboard.leave();
        assert!(!dashboard.active);
    }
}
