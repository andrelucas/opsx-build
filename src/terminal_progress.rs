use std::io::Write;

const BUSY: &[u8] = b"\x1b]9;4;3\x07";
const CLEAR: &[u8] = b"\x1b]9;4;0\x07";

/// The terminal owns the animation; this guard owns its lifetime across stages/retries.
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
    in_tmux: bool,
    in_screen: bool,
) -> bool {
    // tmux 3.7+ tracks progress per pane and forwards it to capable clients.
    // Let it handle routing; passthrough would bypass its pane bookkeeping.
    let expected_program = if in_tmux { "tmux" } else { "iTerm.app" };
    if !terminal || program != Some(expected_program) || in_screen {
        return false;
    }
    let Some(version) = version else {
        return false;
    };
    let mut parts = version.split(|character: char| !character.is_ascii_digit());
    let mut numbers = [0_u32; 3];
    let components = if in_tmux { 2 } else { 3 };
    for number in &mut numbers[..components] {
        let Some(value) = parts.next().and_then(|part| part.parse().ok()) else {
            return false;
        };
        *number = value;
    }
    numbers >= if in_tmux { [3, 7, 0] } else { [3, 6, 6] }
}

#[cfg(test)]
mod tests {
    use std::{io, panic::AssertUnwindSafe};

    use super::*;

    #[test]
    fn requires_a_direct_supported_iterm_terminal() {
        for version in ["3.6.6", "3.7.0beta1", "3.7.2", "3.10.0", "4.0.0"] {
            assert!(supported(
                true,
                Some("iTerm.app"),
                Some(version),
                false,
                false
            ));
        }
        for version in [None, Some(""), Some("3.6.5"), Some("3.7"), Some("unknown")] {
            assert!(!supported(true, Some("iTerm.app"), version, false, false));
        }
        for program in [None, Some("Apple_Terminal"), Some("ghostty"), Some("tmux")] {
            assert!(!supported(true, program, Some("3.7.2"), false, false));
        }
        assert!(!supported(
            false,
            Some("iTerm.app"),
            Some("3.7.2"),
            false,
            false
        ));
        assert!(!supported(
            true,
            Some("iTerm.app"),
            Some("3.7.2"),
            true,
            false
        ));
        assert!(!supported(
            true,
            Some("iTerm.app"),
            Some("3.7.2"),
            false,
            true
        ));
    }

    #[test]
    fn tmux_uses_its_own_version_and_native_progress_support() {
        for version in ["3.7", "3.7c", "3.8", "4.0"] {
            assert!(supported(true, Some("tmux"), Some(version), true, false));
        }
        for version in [None, Some("3.6b"), Some("3"), Some("next-3.7")] {
            assert!(!supported(true, Some("tmux"), version, true, false));
        }
        assert!(!supported(false, Some("tmux"), Some("3.7c"), true, false));
        assert!(!supported(true, Some("tmux"), Some("3.7c"), true, true));
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
