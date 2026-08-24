use std::{
    io::IsTerminal,
    sync::Mutex,
    time::{Duration, Instant},
};

use console::{Style, Term};
use crossterm::style::Color;
use indicatif::{ProgressBar, ProgressStyle};

use crate::{
    dashboard::{
        CampaignDashboardView, CampaignIterationDashboardView, StageView, StreamDashboard,
    },
    stream::{StreamControl, StreamItem},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CampaignView {
    pub iteration: u32,
    pub max_iterations: Option<u32>,
    pub completed: Vec<CampaignIterationView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CampaignIterationView {
    pub iteration: u32,
    pub change: String,
    pub final_head: Option<String>,
    pub elapsed_seconds: Option<u64>,
}

pub trait Ui {
    /// Whether Claude stages should use persistent bidirectional stream input.
    fn supports_stream_input(&self) -> bool {
        false
    }
    fn banner(&self, repo: &str);
    fn change_name(&self, change: Option<&str>);
    fn campaign(&self, _campaign: Option<CampaignView>) {}
    fn stop_after_iteration_requested(&self) -> bool {
        false
    }
    fn stage(&self, current: usize, total: usize, title: &str);
    fn info(&self, message: &str);
    fn warn(&self, message: &str);
    fn success(&self, message: &str);
    fn failure(&self, message: &str);
    fn command(&self, command: &str);
    fn debug(&self, message: &str);
    fn debug_prompt(&self, label: &str, prompt: &str);
    fn start_stream(&self, message: &str);
    /// Poll dashboard input for a command to send to the current subprocess.
    fn poll_stream(&self) -> StreamControl;
    /// Report that a queued message was written to the subprocess input pipe.
    fn stream_message_sent(&self, message: &str);
    fn stream_item(&self, item: &StreamItem);
    fn finish_stream(&self, success: bool, message: &str);
    fn finish_dashboard(&self);
    fn output(&self, stdout: &str, stderr: &str);
    fn start_activity(&self, message: &str) -> Option<ProgressBar>;
    fn finish_activity(&self, spinner: Option<ProgressBar>, success: bool, message: &str);
}

pub struct TerminalUi {
    verbose: bool,
    debug: bool,
    interactive: bool,
    dashboard_enabled: bool,
    state: Mutex<TerminalState>,
}

#[derive(Default)]
struct TerminalState {
    repo: String,
    change_name: Option<String>,
    stage: Option<StageView>,
    campaign: Option<CampaignView>,
    dashboard: Option<StreamDashboard>,
}

impl TerminalUi {
    pub fn new(verbose: bool, debug: bool, workflow_dashboard: bool) -> Self {
        let stderr_terminal = std::io::stderr().is_terminal();
        Self {
            verbose,
            debug,
            interactive: stderr_terminal,
            dashboard_enabled: workflow_dashboard
                && stderr_terminal
                && std::io::stdin().is_terminal(),
            state: Mutex::new(TerminalState::default()),
        }
    }

    fn write_line(&self, message: &str) {
        let _ = Term::stderr().write_line(message);
    }

    fn dashboard_message(&self, label: &str, color: Color, message: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        let Some(dashboard) = state.dashboard.as_mut() else {
            return false;
        };
        dashboard.push_message(label, color, message);
        true
    }
}

impl Ui for TerminalUi {
    fn supports_stream_input(&self) -> bool {
        self.dashboard_enabled
    }

    fn banner(&self, repo: &str) {
        self.state.lock().unwrap().repo = repo.to_owned();
        let title = Style::new().bold().cyan().apply_to("opsx-build");
        self.write_line(&format!("{title}  {repo}"));
    }

    fn change_name(&self, change: Option<&str>) {
        let change = change.map(str::to_owned);
        let mut state = self.state.lock().unwrap();
        state.change_name = change.clone();
        if let Some(dashboard) = state.dashboard.as_mut() {
            dashboard.set_change_name(change);
        }
    }

    fn campaign(&self, campaign: Option<CampaignView>) {
        let dashboard_campaign = campaign.as_ref().map(campaign_dashboard_view);
        let mut state = self.state.lock().unwrap();
        let previous_iteration = state.campaign.as_ref().map(|campaign| campaign.iteration);
        state.campaign = campaign;
        if let Some(dashboard) = state.dashboard.as_mut() {
            dashboard.set_campaign(dashboard_campaign);
            return;
        }
        let campaign = state.campaign.clone();
        drop(state);
        if let Some(campaign) = campaign
            && previous_iteration != Some(campaign.iteration)
        {
            let position = campaign.max_iterations.map_or_else(
                || campaign.iteration.to_string(),
                |maximum| format!("{}/{maximum}", campaign.iteration),
            );
            self.write_line(&format!(
                "{} Campaign iteration {position}",
                Style::new().bold().cyan().apply_to("◆")
            ));
        }
    }

    fn stop_after_iteration_requested(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .dashboard
            .as_ref()
            .is_some_and(StreamDashboard::stop_after_iteration_requested)
    }

    fn stage(&self, current: usize, total: usize, title: &str) {
        let stage = StageView {
            current,
            total,
            title: title.to_owned(),
            started_at: Instant::now(),
        };
        let mut state = self.state.lock().unwrap();
        state.stage = Some(stage.clone());
        if let Some(dashboard) = state.dashboard.as_mut() {
            dashboard.set_stage(stage);
            return;
        }
        if self.dashboard_enabled {
            let repo = state.repo.clone();
            let change_name = state.change_name.clone();
            let campaign = state.campaign.as_ref().map(campaign_dashboard_view);
            drop(state);
            match StreamDashboard::enter(repo, change_name, campaign) {
                Ok(mut dashboard) => {
                    dashboard.set_stage(stage);
                    self.state.lock().unwrap().dashboard = Some(dashboard);
                    return;
                }
                Err(error) => self.warn(&format!(
                    "Could not start terminal dashboard ({error}); using linear output"
                )),
            }
        } else {
            drop(state);
        }
        let count = Style::new().dim().apply_to(format!("[{current}/{total}]"));
        let title = Style::new().bold().apply_to(title);
        self.write_line("");
        self.write_line(&format!("{count} {title}"));
    }

    fn info(&self, message: &str) {
        if self.dashboard_message("info", Color::Blue, message) {
            return;
        }
        self.write_line(&format!("{} {message}", Style::new().blue().apply_to("●")));
    }

    fn warn(&self, message: &str) {
        if self.dashboard_message("warn", Color::Yellow, message) {
            return;
        }
        self.write_line(&format!(
            "{} {message}",
            Style::new().yellow().apply_to("!")
        ));
    }

    fn success(&self, message: &str) {
        if self.dashboard_message("ok", Color::Green, message) {
            return;
        }
        self.write_line(&format!("{} {message}", Style::new().green().apply_to("✓")));
    }

    fn failure(&self, message: &str) {
        if self.dashboard_message("error", Color::Red, message) {
            return;
        }
        self.write_line(&format!("{} {message}", Style::new().red().apply_to("✗")));
    }

    fn command(&self, command: &str) {
        if self.verbose || self.debug {
            if self.dashboard_message("$", Color::DarkGrey, command) {
                return;
            }
            self.write_line(&format!("  {} {command}", Style::new().dim().apply_to("$")));
        }
    }

    fn debug(&self, message: &str) {
        if self.debug {
            if self.dashboard_message("debug", Color::Magenta, message) {
                return;
            }
            let label = Style::new().magenta().apply_to("debug");
            self.write_line(&format!("  {label} {message}"));
        }
    }

    fn debug_prompt(&self, label: &str, prompt: &str) {
        if !self.debug {
            return;
        }
        if self.dashboard_message("prompt", Color::Magenta, &format!("{label}\n{prompt}")) {
            return;
        }
        let marker = Style::new().magenta().apply_to("prompt");
        self.write_line(&format!("  {marker} {label}"));
        for line in prompt.lines() {
            self.write_line(&format!("    {line}"));
        }
    }

    fn start_stream(&self, message: &str) {
        if !self.dashboard_enabled {
            self.info(message);
            return;
        }
        let (repo, change_name, stage, campaign) = {
            let mut state = self.state.lock().unwrap();
            if let Some(dashboard) = state.dashboard.as_mut() {
                dashboard.start_stream(message);
                return;
            }
            (
                state.repo.clone(),
                state.change_name.clone(),
                state.stage.clone(),
                state.campaign.as_ref().map(campaign_dashboard_view),
            )
        };
        match StreamDashboard::enter(repo, change_name, campaign) {
            Ok(mut dashboard) => {
                if let Some(stage) = stage {
                    dashboard.set_stage(stage);
                }
                dashboard.start_stream(message);
                self.state.lock().unwrap().dashboard = Some(dashboard);
            }
            Err(error) => {
                self.warn(&format!(
                    "Could not start terminal dashboard ({error}); using linear output"
                ));
                self.info(message);
            }
        }
    }

    fn poll_stream(&self) -> StreamControl {
        self.state
            .lock()
            .unwrap()
            .dashboard
            .as_mut()
            .map_or(StreamControl::None, StreamDashboard::poll)
    }

    fn stream_message_sent(&self, message: &str) {
        if self.dashboard_message("sent", Color::Green, message) {
            return;
        }
        self.write_line(&format!("{} {message}", Style::new().green().apply_to("→")));
    }

    fn stream_item(&self, item: &StreamItem) {
        if let Some(dashboard) = self.state.lock().unwrap().dashboard.as_mut() {
            dashboard.push(item);
            return;
        }
        let (label, style, text) = match item {
            StreamItem::Assistant(text) => ("claude", Style::new().cyan(), text),
            StreamItem::Subagent(text) => ("agent", Style::new().blue(), text),
            StreamItem::Tool(text) => ("tool", Style::new().yellow(), text),
            StreamItem::ToolResult(text) => ("result", Style::new().dim(), text),
            StreamItem::Lifecycle(text) => ("event", Style::new().magenta(), text),
            StreamItem::Raw(text) => ("json", Style::new().dim(), text),
        };
        let label = style.bold().apply_to(label);
        for (index, line) in text.lines().enumerate() {
            if index == 0 {
                self.write_line(&format!("  {label} {line}"));
            } else {
                self.write_line(&format!("         {line}"));
            }
        }
    }

    fn finish_stream(&self, success: bool, message: &str) {
        if let Some(dashboard) = self.state.lock().unwrap().dashboard.as_mut() {
            dashboard.finish_stream(success, message);
            return;
        }
        self.finish_activity(None, success, message);
    }

    fn finish_dashboard(&self) {
        if let Some(mut dashboard) = self.state.lock().unwrap().dashboard.take() {
            dashboard.leave();
        }
    }

    fn output(&self, stdout: &str, stderr: &str) {
        if !self.verbose {
            return;
        }
        if self.state.lock().unwrap().dashboard.is_some() {
            if !stdout.is_empty() {
                self.dashboard_message("stdout", Color::DarkGrey, stdout);
            }
            if !stderr.is_empty() {
                self.dashboard_message("stderr", Color::Yellow, stderr);
            }
            return;
        }
        for line in stdout.lines() {
            self.write_line(&format!("  {line}"));
        }
        for line in stderr.lines() {
            self.write_line(&format!("  {}", Style::new().yellow().apply_to(line)));
        }
    }

    fn start_activity(&self, message: &str) -> Option<ProgressBar> {
        if let Some(dashboard) = self.state.lock().unwrap().dashboard.as_mut() {
            dashboard.start_activity(message);
            return None;
        }
        if !self.interactive {
            self.info(message);
            return None;
        }

        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .expect("valid spinner template")
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        spinner.set_message(message.to_owned());
        spinner.enable_steady_tick(Duration::from_millis(90));
        Some(spinner)
    }

    fn finish_activity(&self, spinner: Option<ProgressBar>, success: bool, message: &str) {
        if let Some(spinner) = spinner {
            spinner.finish_and_clear();
        }
        if let Some(dashboard) = self.state.lock().unwrap().dashboard.as_mut() {
            dashboard.finish_activity(success, message);
            return;
        }
        if success {
            self.success(message);
        } else {
            self.failure(message);
        }
    }
}

fn campaign_dashboard_view(campaign: &CampaignView) -> CampaignDashboardView {
    CampaignDashboardView {
        iteration: campaign.iteration,
        max_iterations: campaign.max_iterations,
        completed: campaign
            .completed
            .iter()
            .map(|entry| CampaignIterationDashboardView {
                iteration: entry.iteration,
                change: entry.change.clone(),
                final_head: entry.final_head.clone(),
                elapsed_seconds: entry.elapsed_seconds,
            })
            .collect(),
    }
}
