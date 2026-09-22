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
const MAX_CLAUDE_MESSAGE_LINES: usize = 200;
const CLAUDE_MESSAGE_HEAD_LINES: usize = 120;
const CLAUDE_MESSAGE_TAIL_LINES: usize = MAX_CLAUDE_MESSAGE_LINES - CLAUDE_MESSAGE_HEAD_LINES;
const SOURCE_LABEL_WIDTH: usize = 8;
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
    source: Option<SourceLabel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceLabel {
    text: String,
    color: Color,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhaseStatus {
    Running,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingConfirmation {
    Pause,
    StopAfterIteration,
    Escalate,
    Compact,
    Context,
}

impl PendingConfirmation {
    fn prompt(self) -> &'static str {
        match self {
            Self::Pause => "Pause now and stop the current agent process?",
            Self::StopAfterIteration => "Pause after the current OpenSpec change completes?",
            Self::Escalate => "Stop the local worker and request frontier replanning?",
            Self::Compact => "Interrupt the active agent, compact, and restart the current phase?",
            Self::Context => "Interrupt the active agent and request a context report?",
        }
    }
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
    source: Option<SourceLabel>,
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
    last_size: Option<(u16, u16)>,
    last_footer: Option<String>,
    dirty: bool,
    active: bool,
    compact_requested: bool,
    context_requested: bool,
    injection_input: Option<String>,
    pending_confirmation: Option<PendingConfirmation>,
    stop_after_iteration: bool,
    frontier_enabled: bool,
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
            last_size: None,
            last_footer: None,
            dirty: true,
            active: true,
            compact_requested: false,
            context_requested: false,
            injection_input: None,
            pending_confirmation: None,
            stop_after_iteration: false,
            frontier_enabled: true,
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

    pub(crate) fn set_frontier_enabled(&mut self, enabled: bool) {
        self.frontier_enabled = enabled;
        self.dirty = true;
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
                format!("opsx-build  {campaign}  change: {change}  {}", self.repo)
            }
            (Some(campaign), None) => format!("opsx-build  {campaign}  {}", self.repo),
            (None, Some(change)) => format!("opsx-build  change: {change}  {}", self.repo),
            (None, None) => format!("opsx-build  {}", self.repo),
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
        self.pending_confirmation = None;
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
            source: None,
        });
        self.activity = message.to_owned();
        if !success && let Some(panel) = self.panels.last_mut() {
            panel.status = PhaseStatus::Failed;
            panel.finished_at = Some(Instant::now());
        }
    }

    pub(crate) fn push_message(&mut self, label: &str, color: Color, text: &str) {
        for (index, line) in text.lines().enumerate() {
            self.push_line(DisplayLine {
                text: sanitize(line),
                color: Color::White,
                source: Some(SourceLabel {
                    text: if index == 0 {
                        label.to_owned()
                    } else {
                        String::new()
                    },
                    color,
                }),
            });
        }
    }

    pub(crate) fn push(&mut self, item: &StreamItem) {
        if let StreamItem::Assistant(text) = item {
            self.push_agent_message("claude", "Claude", text);
            return;
        }
        if let StreamItem::OpenCode(text) = item {
            self.push_agent_message("opencode", "OpenCode", text);
            return;
        }
        if let StreamItem::Codex(text) = item {
            self.push_agent_message("codex", "Codex", text);
            return;
        }
        let (label, color, text) = match item {
            StreamItem::Assistant(_) | StreamItem::OpenCode(_) | StreamItem::Codex(_) => {
                unreachable!("assistant messages returned above")
            }
            StreamItem::Subagent(text) => ("agent", Color::Blue, text),
            StreamItem::Tool(text) => ("tool", Color::Yellow, text),
            StreamItem::ToolResult(text) => ("result", Color::DarkGrey, text),
            StreamItem::Lifecycle(text) => ("event", Color::Magenta, text),
            StreamItem::Raw(text) => ("json", Color::DarkGrey, text),
        };
        self.push_message(label, color, text);
    }

    fn push_agent_message(&mut self, label: &str, display_name: &str, text: &str) {
        let lines = text.lines().collect::<Vec<_>>();
        if lines.len() <= MAX_CLAUDE_MESSAGE_LINES {
            self.push_message(label, Color::Cyan, text);
            return;
        }

        let omitted = lines.len() - MAX_CLAUDE_MESSAGE_LINES;
        let mut displayed = lines[..CLAUDE_MESSAGE_HEAD_LINES].to_vec();
        let marker = format!("… {omitted} lines omitted from this {display_name} message …");
        displayed.push(&marker);
        displayed.extend_from_slice(&lines[lines.len() - CLAUDE_MESSAGE_TAIL_LINES..]);
        self.push_message(label, Color::Cyan, &displayed.join("\n"));
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

        if self.pending_confirmation.is_some() {
            return self.handle_confirmation_event(event);
        }

        if let Event::Mouse(mouse) = &event
            && self.handle_scrollbar_mouse(*mouse)
        {
            return StreamControl::None;
        }

        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') if self.campaign.is_some() => {
                    self.request_confirmation(PendingConfirmation::StopAfterIteration);
                }
                KeyCode::Char('p') => {
                    self.request_confirmation(PendingConfirmation::Pause);
                }
                KeyCode::Char('f') => {
                    if !self.frontier_enabled {
                        self.push_message(
                            "frontier",
                            Color::Yellow,
                            "Frontier replanning is disabled for this local-only workflow",
                        );
                        return StreamControl::None;
                    }
                    if !self.frontier_available() {
                        self.push_message(
                            "frontier",
                            Color::Yellow,
                            "Frontier replanning is available only during Explore, Propose, Apply, Verify, or Repair",
                        );
                        return StreamControl::None;
                    }
                    self.request_confirmation(PendingConfirmation::Escalate);
                }
                KeyCode::Char('c') if !self.compact_requested => {
                    self.request_confirmation(PendingConfirmation::Compact);
                }
                KeyCode::Char('C') if !self.context_requested => {
                    self.request_confirmation(PendingConfirmation::Context);
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

    fn request_confirmation(&mut self, action: PendingConfirmation) {
        self.pending_confirmation = Some(action);
        self.dirty = true;
    }

    fn handle_confirmation_event(&mut self, event: Event) -> StreamControl {
        let key = match event {
            Event::Key(key) => key,
            Event::Resize(_, _) => {
                self.dirty = true;
                return StreamControl::None;
            }
            _ => return StreamControl::None,
        };
        if key.kind != KeyEventKind::Press {
            return StreamControl::None;
        }

        let Some(action) = self.pending_confirmation.take() else {
            return StreamControl::None;
        };
        self.dirty = true;
        if !matches!(key.code, KeyCode::Char('y' | 'Y')) {
            return StreamControl::None;
        }

        match action {
            PendingConfirmation::Pause => {
                self.push_message(
                    "pause",
                    Color::Magenta,
                    "Pausing now; the current phase checkpoint will be preserved",
                );
                StreamControl::Pause
            }
            PendingConfirmation::StopAfterIteration => {
                self.stop_after_iteration = true;
                self.push_message(
                    "campaign",
                    Color::Magenta,
                    "Will pause after the current OpenSpec change completes",
                );
                StreamControl::None
            }
            PendingConfirmation::Escalate => {
                self.push_message(
                    "frontier",
                    Color::Magenta,
                    "Stopping the local worker and requesting frontier replanning",
                );
                StreamControl::Escalate
            }
            PendingConfirmation::Compact => {
                self.compact_requested = true;
                self.push_message(
                    "compact",
                    Color::Magenta,
                    "Requested mid-command compaction; interrupting the active agent first",
                );
                StreamControl::Compact
            }
            PendingConfirmation::Context => {
                self.context_requested = true;
                self.push_message(
                    "context",
                    Color::Magenta,
                    "Requested context inspection; interrupting the active agent first",
                );
                StreamControl::Context
            }
        }
    }

    fn frontier_available(&self) -> bool {
        self.frontier_enabled
            && self
                .panels
                .iter()
                .rev()
                .find(|panel| panel.status == PhaseStatus::Running)
                .is_some_and(|panel| {
                    matches!(
                        panel.stage.title.as_str(),
                        "Explore" | "Propose" | "Apply" | "Verify" | "Repair"
                    )
                })
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
                    source: None,
                }
            }));
            let change = self
                .change_name
                .as_deref()
                .map_or_else(String::new, |change| format!(" · {change}"));
            lines.push(RenderLine {
                text: format!("  ◆ iteration {} · current{change}", campaign.iteration),
                color: Color::Cyan,
                bold: true,
                panel: None,
                source: None,
            });
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
                source: None,
            });
            if panel.expanded {
                lines.extend(panel.lines.iter().cloned().map(|line| RenderLine {
                    text: line.text,
                    color: line.color,
                    bold: false,
                    panel: None,
                    source: line.source,
                }));
            }
        }
        lines
    }

    fn draw(&mut self) -> io::Result<()> {
        let (width, height) = terminal::size()?;
        self.draw_to(&mut io::stderr().lock(), width, height)
    }

    fn draw_to<W: Write>(&mut self, terminal: &mut W, width: u16, height: u16) -> io::Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        let now = Instant::now();
        let resized = self.last_size != Some((width, height));
        // Buffer the frame instead of writing each terminal command to stderr.
        let mut output = Vec::new();
        if resized {
            queue!(output, MoveTo(0, 0), Clear(ClearType::All))?;
        }
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
        let visible_rows = lines
            .len()
            .saturating_sub(self.viewport_start)
            .min(body_height);
        for (index, line) in lines
            .iter()
            .skip(self.viewport_start)
            .take(body_height)
            .enumerate()
        {
            let row = 3 + index as u16;
            draw_render_row(&mut output, row, body_width, line)?;
            if let Some(panel) = line.panel {
                self.heading_rows.push((row, panel));
            }
        }
        // Collapsing a panel or starting an iteration can leave fewer body rows.
        for index in visible_rows..body_height {
            queue!(
                output,
                MoveTo(0, 3 + index as u16),
                Clear(ClearType::CurrentLine)
            )?;
        }
        if let Some(scrollbar) = self.scrollbar {
            draw_scrollbar(&mut output, scrollbar)?;
        }

        let footer = if height > 3 {
            let footer = if let Some(action) = self.pending_confirmation {
                format!("{}  y confirm · any other key cancel", action.prompt())
            } else {
                self.injection_input.as_ref().map_or_else(
                    || {
                    let campaign = if self.campaign.is_some() {
                        " · q pause after slice"
                    } else {
                        ""
                    };
                    let frontier = if self.frontier_enabled { " · f frontier" } else { "" };
                    format!("p pause{frontier} · c compact · C context · i steer{campaign} · click/Enter/Space toggle · Tab/←→ select · ↑↓/Pg scroll · Ctrl-C stop")
                },
                    |input| {
                        format!("steer> {input}█   Enter interrupt · Esc cancel · Ctrl-C stop")
                    },
                )
            };
            if resized || self.last_footer.as_ref() != Some(&footer) {
                draw_row(
                    &mut output,
                    height - 1,
                    width,
                    &footer,
                    Color::DarkGrey,
                    false,
                )?;
            }
            Some(footer)
        } else {
            None
        };
        terminal.write_all(&output)?;
        terminal.flush()?;
        self.last_size = Some((width, height));
        self.last_footer = footer;
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

