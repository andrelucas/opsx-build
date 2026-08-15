use std::{io::IsTerminal, time::Duration};

use console::{Style, Term};
use indicatif::{ProgressBar, ProgressStyle};

use crate::stream::StreamItem;

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
    fn stream_item(&self, item: &StreamItem);
    fn output(&self, stdout: &str, stderr: &str);
    fn start_activity(&self, message: &str) -> Option<ProgressBar>;
    fn finish_activity(&self, spinner: Option<ProgressBar>, success: bool, message: &str);
}

pub struct TerminalUi {
    verbose: bool,
    debug: bool,
    interactive: bool,
}

impl TerminalUi {
    pub fn new(verbose: bool, debug: bool) -> Self {
        Self {
            verbose,
            debug,
            interactive: std::io::stderr().is_terminal(),
        }
    }

    fn write_line(&self, message: &str) {
        let _ = Term::stderr().write_line(message);
    }
}

impl Ui for TerminalUi {
    fn banner(&self, repo: &str) {
        let title = Style::new().bold().cyan().apply_to("ospx-build");
        self.write_line(&format!("{title}  {repo}"));
    }

    fn stage(&self, current: usize, total: usize, title: &str) {
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

    fn stream_item(&self, item: &StreamItem) {
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
