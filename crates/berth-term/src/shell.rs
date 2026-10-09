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
//! The engine has no use for these, so they are scanned out of the byte stream
//! here.
//!
//! The same scanner also picks out Berth's own agent signal,
//! `OSC 7717 ; berth ; ...` (D48), which a Claude Code hook writes into the
//! pane. This crate does not interpret it: the raw payload is handed up, and
//! checking and trusting it is `berth-core`'s job.
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

/// Longest agent signal kept. The format is capped at 1 KB (D48); anything
/// longer is not one Berth's hook wrote.
pub const MAX_AGENT_PARAM: usize = 1100;

/// What every agent signal's parameter string starts with.
const AGENT_PREFIX: &[u8] = b"7717;berth;";
const SHELL_PREFIX: &[u8] = b"133;";

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
    /// An agent signal's payload, everything after `7717;berth;`. Untrusted:
    /// any program in the pane can print one. Passed through as `Bytes` too.
    Agent(String),
}

/// What a completed OSC turned out to be.
enum Found {
    Shell(ShellEventKind),
    Agent(String),
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
        let mut index = 0usize;

        while index < input.len() {
            // Ordinary text, and the inside of an OSC that cannot be ours (a
            // window title, a hyperlink), are skipped to the next byte that
            // could matter, eight at a time: most output is one or the other.
            match self.state {
                State::Text => match find(&input[index..], 0x1b, 0x1b) {
                    Some(at) => index += at,
                    None => break,
                },
                State::Osc if self.uninteresting => match find(&input[index..], 0x07, 0x1b) {
                    Some(at) => index += at,
                    None => break,
                },
                _ => {}
            }
            let byte = &input[index];
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
                            pieces.push(match kind {
                                Found::Shell(kind) => Piece::Marker(kind),
                                Found::Agent(payload) => Piece::Agent(payload),
                            });
                        }
                        self.state = State::Text;
                    }
                    0x1b => self.state = State::OscEscape,
                    b => {
                        if !self.uninteresting {
                            self.param.push(b);
                            if self.param.len() > self.limit() || !self.could_be_ours() {
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
                            pieces.push(match kind {
                                Found::Shell(kind) => Piece::Marker(kind),
                                Found::Agent(payload) => Piece::Agent(payload),
                            });
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
            index += 1;
        }

        if run_start < input.len() {
            pieces.push(Piece::Bytes(&input[run_start..]));
        }
        pieces
    }

    /// Whether the parameter so far can still be an OSC 133 marker or an agent
    /// signal. Lets a long unrelated OSC, such as a title, be discarded early.
    fn could_be_ours(&self) -> bool {
        [SHELL_PREFIX, AGENT_PREFIX].iter().any(|prefix| {
            let n = self.param.len().min(prefix.len());
            self.param[..n] == prefix[..n]
        })
    }

    /// How long this parameter may grow: an agent signal carries a directory,
    /// a shell marker never carries more than an exit code.
    fn limit(&self) -> usize {
        if self.param.starts_with(AGENT_PREFIX) {
            MAX_AGENT_PARAM
        } else {
            MAX_PARAM
        }
    }

    /// Interpret a completed parameter string.
    fn finish(&mut self) -> Option<Found> {
        let param = std::mem::take(&mut self.param);
        let uninteresting = std::mem::replace(&mut self.uninteresting, false);
        if uninteresting {
            return None;
        }
        let text = std::str::from_utf8(&param).ok()?;
        if let Some(payload) = text.strip_prefix("7717;berth;") {
            return Some(Found::Agent(payload.to_string()));
        }
        let rest = text.strip_prefix("133;")?;
        let mut parts = rest.split(';');
        match parts.next()? {
            "A" => Some(Found::Shell(ShellEventKind::PromptStart)),
            "B" => Some(Found::Shell(ShellEventKind::CommandStart)),
            "C" => Some(Found::Shell(ShellEventKind::OutputStart)),
            "D" => {
                // `D` alone means finished with no code reported. Some shells
                // append key=value pairs after the code, which are ignored.
                let exit_code = parts.next().and_then(|c| c.trim().parse::<i32>().ok());
                Some(Found::Shell(ShellEventKind::CommandEnd { exit_code }))
            }
            _ => None,
        }
    }
}