fn draw_render_row<W: Write>(
    output: &mut W,
    row: u16,
    width: u16,
    line: &RenderLine,
) -> io::Result<()> {
    let Some(source) = &line.source else {
        return draw_row(output, row, width, &line.text, line.color, line.bold);
    };

    let label = truncate_str(&source.text, SOURCE_LABEL_WIDTH, "…");
    let prefix = format!("{label:>SOURCE_LABEL_WIDTH$} ");
    let prefix = truncate_str(&prefix, width as usize, "…");
    let message_width = usize::from(width).saturating_sub(SOURCE_LABEL_WIDTH + 1);
    let message = truncate_str(&line.text, message_width, "…");
    queue!(
        output,
        MoveTo(0, row),
        Clear(ClearType::CurrentLine),
        SetForegroundColor(source.color),
        SetAttribute(Attribute::Bold),
        Print(prefix),
        SetAttribute(Attribute::Reset),
        SetForegroundColor(line.color),
        Print(message),
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
            last_size: None,
            last_footer: None,
            dirty: false,
            active: false,
            compact_requested: false,
            context_requested: false,
            injection_input: None,
            pending_confirmation: None,
            stop_after_iteration: false,
            frontier_enabled: true,
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
        assert_eq!(dashboard.panels[0].lines[0].text, "exploring");
        assert_eq!(dashboard.panels[1].lines[0].text, "proposing");
        assert_eq!(
            dashboard.panels[0].lines[0].source,
            Some(SourceLabel {
                text: "claude".to_owned(),
                color: Color::Cyan,
            })
        );
    }

    #[test]
    fn long_claude_messages_keep_their_beginning_and_conclusion() {
        let mut dashboard = dashboard();
        let message = (0..250)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");

        dashboard.push(&StreamItem::Assistant(message));

        let lines = &dashboard.panels[0].lines;
        assert_eq!(lines.len(), MAX_CLAUDE_MESSAGE_LINES + 1);
        assert_eq!(lines[0].text, "line 0");
        assert_eq!(lines[CLAUDE_MESSAGE_HEAD_LINES - 1].text, "line 119");
        assert_eq!(
            lines[CLAUDE_MESSAGE_HEAD_LINES].text,
            "… 50 lines omitted from this Claude message …"
        );
        assert_eq!(lines[CLAUDE_MESSAGE_HEAD_LINES + 1].text, "line 170");
        assert_eq!(lines.back().unwrap().text, "line 249");
    }

    #[test]
    fn opencode_messages_have_their_own_source_label() {
        let mut dashboard = dashboard();

        dashboard.push(&StreamItem::OpenCode("working".to_owned()));

        assert_eq!(dashboard.panels[0].lines[0].text, "working");
        assert_eq!(
            dashboard.panels[0].lines[0].source,
            Some(SourceLabel {
                text: "opencode".to_owned(),
                color: Color::Cyan,
            })
        );
    }

    #[test]
    fn header_shows_change_name_when_known() {
        let mut dashboard = dashboard();
        assert_eq!(dashboard.header_text(), "opsx-build  /repo");

        dashboard.set_change_name(Some("slice-m-test-infrastructure".to_owned()));
        assert_eq!(
            dashboard.header_text(),
            "opsx-build  change: slice-m-test-infrastructure  /repo"
        );
    }

    #[test]
    fn first_campaign_iteration_clears_bootstrap_panels() {
        let mut dashboard = dashboard();
        dashboard.set_change_name(Some("bootstrap-implementation-slices".to_owned()));
        for (index, title) in [
            "Propose",
            "Proposal commit",
            "Apply",
            "Verify",
            "Archive",
            "Completion commit",
        ]
        .iter()
        .enumerate()
        {
            dashboard.set_stage(StageView {
                current: index + 1,
                total: 6,
                title: format!("{title} · frontier (codex-frontier, codex)"),
                started_at: Instant::now(),
            });
        }
        assert!(
            dashboard
                .header_text()
                .contains("bootstrap-implementation-slices")
        );

        dashboard.set_campaign(Some(CampaignDashboardView {
            iteration: 1,
            max_iterations: None,
            completed: Vec::new(),
        }));
        dashboard.set_change_name(Some("0001-reproducible-go-bindings".to_owned()));
        dashboard.set_stage(StageView {
            current: 1,
            total: 6,
            title: "Propose · worker (codex-worker, codex)".to_owned(),
            started_at: Instant::now(),
        });

        assert_eq!(dashboard.panels.len(), 1);
        let lines = dashboard.render_lines_at(Instant::now());
        assert_eq!(
            lines[0].text,
            "  ◆ iteration 1 · current · 0001-reproducible-go-bindings"
        );
        assert!(lines[1].text.contains("[1/6] Propose · worker"));
        assert!(!lines.iter().any(|line| line.text.contains("frontier")));
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

        assert_eq!(dashboard.header_text(), "opsx-build  iteration 2/10  /repo");
        assert!(dashboard.panels.is_empty());
        let lines = dashboard.render_lines_at(Instant::now());
        assert!(lines[0].text.contains("iteration 1 · slice-a"));
        assert!(lines[0].text.contains("1m 05s"));
        assert!(lines[0].text.contains("1234567890ab"));
        assert_eq!(lines[1].text, "  ◆ iteration 2 · current");

        dashboard.set_change_name(Some("slice-b".to_owned()));
        dashboard.set_stage(StageView {
            current: 2,
            total: 6,
            title: "Propose".to_owned(),
            started_at: Instant::now(),
        });
        let lines = dashboard.render_lines_at(Instant::now());
        assert_eq!(lines[1].text, "  ◆ iteration 2 · current · slice-b");
        assert!(lines[2].text.contains("[2/6] Propose"));
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
        assert!(!dashboard.stop_after_iteration_requested());
        assert_eq!(
            dashboard.handle_event(Event::Key(event::KeyEvent::new(
                KeyCode::Char('y'),
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
    fn plain_p_requires_confirmation_before_an_immediate_pause() {
        let mut dashboard = dashboard();
        assert_eq!(
            dashboard.handle_event(Event::Key(event::KeyEvent::new(
                KeyCode::Char('p'),
                KeyModifiers::NONE,
            ))),
            StreamControl::None
        );
        assert_eq!(
            dashboard.pending_confirmation,
            Some(PendingConfirmation::Pause)
        );
        assert_eq!(
            dashboard.handle_event(Event::Key(event::KeyEvent::new(
                KeyCode::Char('n'),
                KeyModifiers::NONE,
            ))),
            StreamControl::None
        );
        assert_eq!(dashboard.pending_confirmation, None);

        dashboard.handle_event(Event::Key(event::KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::NONE,
        )));
        assert_eq!(
            dashboard.handle_event(Event::Key(event::KeyEvent::new(
                KeyCode::Char('y'),
                KeyModifiers::NONE,
            ))),
            StreamControl::Pause
        );
        assert!(
            dashboard.panels[0]
                .lines
                .back()
                .unwrap()
                .text
                .contains("checkpoint will be preserved")
        );
    }

    #[test]
    fn plain_f_requests_frontier_escalation() {
        let mut dashboard = dashboard();
        assert_eq!(
            dashboard.handle_event(Event::Key(event::KeyEvent::new(
                KeyCode::Char('f'),
                KeyModifiers::NONE,
            ))),
            StreamControl::None
        );
        assert_eq!(
            dashboard.handle_event(Event::Key(event::KeyEvent::new(
                KeyCode::Char('y'),
                KeyModifiers::NONE,
            ))),
            StreamControl::Escalate
        );
        assert!(
            dashboard.panels[0]
                .lines
                .back()
                .unwrap()
                .text
                .contains("frontier replanning")
        );
    }

    #[test]
    fn local_only_dashboard_does_not_offer_frontier_escalation() {
        let mut dashboard = dashboard();
        dashboard.set_frontier_enabled(false);
        assert_eq!(
            dashboard.handle_event(Event::Key(event::KeyEvent::new(
                KeyCode::Char('f'),
                KeyModifiers::NONE,
            ))),
            StreamControl::None
        );
        assert_eq!(dashboard.pending_confirmation, None);
        assert!(
            dashboard.panels[0]
                .lines
                .back()
                .unwrap()
                .text
                .contains("disabled")
        );
    }

    #[test]
    fn frontier_escalation_does_not_interrupt_a_milestone_phase() {
        let mut dashboard = dashboard();
        dashboard.set_stage(StageView {
            current: 3,
            total: 7,
            title: "Proposal commit".to_owned(),
            started_at: Instant::now(),
        });
        assert_eq!(
            dashboard.handle_event(Event::Key(event::KeyEvent::new(
                KeyCode::Char('f'),
                KeyModifiers::NONE,
            ))),
            StreamControl::None
        );
        assert!(
            dashboard
                .panels
                .last()
                .unwrap()
                .lines
                .back()
                .unwrap()
                .text
                .contains("available only")
        );
    }

    #[test]
    fn plain_c_queues_one_compaction_per_stream() {
        let mut dashboard = dashboard();
        let compact = Event::Key(event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        let confirm = Event::Key(event::KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(dashboard.handle_event(compact.clone()), StreamControl::None);
        assert_eq!(
            dashboard.handle_event(confirm.clone()),
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
            StreamControl::None
        );
        assert_eq!(dashboard.handle_event(confirm), StreamControl::Compact);
    }

    #[test]
    fn capital_c_queues_one_context_inspection_per_stream() {
        let mut dashboard = dashboard();
        let context = Event::Key(event::KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::SHIFT,
        ));
        let confirm = Event::Key(event::KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(dashboard.handle_event(context.clone()), StreamControl::None);
        assert_eq!(
            dashboard.handle_event(confirm.clone()),
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
            StreamControl::None
        );
        assert_eq!(dashboard.handle_event(confirm), StreamControl::Context);
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

        dashboard.handle_event(Event::Key(event::KeyEvent::new(
            KeyCode::Char('i'),
            KeyModifiers::NONE,
        )));
        dashboard.handle_event(Event::Key(event::KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        assert_eq!(
            dashboard.handle_event(Event::Key(event::KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            ))),
            StreamControl::None
        );
        assert!(dashboard.injection_input.is_none());
    }

    #[test]
    fn sanitizes_terminal_control_characters() {
        assert_eq!(sanitize("safe\u{1b}[2J\ttext"), "safe[2J text");
    }

    #[test]
    fn codex_redraw_does_not_erase_unchanged_keyboard_help() {
        let mut dashboard = dashboard();
        let mut output = Vec::new();
        dashboard.draw_to(&mut output, 180, 12).unwrap();
        assert!(
            String::from_utf8(output.clone())
                .unwrap()
                .contains("p pause")
        );

        output.clear();
        dashboard.push(&StreamItem::Codex("new Codex output".to_owned()));
        dashboard.spinner_index += 1;
        dashboard.draw_to(&mut output, 180, 12).unwrap();

        let frame = String::from_utf8(output).unwrap();
        assert!(frame.contains("new Codex output"));
        assert!(!frame.contains(&Clear(ClearType::All).to_string()));
        assert!(!frame.contains(&MoveTo(0, 11).to_string()));
        assert!(!frame.contains("p pause"));
    }

    #[test]
    fn keyboard_help_refreshes_for_controls_and_terminal_resize() {
        let mut dashboard = dashboard();
        let mut output = Vec::new();
        dashboard.draw_to(&mut output, 180, 12).unwrap();

        output.clear();
        dashboard.handle_event(Event::Key(event::KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::NONE,
        )));
        dashboard.draw_to(&mut output, 180, 12).unwrap();
        let frame = String::from_utf8(output.clone()).unwrap();
        assert!(frame.contains(PendingConfirmation::Pause.prompt()));
        assert!(!frame.contains(&Clear(ClearType::All).to_string()));

        output.clear();
        dashboard.handle_event(Event::Key(event::KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )));
        dashboard.draw_to(&mut output, 180, 12).unwrap();
        assert!(
            String::from_utf8(output.clone())
                .unwrap()
                .contains("p pause")
        );

        output.clear();
        dashboard.draw_to(&mut output, 100, 15).unwrap();
        let frame = String::from_utf8(output).unwrap();
        assert!(frame.contains(&Clear(ClearType::All).to_string()));
        assert!(frame.contains(&MoveTo(0, 14).to_string()));
        assert!(frame.contains("p pause"));
    }

    #[test]
    fn collapsing_output_clears_old_body_rows_without_erasing_help() {
        let mut dashboard = dashboard();
        dashboard.push(&StreamItem::Codex("first\nsecond\nthird".to_owned()));
        let mut output = Vec::new();
        dashboard.draw_to(&mut output, 120, 10).unwrap();

        output.clear();
        dashboard.panels[0].expanded = false;
        dashboard.draw_to(&mut output, 120, 10).unwrap();
        let frame = String::from_utf8(output).unwrap();
        for row in 4..9 {
            assert!(frame.contains(&format!(
                "{}{}",
                MoveTo(0, row),
                Clear(ClearType::CurrentLine)
            )));
        }
        assert!(!frame.contains(&Clear(ClearType::All).to_string()));
        assert!(!frame.contains(&MoveTo(0, 9).to_string()));
        assert!(!frame.contains("second"));
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
