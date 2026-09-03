//! OSC 133 shell integration markers.
//!
//! A shell that sources Berth's snippet brackets its prompt and each command,
//! which is what makes command blocks and reliable exit codes possible. The
//! markers are:
//!
//! - `OSC 133 ; A ST` prompt start
//! - `OSC 133 ; B ST` command start, the end of the prompt
//! - `OSC 133 ; C ST` output start, the command is now running
//! - `OSC 133 ; D ; <exit> ST` command end, with the exit code
//!
//! alacritty ignores these, so they are scanned out of the byte stream here.
//! The scanner is a state machine rather than a search because PTY output is
//! chunked arbitrarily: a marker routinely arrives split across two reads, and
//! anything that only searches within a chunk will miss those silently.

/// What a marker means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellEventKind {
    /// The prompt begins here.
    PromptStart,
    /// The prompt ends and user input begins here.
    CommandStart,
    /// The command is running; everything after this is its output.
    OutputStart,
    /// The command finished. `None` when the shell reported no usable code.
    CommandEnd { exit_code: Option<i32> },
}

/// A marker, with where it landed in the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellEvent {
    pub kind: ShellEventKind,
    /// Row in the visible grid at the moment the marker was reached.
    pub line: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Ordinary output.
    Text,
    /// Seen ESC.
    Escape,
    /// Seen ESC ], collecting the parameter string.
    Osc,
    /// Seen ESC inside an OSC, which may begin the ST terminator.
    OscEscape,
}

/// Longest parameter string kept while scanning. An OSC that runs longer than
/// this is not one of ours, and buffering it unboundedly would let a remote
/// grow the client's memory without limit.
const MAX_PARAM: usize = 128;

/// Splits a byte stream into runs of ordinary output and OSC 133 markers.
#[derive(Debug, Clone)]
pub struct Scanner {
    state: State,
    param: Vec<u8>,
    /// True once the parameter is known not to start with `133;`.
    uninteresting: bool,
}

