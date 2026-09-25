use std::io::Write;

const BUSY: &[u8] = b"\x1b]9;4;3\x07";
const CLEAR: &[u8] = b"\x1b]9;4;0\x07";

/// iTerm2 owns the animation; this guard owns its lifetime across stages/retries.
pub(crate) struct TerminalProgress<W: Write> {
    output: W,
    enabled: bool,
}

impl<W: Write> TerminalProgress<W> {
    pub(crate) fn new(output: W, enabled: bool) -> Self {
        let mut progress = Self { output, enabled };
        if enabled {
            progress.send(BUSY);
        }
        progress
    }

    fn send(&mut self, sequence: &[u8]) {
        // An optional terminal decoration must never fail the workflow.
        let _ = self.output.write_all(sequence);
        let _ = self.output.flush();
    }
}

impl<W: Write> Drop for TerminalProgress<W> {
    fn drop(&mut self) {
        if self.enabled {
            self.send(CLEAR);
        }
    }
}

pub(crate) fn supported(
    terminal: bool,
    program: Option<&str>,
    version: Option<&str>,
    multiplexed: bool,
) -> bool {
    if !terminal || program != Some("iTerm.app") || multiplexed {
        return false;
    }
    let Some(version) = version else {
        return false;
    };
    let mut parts = version.split(|character: char| !character.is_ascii_digit());
    let mut numbers = [0_u32; 3];
    for number in &mut numbers {
        let Some(value) = parts.next().and_then(|part| part.parse().ok()) else {
            return false;
        };
        *number = value;
    }
    numbers >= [3, 6, 6]
}

#[cfg(test)]
mod tests {
    use std::{io, panic::AssertUnwindSafe};

    use super::*;

    #[test]
    fn requires_a_direct_supported_iterm_terminal() {
        for version in ["3.6.6", "3.7.0beta1", "3.7.2", "3.10.0", "4.0.0"] {
            assert!(supported(true, Some("iTerm.app"), Some(version), false));
        }
        for version in [None, Some(""), Some("3.6.5"), Some("3.7"), Some("unknown")] {
            assert!(!supported(true, Some("iTerm.app"), version, false));
        }
        for program in [None, Some("Apple_Terminal"), Some("ghostty"), Some("tmux")] {
            assert!(!supported(true, program, Some("3.7.2"), false));
        }
        assert!(!supported(false, Some("iTerm.app"), Some("3.7.2"), false));
        assert!(!supported(true, Some("iTerm.app"), Some("3.7.2"), true));
    }

    #[test]
    fn stays_busy_until_the_owner_finishes() {
        let mut output = Vec::new();
        {
            let progress = TerminalProgress::new(&mut output, true);
            assert_eq!(*progress.output, BUSY);
            // Intermediate stage success/failure has no effect on this guard.
        }
        assert_eq!(output, [BUSY, CLEAR].concat());
    }

    #[test]
    fn clears_on_error_return_and_unwinding() {
        fn fail(output: &mut Vec<u8>) -> io::Result<()> {
            let _progress = TerminalProgress::new(output, true);
            Err(io::Error::other("run stopped"))
        }
        let mut output = Vec::new();
        assert!(fail(&mut output).is_err());
        assert_eq!(output, [BUSY, CLEAR].concat());

        output.clear();
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _progress = TerminalProgress::new(&mut output, true);
            panic!("run unwound");
        }));
        assert!(result.is_err());
        assert_eq!(output, [BUSY, CLEAR].concat());
    }

    #[test]
    fn disabled_progress_emits_nothing() {
        let mut output = Vec::new();
        drop(TerminalProgress::new(&mut output, false));
        assert!(output.is_empty());
    }
}