/// The first byte in `hay` that is `a` or `b`, looked for eight bytes at a
/// time: a word holds a zero byte exactly where it equals the one sought.
fn find(hay: &[u8], a: u8, b: u8) -> Option<usize> {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
    let (wa, wb) = (ONES * u64::from(a), ONES * u64::from(b));
    let zero_at = |x: u64| x.wrapping_sub(ONES) & !x & HIGHS;
    let (words, rest) = hay.as_chunks::<8>();
    for (i, word) in words.iter().enumerate() {
        let w = u64::from_le_bytes(*word);
        let hits = zero_at(w ^ wa) | zero_at(w ^ wb);
        if hits != 0 {
            // A borrow can mark a byte past the first match, never before
            // it, so the lowest mark is exact.
            return Some(i * 8 + (hits.trailing_zeros() / 8) as usize);
        }
    }
    let at = words.len() * 8;
    rest.iter().position(|&x| x == a || x == b).map(|i| at + i)
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
                    Piece::Agent(_) => {}
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

    /// Agent payloads, with the same byte-exact pass-through check.
    fn agents(chunks: &[&[u8]]) -> Vec<String> {
        let mut scanner = Scanner::new();
        let mut found = Vec::new();
        let mut passed = Vec::new();
        for chunk in chunks {
            for piece in scanner.scan(chunk) {
                match piece {
                    Piece::Bytes(b) => passed.extend_from_slice(b),
                    Piece::Agent(payload) => found.push(payload),
                    Piece::Marker(_) => {}
                }
            }
        }
        assert_eq!(
            passed,
            chunks.concat(),
            "every byte must pass through unchanged"
        );
        found
    }

    #[test]
    fn an_agent_signal_hands_up_its_payload() {
        assert_eq!(
            agents(&[b"out\x1b]7717;berth;1;tok;stop;id;-;L3RtcA\x07more"]),
            ["1;tok;stop;id;-;L3RtcA"]
        );
    }

    #[test]
    fn an_agent_signal_split_across_reads_is_still_found() {
        assert_eq!(agents(&[b"\x1b]77", b"17;ber", b"th;1;a;b\x07"]), ["1;a;b"]);
    }

    /// A directory makes the payload longer than any shell marker, so the
    /// shell limit must not cut it off.
    #[test]
    fn a_long_agent_signal_within_its_limit_survives() {
        let body = "x".repeat(900);
        let signal = format!("\x1b]7717;berth;{body}\x07");
        assert_eq!(agents(&[signal.as_bytes()]), [body]);
    }

    #[test]
    fn an_overlong_agent_signal_is_dropped() {
        let body = "x".repeat(MAX_AGENT_PARAM + 10);
        let signal = format!("\x1b]7717;berth;{body}\x07");
        assert!(agents(&[signal.as_bytes()]).is_empty());
    }

    /// Another program's 7717, without the namespace, is not ours.
    #[test]
    fn another_7717_is_not_an_agent_signal() {
        assert!(agents(&[b"\x1b]7717;other;1\x07"]).is_empty());
    }

    #[test]
    fn shell_markers_and_agent_signals_share_a_stream() {
        let stream: &[u8] = b"\x1b]133;A\x07$ \x1b]7717;berth;p\x07";
        assert_eq!(markers(&[stream]), [ShellEventKind::PromptStart]);
        assert_eq!(agents(&[stream]), ["p"]);
    }

    proptest::proptest! {
        /// The word-at-a-time search finds what a byte-by-byte one does.
        #[test]
        fn find_agrees_with_a_plain_search(
            hay in proptest::collection::vec(
                proptest::prop_oneof![
                    proptest::strategy::Just(0x1bu8),
                    proptest::strategy::Just(0x07u8),
                    proptest::strategy::Just(0x00u8),
                    proptest::strategy::Just(0x1au8),
                    proptest::strategy::Just(0x80u8),
                    proptest::num::u8::ANY,
                ],
                0..64,
            ),
            a in proptest::prop_oneof![
                proptest::strategy::Just(0x1bu8),
                proptest::strategy::Just(0x07u8),
            ],
        ) {
            let plain = hay.iter().position(|&x| x == a || x == 0x1b);
            proptest::prop_assert_eq!(find(&hay, a, 0x1b), plain);
        }

        /// However the stream is cut, the same markers and signals come out
        /// and every byte passes through, titles and colours around them.
        #[test]
        fn scanning_does_not_depend_on_the_cuts(
            parts in proptest::collection::vec(
                proptest::prop_oneof![
                    proptest::strategy::Just(b"plain text ".to_vec()),
                    proptest::strategy::Just(b"\x1b[38;5;3mcol\x1b[0m".to_vec()),
                    proptest::strategy::Just(b"\x1b]0;a title\x07".to_vec()),
                    proptest::strategy::Just(b"\x1b]2;title st\x1b\\".to_vec()),
                    proptest::strategy::Just(b"\x1b]133;A\x07".to_vec()),
                    proptest::strategy::Just(b"\x1b]133;D;1\x1b\\".to_vec()),
                    proptest::strategy::Just(b"\x1b]7717;berth;sig\x07".to_vec()),
                    proptest::strategy::Just(b"\x1b\x1b]133;C\x07".to_vec()),
                ],
                0..20,
            ),
            cuts in proptest::collection::vec(1usize..17, 0..40),
        ) {
            let stream: Vec<u8> = parts.concat();
            let mut chunks: Vec<&[u8]> = Vec::new();
            let mut rest: &[u8] = &stream;
            for cut in cuts {
                if rest.is_empty() {
                    break;
                }
                let (head, tail) = rest.split_at(cut.min(rest.len()));
                chunks.push(head);
                rest = tail;
            }
            chunks.push(rest);
            proptest::prop_assert_eq!(markers(&chunks), markers(&[&stream]));
            proptest::prop_assert_eq!(agents(&chunks), agents(&[&stream]));
        }
    }
}
