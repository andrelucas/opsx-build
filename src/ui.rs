use std::{io::IsTerminal, sync::Mutex, time::Duration};

use console::{Style, Term};
use indicatif::{ProgressBar, ProgressStyle};

use crate::{
    dashboard::{StageView, StreamDashboard},
    stream::StreamItem,
};

pub trait Ui {
    fn banner(&self, repo: &str);
    fn stage(&self, current: usize, total: usize, title: &str);
    fn info(&self, message: &str);
    fn warn(&self, message: &str);
    fn success(&self, message: &str);
    fn failure(&self, message: &str);
    fn command(&self, command: &str);
    fn debug(&self, message: &str);
    fn debug_prompt(&self, label: &str, prompt: &str);
    fn start_stream(&self, message: &str);
    /// Poll dashboard input. Returns true when the current subprocess should stop.
    fn poll_stream(&self) -> bool;
    fn stream_item(&self, item: &StreamItem);
    fn finish_stream(&self, success: bool, message: &str);
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
    stage: Option<StageView>,
    dashboard: Option<StreamDashboard>,
}

impl TerminalUi {
    pub fn new(verbose: bool, debug: bool, streaming: bool) -> Self {
        let stderr_terminal = std::io::stderr().is_terminal();
        Self {
            verbose,
            debug,
            interactive: stderr_terminal,
            dashboard_enabled: streaming && stderr_terminal && std::io::stdin().is_terminal(),
            state: Mutex::new(TerminalState::default()),
        }
    }

    fn write_line(&self, message: &str) {
        let _ = Term::stderr().write_line(message);
    }
}

impl Ui for TerminalUi {
    fn banner(&self, repo: &str) {
        self.state.lock().unwrap().repo = repo.to_owned();
        let title = Style::new().bold().cyan().apply_to("ospx-build");
        self.write_line(&format!("{title}  {repo}"));
    }

    fn stage(&self, current: usize, total: usize, title: &str) {
        self.state.lock().unwrap().stage = Some(StageView {
            current,
            total,
            title: title.to_owned(),
        });
        let count = Style::new().dim().apply_to(format!("[{current}/{total}]"));
        let title = Style::new().bold().apply_to(title);
        self.write_line("");
        self.write_line(&format!("{count} {title}"));
    }

    fn info(&self, message: &str) {
        self.write_line(&format!("{} {message}", Style::new().blue().apply_to("●")));
    }

    fn warn(&self, message: &str) {
        self.write_line(&format!(
            "{} {message}",
            Style::new().yellow().apply_to("!")
        ));
    }

    fn success(&self, message: &str) {
        self.write_line(&format!("{} {message}", Style::new().green().apply_to("✓")));
    }

    fn failure(&self, message: &str) {
        self.write_line(&format!("{} {message}", Style::new().red().apply_to("✗")));
    }

    fn command(&self, command: &str) {
        if self.verbose || self.debug {
            self.write_line(&format!("  {} {command}", Style::new().dim().apply_to("$")));
        }
    }

    fn debug(&self, message: &str) {
        if self.debug {
            let label = Style::new().magenta().apply_to("debug");
            self.write_line(&format!("  {label} {message}"));
        }
    }

    fn debug_prompt(&self, label: &str, prompt: &str) {
        if !self.debug {
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
        let (repo, stage) = {
            let state = self.state.lock().unwrap();
            (state.repo.clone(), state.stage.clone())
        };
        match StreamDashboard::enter(repo, stage, message.to_owned()) {
            Ok(dashboard) => self.state.lock().unwrap().dashboard = Some(dashboard),
            Err(error) => {
                self.warn(&format!(
                    "Could not start terminal dashboard ({error}); using linear stream output"
                ));
                self.info(message);
            }
        }
    }

    fn poll_stream(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .dashboard
            .as_mut()
            .is_some_and(StreamDashboard::poll)
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
        let label = style.apply_to(label);
        for (index, line) in text.lines().enumerate() {
            if index == 0 {
                self.write_line(&format!("  {label} {line}"));
            } else {
                self.write_line(&format!("         {line}"));
            }
        }
    }

    fn finish_stream(&self, success: bool, message: &str) {
        if let Some(mut dashboard) = self.state.lock().unwrap().dashboard.take() {
            dashboard.leave();
        }
        self.finish_activity(None, success, message);
    }

    fn output(&self, stdout: &str, stderr: &str) {
        if !self.verbose {
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
        if success {
            self.success(message);
        } else {
            self.failure(message);
        }
    }
}