/// One piece of a scanned chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece<'a> {
    /// Bytes to feed to the terminal parser.
    Bytes(&'a [u8]),
    /// A recognised marker. Its bytes are also passed through as `Bytes`, so
    /// the parser's state machine sees exactly the original stream.
    Marker(ShellEventKind),
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    pub fn new() -> Self {
        Self {
            state: State::Text,
            param: Vec::new(),
            uninteresting: false,
        }
    }

    /// Scan `input`, returning pieces in stream order.
    ///
    /// Every input byte appears in exactly one `Bytes` piece, so a caller that
    /// concatenates them reproduces the input exactly. `Marker` pieces are
    /// interleaved at the point the marker completed, which is what lets the
    /// caller record the cursor position at that moment.
    pub fn scan<'a>(&mut self, input: &'a [u8]) -> Vec<Piece<'a>> {
        let mut pieces = Vec::new();
        let mut run_start = 0usize;

        for (index, byte) in input.iter().enumerate() {
            match self.state {
                State::Text => {
                    if *byte == 0x1b {
                        self.state = State::Escape;
                    }
                }
                State::Escape => {
                    if *byte == b']' {
                        self.state = State::Osc;
                        self.param.clear();
                        self.uninteresting = false;
                    } else if *byte == 0x1b {
                        // Another ESC restarts the sequence.
                    } else {
                        self.state = State::Text;
                    }
                }
                State::Osc => match *byte {
                    // BEL terminates an OSC.
                    0x07 => {
                        if let Some(kind) = self.finish() {
                            pieces.push(Piece::Bytes(&input[run_start..=index]));
                            run_start = index + 1;
                            pieces.push(Piece::Marker(kind));
                        }
                        self.state = State::Text;
                    }
                    0x1b => self.state = State::OscEscape,
                    b => {
                        if !self.uninteresting {
                            self.param.push(b);
                            if self.param.len() > MAX_PARAM || !self.could_be_ours() {
                                self.uninteresting = true;
                                self.param.clear();
                            }
                        }
                    }
                },
                State::OscEscape => {
                    // ESC \ is the string terminator.
                    if *byte == b'\\' {
                        if let Some(kind) = self.finish() {
                            pieces.push(Piece::Bytes(&input[run_start..=index]));
                            run_start = index + 1;
                            pieces.push(Piece::Marker(kind));
                        }
                        self.state = State::Text;
                    } else {
                        // Not a terminator after all; the ESC was payload.
                        self.state = State::Osc;
                        if !self.uninteresting {
                            self.param.push(0x1b);
                        }
                    }
                }
            }
        }

        if run_start < input.len() {
            pieces.push(Piece::Bytes(&input[run_start..]));
        }
        pieces
    }

    /// Whether the parameter so far can still be an OSC 133 marker. Lets a long
    /// unrelated OSC, such as a title, be discarded early.
    fn could_be_ours(&self) -> bool {
        const PREFIX: &[u8] = b"133;";
        let n = self.param.len().min(PREFIX.len());
        self.param[..n] == PREFIX[..n]
    }

    /// Interpret a completed parameter string.
    fn finish(&mut self) -> Option<ShellEventKind> {
        let param = std::mem::take(&mut self.param);
        let uninteresting = std::mem::replace(&mut self.uninteresting, false);
        if uninteresting {
            return None;
        }
        let text = std::str::from_utf8(&param).ok()?;
        let rest = text.strip_prefix("133;")?;
        let mut parts = rest.split(';');
        match parts.next()? {
            "A" => Some(ShellEventKind::PromptStart),
            "B" => Some(ShellEventKind::CommandStart),
            "C" => Some(ShellEventKind::OutputStart),
            "D" => {
                // `D` alone means finished with no code reported. Some shells
                // append key=value pairs after the code, which are ignored.
                let exit_code = parts.next().and_then(|c| c.trim().parse::<i32>().ok());
                Some(ShellEventKind::CommandEnd { exit_code })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect just the markers, and check the pass-through is byte exact.
    fn markers(chunks: &[&[u8]]) -> Vec<ShellEventKind> {
        let mut scanner = Scanner::new();
        let mut found = Vec::new();
        let mut passed = Vec::new();
        for chunk in chunks {
            for piece in scanner.scan(chunk) {
                match piece {
                    Piece::Bytes(b) => passed.extend_from_slice(b),
                    Piece::Marker(kind) => found.push(kind),
                }
            }
        }
        let original: Vec<u8> = chunks.concat();
        assert_eq!(passed, original, "every byte must pass through unchanged");
        found
    }

    #[test]
    fn plain_output_yields_no_markers() {
        assert!(markers(&[b"hello world\n"]).is_empty());
    }

    #[test]
    fn the_four_markers_are_recognised() {
        assert_eq!(
            markers(&[b"\x1b]133;A\x07"]),
            vec![ShellEventKind::PromptStart]
        );
        assert_eq!(
            markers(&[b"\x1b]133;B\x07"]),
            vec![ShellEventKind::CommandStart]
        );
        assert_eq!(
            markers(&[b"\x1b]133;C\x07"]),
            vec![ShellEventKind::OutputStart]
        );
        assert_eq!(
            markers(&[b"\x1b]133;D;0\x07"]),
            vec![ShellEventKind::CommandEnd { exit_code: Some(0) }]
        );
    }

    #[test]
    fn a_non_zero_exit_code_is_captured() {
        assert_eq!(
            markers(&[b"\x1b]133;D;127\x07"]),
            vec![ShellEventKind::CommandEnd {
                exit_code: Some(127)
            }]
        );
    }

    #[test]
    fn d_without_a_code_is_still_an_end() {
        assert_eq!(
            markers(&[b"\x1b]133;D\x07"]),
            vec![ShellEventKind::CommandEnd { exit_code: None }]
        );
    }

    #[test]
    fn extra_parameters_after_the_code_are_ignored() {
        assert_eq!(
            markers(&[b"\x1b]133;D;3;aid=42\x07"]),
            vec![ShellEventKind::CommandEnd { exit_code: Some(3) }]
        );
    }

    #[test]
    fn the_st_terminator_works_as_well_as_bel() {
        assert_eq!(
            markers(&[b"\x1b]133;A\x1b\\"]),
            vec![ShellEventKind::PromptStart]
        );
    }

    /// The reason this is a state machine: PTY reads split wherever they like.
    #[test]
    fn a_marker_split_across_chunks_is_still_found() {
        assert_eq!(
            markers(&[b"\x1b]13", b"3;D;7\x07"]),
            vec![ShellEventKind::CommandEnd { exit_code: Some(7) }]
        );
        assert_eq!(
            markers(&[b"\x1b", b"]", b"1", b"3", b"3", b";", b"A", b"\x07"]),
            vec![ShellEventKind::PromptStart]
        );
        // Split right before the ST terminator.
        assert_eq!(
            markers(&[b"\x1b]133;B\x1b", b"\\"]),
            vec![ShellEventKind::CommandStart]
        );
    }

    #[test]
    fn markers_are_found_among_ordinary_output() {
        assert_eq!(
            markers(&[b"before\x1b]133;A\x07prompt$ \x1b]133;B\x07cmd\n"]),
            vec![ShellEventKind::PromptStart, ShellEventKind::CommandStart]
        );
    }

    #[test]
    fn a_full_command_cycle_is_recognised_in_order() {
        let stream: &[u8] =
            b"\x1b]133;A\x07user@host$ \x1b]133;B\x07ls -l\r\n\x1b]133;C\x07total 0\r\n\x1b]133;D;0\x07";
        assert_eq!(
            markers(&[stream]),
            vec![
                ShellEventKind::PromptStart,
                ShellEventKind::CommandStart,
                ShellEventKind::OutputStart,
                ShellEventKind::CommandEnd { exit_code: Some(0) },
            ]
        );
    }

    #[test]
    fn other_osc_sequences_are_ignored() {
        assert!(markers(&[b"\x1b]0;a window title\x07"]).is_empty());
        assert!(markers(&[b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07"]).is_empty());
        assert!(markers(&[b"\x1b]1337;File=name\x07"]).is_empty());
    }

    #[test]
    fn a_malformed_marker_is_ignored_rather_than_guessed() {
        assert!(markers(&[b"\x1b]133;Z\x07"]).is_empty());
        assert!(
            markers(&[b"\x1b]133;D;notanumber\x07"])
                .contains(&ShellEventKind::CommandEnd { exit_code: None })
        );
    }

    #[test]
    fn csi_sequences_do_not_confuse_the_scanner() {
        assert_eq!(
            markers(&[b"\x1b[31mred\x1b[0m\x1b]133;A\x07"]),
            vec![ShellEventKind::PromptStart]
        );
    }

    /// A remote must not be able to grow the buffer without bound.
    #[test]
    fn an_enormous_unrelated_osc_is_discarded_early() {
        let mut payload = b"\x1b]0;".to_vec();
        payload.extend(std::iter::repeat_n(b'x', 100_000));
        payload.push(0x07);
        let mut scanner = Scanner::new();
        scanner.scan(&payload);
        assert!(scanner.param.len() <= MAX_PARAM);
    }

    #[test]
    fn an_overlong_133_parameter_is_dropped() {
        let mut payload = b"\x1b]133;D;".to_vec();
        payload.extend(std::iter::repeat_n(b'9', MAX_PARAM * 2));
        payload.push(0x07);
        let mut scanner = Scanner::new();
        let found: Vec<_> = scanner
            .scan(&payload)
            .into_iter()
            .filter_map(|p| match p {
                Piece::Marker(k) => Some(k),
                _ => None,
            })
            .collect();
        assert!(
            found.is_empty(),
            "an absurd parameter is not a valid marker"
        );
    }

    #[test]
    fn an_unterminated_marker_does_not_fire() {
        assert!(markers(&[b"\x1b]133;A"]).is_empty());
    }
}
