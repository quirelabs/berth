//! Claude Code status and resume, from Claude's own hooks (D48).
//!
//! A hook in Berth's plugin writes one escape sequence into the pane:
//!
//! ```text
//! ESC ] 7717 ; berth ; 1 ; <token> ; <event> ; <session-id> ; <kind> ; <cwd> BEL
//! ```
//!
//! `berth-term` picks the payload out of the stream; this module decides
//! whether to believe it and what it means. Every field is untrusted: any
//! program in a pane can print the sequence. A signal is only believed when it
//! carries the pane's own token, and the session id, which ends up in
//! `claude --resume`, must be a strict UUID.
//!
//! State comes only from these signals, never from the screen. That is what is
//! left of D34, and the reason there is no heuristic anywhere in here.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};

/// The plugin and the shim, compiled in so the app can put them in place
/// without shipping loose files next to the binary.
const PLUGIN_JSON: &str = include_str!("../claude/plugin/.claude-plugin/plugin.json");
const HOOKS_JSON: &str = include_str!("../claude/plugin/hooks/hooks.json");
const HOOK_SH: &str = include_str!("../claude/plugin/hook.sh");
const SHIM_SH: &str = include_str!("../claude/shim.sh");
const ZSH_FILES: [(&str, &str); 4] = [
    (".zshenv", include_str!("../claude/shell/zsh/.zshenv")),
    (".zprofile", include_str!("../claude/shell/zsh/.zprofile")),
    (".zshrc", include_str!("../claude/shell/zsh/.zshrc")),
    (".zlogin", include_str!("../claude/shell/zsh/.zlogin")),
];
const BASH_INIT: &str = include_str!("../claude/shell/bash/berth.bash");

/// Longest directory believed from a signal, matching the format's cap.
const MAX_CWD: usize = 1024;

/// A pane's secret, set as `BERTH_PANE` in its environment and echoed back by
/// the hook. 128 random bits as 32 hex characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneToken(String);

impl PaneToken {
    /// A new token.
    ///
    /// Drawn from std's `RandomState`, whose keys come from the operating
    /// system's random source, so no crate is needed for it. The token keeps
    /// stray output from being believed; it is not a key protecting anything
    /// from code already running as the user.
    pub fn new() -> Self {
        let high = random_u64();
        let low = random_u64();
        Self(format!("{high:016x}{low:016x}"))
    }

    /// A token read back from somewhere Berth put one, such as a tmux
    /// session's environment after a relaunch. `None` unless it has the exact
    /// shape Berth writes.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        (text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| Self(text.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Compared without an early exit, so how long a comparison takes says
    /// nothing about how much of a guess was right.
    fn matches(&self, other: &str) -> bool {
        self.0.len() == other.len()
            && self
                .0
                .bytes()
                .zip(other.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    }
}

impl Default for PaneToken {
    fn default() -> Self {
        Self::new()
    }
}

fn random_u64() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hasher.write_u128(nanos);
    hasher.finish()
}

/// The line typed into a restored pane's shell to resume a conversation, or
/// `None` for an id that is not a strict UUID. Nothing else is ever typed: the
/// directory is the shell's own.
pub fn resume_line(session: &str) -> Option<String> {
    is_session_id(session).then(|| format!("claude --resume {session}"))
}

/// What Berth puts in a pane's environment so the hook can find its way back
/// (D48): the token, and the shim directory for the front of PATH. The pane's
/// terminal device is added by whoever opens the pty, since only it knows.
#[derive(Debug, Clone)]
pub struct PaneEnv {
    pub token: PaneToken,
    /// `None` when the shim could not be put in place; status then waits for
    /// the fallback rather than failing the pane.
    pub shim_dir: Option<PathBuf>,
    /// Berth's startup wrappers for zsh and bash, which put the shim back in
    /// front after the user's own startup files have run. Login profiles move
    /// it otherwise: macOS's `path_helper` does, and so does any `.zshrc` that
    /// prepends its own directories.
    pub shell_dir: Option<PathBuf>,
    /// Where the user's own zsh startup files are: their `ZDOTDIR`, or home.
    pub user_zdotdir: Option<PathBuf>,
    /// Whether the user had set `ZDOTDIR` themselves, so it is put back rather
    /// than left set to home.
    pub user_zdotdir_was_set: bool,
    /// A conversation for the shell's startup wrapper to resume before its
    /// first prompt. Only ever a strict UUID; see [`PaneEnv::resuming`].
    pub resume: Option<String>,
    /// A worker's hook socket and the binary that talks to it (D50): set, the
    /// hook tells the worker rather than writing to the terminal.
    pub hook: Option<(PathBuf, PathBuf)>,
}

impl PaneEnv {
    /// For a pane in a Berth whose support files live under `base`, or with
    /// none when they could not be put in place.
    pub fn new(base: Option<&Path>) -> Self {
        let user = std::env::var_os("ZDOTDIR").filter(|v| !v.is_empty());
        Self {
            token: PaneToken::new(),
            shim_dir: base.map(shim_dir),
            shell_dir: base.map(|b| b.join("shell")),
            user_zdotdir_was_set: user.is_some(),
            user_zdotdir: user.map(PathBuf::from).or_else(dirs::home_dir),
            resume: None,
            hook: None,
        }
    }

    /// Ask the shell to resume this conversation as it starts. Ignored unless
    /// `session` is a strict UUID.
    pub fn resuming(mut self, session: &str) -> Self {
        self.resume = is_session_id(session).then(|| session.to_ascii_lowercase());
        self
    }

    /// `PATH` with the shim directory first, from the PATH Berth itself was
    /// started with.
    pub fn path_with_shim(
        &self,
        inherited: Option<std::ffi::OsString>,
    ) -> Option<std::ffi::OsString> {
        let shim = self.shim_dir.as_ref()?;
        let rest = inherited.unwrap_or_default();
        let dirs =
            std::iter::once(shim.clone()).chain(std::env::split_paths(&rest).filter(|d| d != shim));
        std::env::join_paths(dirs).ok()
    }
}

/// Variables a running Claude Code session sets for the processes it starts,
/// which a Berth started from inside one would otherwise pass to every shell.
/// A `claude` seeing them believes it is a sub-session: it turns transcript
/// saving off, which makes resume impossible, and the shells could reach the
/// parent session's messaging socket.
///
/// Markers only. Settings a person sets on purpose, such as
/// `CLAUDE_CODE_USE_BEDROCK`, are never touched. Seen in Claude Code 2.1.280.
pub const SESSION_MARKERS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
];

/// Which hook fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Start,
    Prompt,
    Tool,
    Notify,
    Stop,
    End,
}

/// What a notification was about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Permission,
    Idle,
    None,
}

/// A signal that carried the right token and parsed cleanly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal {
    pub event: Event,
    /// A strict UUID, safe to pass to `claude --resume`.
    pub session_id: String,
    pub kind: Kind,
    /// The directory resume is scoped to. Absolute.
    pub cwd: PathBuf,
}

/// Believe a payload, or not.
///
/// `payload` is everything after `7717;berth;`, as `berth-term` hands it up.
/// Anything short of exactly the expected shape is `None`: a signal that has
/// to be guessed at is worse than a missing one.
pub fn parse(payload: &str, token: &PaneToken) -> Option<Signal> {
    let mut fields = payload.split(';');
    let (version, sent_token, event, session, kind, cwd) = (
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
    );
    if fields.next().is_some() || version != "1" || !token.matches(sent_token) {
        return None;
    }
    let event = match event {
        "start" => Event::Start,
        "prompt" => Event::Prompt,
        "tool" => Event::Tool,
        "notify" => Event::Notify,
        "stop" => Event::Stop,
        "end" => Event::End,
        _ => return None,
    };
    if !is_session_id(session) {
        return None;
    }
    let kind = match kind {
        "permission" => Kind::Permission,
        "idle" => Kind::Idle,
        "-" => Kind::None,
        _ => return None,
    };
    let cwd = decode_dir(cwd)?;
    Some(Signal {
        event,
        session_id: session.to_ascii_lowercase(),
        kind,
        cwd,
    })
}

/// Exactly `8-4-4-4-12` hex digits. This string is later an argument to
/// `claude --resume`, typed into a shell on remote hosts, so nothing else is
/// let through.
pub fn is_session_id(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn decode_dir(encoded: &str) -> Option<PathBuf> {
    let bytes = base64url(encoded)?;
    if bytes.is_empty() || bytes.len() > MAX_CWD || bytes.contains(&0) {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().any(char::is_control) {
        return None;
    }
    let path = PathBuf::from(text);
    path.is_absolute().then_some(path)
}

/// Unpadded base64url, the form the hook writes. Hand-rolled because it is
/// twenty lines and a crate for it would be a dependency (D48).
fn base64url(text: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        } as u32)
    }
    let text = text.trim_end_matches('=');
    if text.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    for chunk in text.as_bytes().chunks(4) {
        let mut acc = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            acc |= value(*c)? << (18 - 6 * i);
        }
        let bytes = acc.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    Some(out)
}

/// Why Claude is waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// A permission prompt.
    Permission,
    /// Idle for a while at the prompt.
    Idle,
    /// The turn ended; the next message is the user's.
    YourMove,
}

/// What a pane's Claude is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Working,
    Waiting(Wait),
    /// Exited after a finished turn.
    Finished,
    /// Exited in the middle of a turn: a quit, a hangup, a crash.
    Interrupted,
}

/// A Claude as the wire carries it, from what a tracker knows.
pub fn to_wire(tracker: &Tracker) -> Option<berth_proto::v1::Claude> {
    use berth_proto::v1::ClaudeState as W;
    let state = match tracker.state? {
        State::Working => W::Working,
        State::Waiting(Wait::Permission) => W::Permission,
        State::Waiting(Wait::Idle) => W::Idle,
        State::Waiting(Wait::YourMove) => W::YourMove,
        State::Finished => W::Finished,
        State::Interrupted => W::Interrupted,
    };
    Some(berth_proto::v1::Claude {
        state,
        session_id: tracker.session_id.clone(),
        cwd: tracker
            .cwd
            .as_ref()
            .map(|c| c.to_string_lossy().to_string()),
    })
}

/// A tracker that says what a worker reported (D50): the worker checked the
/// token, so the app takes its word.
pub fn from_wire(claude: &berth_proto::v1::Claude) -> Tracker {
    use berth_proto::v1::ClaudeState as W;
    let state = match claude.state {
        W::Working => State::Working,
        W::Permission => State::Waiting(Wait::Permission),
        W::Idle => State::Waiting(Wait::Idle),
        W::YourMove => State::Waiting(Wait::YourMove),
        W::Finished => State::Finished,
        W::Interrupted => State::Interrupted,
    };
    Tracker {
        session_id: claude.session_id.clone().filter(|s| is_session_id(s)),
        cwd: claude.cwd.as_ref().map(PathBuf::from),
        state: Some(state),
        settled: matches!(state, State::Waiting(_) | State::Finished),
    }
}

/// One pane's Claude, as its signals describe it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tracker {
    pub session_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub state: Option<State>,
    /// Whether a turn ended since the last prompt, which is what tells an end
    /// after a finished turn from an end in the middle of one.
    settled: bool,
}

impl Tracker {
    /// Fold one signal in.
    ///
    /// Every transition is idempotent, so the same event arriving twice, from
    /// a plugin that is also installed permanently for instance, changes
    /// nothing. That is why there is no deduplication anywhere.
    pub fn apply(&mut self, signal: &Signal) {
        if signal.event == Event::Start || self.session_id.is_none() {
            self.session_id = Some(signal.session_id.clone());
        }
        // A later signal for another session in the same pane, after `/clear`
        // for instance, is that session's news, not this one's.
        if self.session_id.as_deref() != Some(signal.session_id.as_str()) {
            return;
        }
        self.cwd = Some(signal.cwd.clone());
        self.state = Some(match signal.event {
            Event::Start => {
                self.settled = true;
                State::Waiting(Wait::YourMove)
            }
            Event::Prompt => {
                self.settled = false;
                State::Working
            }
            Event::Tool => match self.state {
                Some(State::Finished | State::Interrupted) => return,
                _ => State::Working,
            },
            Event::Notify => State::Waiting(match signal.kind {
                Kind::Permission => Wait::Permission,
                Kind::Idle => Wait::Idle,
                Kind::None => Wait::YourMove,
            }),
            Event::Stop => {
                self.settled = true;
                State::Waiting(Wait::YourMove)
            }
            Event::End if self.settled => State::Finished,
            Event::End => State::Interrupted,
        });
    }
}

/// Put the plugin and the shim under `base`: `base/claude-plugin/` and
/// `base/bin/claude`. Only files whose contents differ are written, so running
/// this on every launch costs nothing once they are there.
pub fn install(base: &Path) -> std::io::Result<()> {
    let plugin = base.join("claude-plugin");
    write_if_changed(
        &plugin.join(".claude-plugin/plugin.json"),
        PLUGIN_JSON,
        false,
    )?;
    write_if_changed(&plugin.join("hooks/hooks.json"), HOOKS_JSON, false)?;
    write_if_changed(&plugin.join("hook.sh"), HOOK_SH, true)?;
    write_if_changed(&base.join("bin/claude"), SHIM_SH, true)?;
    for (name, contents) in ZSH_FILES {
        write_if_changed(&base.join("shell/zsh").join(name), contents, false)?;
    }
    write_if_changed(&base.join("shell/bash/berth.bash"), BASH_INIT, false)?;
    Ok(())
}

/// Where the shim lives, for the front of a pane's PATH.
pub fn shim_dir(base: &Path) -> PathBuf {
    base.join("bin")
}

fn write_if_changed(path: &Path, contents: &str, executable: bool) -> std::io::Result<()> {
    if std::fs::read_to_string(path).ok().as_deref() != Some(contents) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let partial = path.with_extension("partial");
        std::fs::write(&partial, contents)?;
        std::fs::rename(&partial, path)?;
    }
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

/// Where Berth keeps the plugin and the shim: `BERTH_DATA` for tests,
/// otherwise the platform data directory beside `local-sessions.toml` (D46).
pub fn base_dir() -> Option<PathBuf> {
    if let Some(dir) = crate::env::data() {
        return Some(dir);
    }
    Some(dirs::data_dir()?.join("berth"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "48efac83-05d7-447d-b460-e5bbe01e0c7e";

    fn encode(path: &str) -> String {
        // The encoder the hook uses is `base64 | tr`; this is the same thing,
        // written out for the tests only.
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in path.as_bytes().chunks(3) {
            let mut acc = 0u32;
            for (i, b) in chunk.iter().enumerate() {
                acc |= (*b as u32) << (16 - 8 * i);
            }
            for i in 0..=chunk.len() {
                out.push(ALPHABET[((acc >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        out
    }

    fn payload(token: &PaneToken, event: &str, kind: &str, dir: &str) -> String {
        format!("1;{};{event};{ID};{kind};{}", token.as_str(), encode(dir))
    }

    #[test]
    fn a_well_formed_signal_is_believed() {
        let token = PaneToken::new();
        let signal = parse(
            &payload(&token, "notify", "permission", "/Users/a/b c"),
            &token,
        )
        .expect("believed");
        assert_eq!(signal.event, Event::Notify);
        assert_eq!(signal.kind, Kind::Permission);
        assert_eq!(signal.session_id, ID);
        assert_eq!(signal.cwd, PathBuf::from("/Users/a/b c"));
    }

    /// The token is what stops a file or a log that happens to contain the
    /// sequence from being believed.
    #[test]
    fn another_panes_token_is_not_believed() {
        let ours = PaneToken::new();
        let theirs = PaneToken::new();
        assert_ne!(ours, theirs);
        assert!(parse(&payload(&theirs, "stop", "-", "/tmp"), &ours).is_none());
    }

    #[test]
    fn a_session_id_must_be_a_strict_uuid() {
        assert!(is_session_id(ID));
        for bad in [
            "",
            "48efac83",
            "48efac83-05d7-447d-b460-e5bbe01e0c7",
            "48efac83-05d7-447d-b460-e5bbe01e0c7eX",
            "48efac83-05d7-447d-b460-e5bbe01e0c7g",
            "x; rm -rf ~-05d7-447d-b460-e5bbe01e0c7e",
            "48efac83-05d7-447d-b460-e5bb'01e0c7e",
        ] {
            assert!(!is_session_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn anything_off_shape_is_dropped() {
        let token = PaneToken::new();
        let t = token.as_str();
        let dir = encode("/tmp");
        for bad in [
            format!("2;{t};stop;{ID};-;{dir}"),
            format!("1;{t};explode;{ID};-;{dir}"),
            format!("1;{t};stop;not-a-uuid;-;{dir}"),
            format!("1;{t};stop;{ID};sideways;{dir}"),
            format!("1;{t};stop;{ID};-;{dir};extra"),
            format!("1;{t};stop;{ID};-"),
            format!("1;{t};stop;{ID};-;{}", encode("relative/path")),
            format!("1;{t};stop;{ID};-;{}", encode("/tmp/a\nb")),
            format!("1;{t};stop;{ID};-;!!!"),
            format!(
                "1;{t};stop;{ID};-;{}",
                encode(&format!("/{}", "x".repeat(MAX_CWD)))
            ),
        ] {
            assert!(parse(&bad, &token).is_none(), "{bad}");
        }
    }

    #[test]
    fn base64url_matches_what_the_hook_writes() {
        // `printf '%s' '/Users/me/my repo' | base64 | tr -d '\n=' | tr '+/' '-_'`
        assert_eq!(
            base64url("L1VzZXJzL21lL215IHJlcG8").as_deref(),
            Some(b"/Users/me/my repo".as_slice())
        );
        assert_eq!(base64url(&encode("/a")), Some(b"/a".to_vec()));
        assert_eq!(base64url("A"), None);
    }

    #[test]
    fn a_token_reads_back_only_in_its_own_shape() {
        let token = PaneToken::new();
        assert_eq!(PaneToken::parse(token.as_str()), Some(token.clone()));
        assert_eq!(
            PaneToken::parse(&format!("{}\n", token.as_str())),
            Some(token)
        );
        assert_eq!(PaneToken::parse("short"), None);
        assert_eq!(PaneToken::parse(&"g".repeat(32)), None);
    }

    fn signal(event: Event, kind: Kind) -> Signal {
        Signal {
            event,
            session_id: ID.into(),
            kind,
            cwd: PathBuf::from("/work"),
        }
    }

    fn run(events: &[(Event, Kind)]) -> Option<State> {
        let mut tracker = Tracker::default();
        for (event, kind) in events {
            tracker.apply(&signal(*event, *kind));
        }
        tracker.state
    }

    /// The order seen live in Claude Code 2.1.280, turn by turn.
    #[test]
    fn a_turn_goes_working_then_waiting_then_finished() {
        use Event::*;
        let n = Kind::None;
        assert_eq!(run(&[(Start, n)]), Some(State::Waiting(Wait::YourMove)));
        assert_eq!(run(&[(Start, n), (Prompt, n)]), Some(State::Working));
        assert_eq!(
            run(&[(Start, n), (Prompt, n), (Notify, Kind::Permission)]),
            Some(State::Waiting(Wait::Permission))
        );
        assert_eq!(
            run(&[
                (Start, n),
                (Prompt, n),
                (Notify, Kind::Permission),
                (Tool, n)
            ]),
            Some(State::Working)
        );
        assert_eq!(
            run(&[(Start, n), (Prompt, n), (Stop, n)]),
            Some(State::Waiting(Wait::YourMove))
        );
        assert_eq!(
            run(&[(Start, n), (Prompt, n), (Stop, n), (Notify, Kind::Idle)]),
            Some(State::Waiting(Wait::Idle))
        );
        assert_eq!(
            run(&[(Start, n), (Prompt, n), (Stop, n), (End, n)]),
            Some(State::Finished)
        );
    }

    /// Seen live: a hangup mid-turn is an end with no stop before it.
    #[test]
    fn an_end_in_the_middle_of_a_turn_is_interrupted() {
        use Event::*;
        let n = Kind::None;
        assert_eq!(
            run(&[(Start, n), (Prompt, n), (Tool, n), (End, n)]),
            Some(State::Interrupted)
        );
        // Ending at the prompt, before anything was asked, is not an
        // interruption.
        assert_eq!(run(&[(Start, n), (End, n)]), Some(State::Finished));
    }

    /// A permanently installed copy of the plugin fires every hook a second
    /// time. Nothing may change because of it.
    #[test]
    fn every_event_twice_changes_nothing() {
        use Event::*;
        let n = Kind::None;
        let once = [
            (Start, n),
            (Prompt, n),
            (Tool, n),
            (Stop, n),
            (Notify, Kind::Idle),
            (End, n),
        ];
        let twice: Vec<_> = once.iter().flat_map(|e| [*e, *e]).collect();
        for len in 1..=once.len() {
            assert_eq!(run(&once[..len]), run(&twice[..len * 2]), "after {len}");
        }
        let cut = [(Start, n), (Prompt, n), (End, n)];
        let cut_twice: Vec<_> = cut.iter().flat_map(|e| [*e, *e]).collect();
        assert_eq!(run(&cut_twice), Some(State::Interrupted));
    }

    /// A tool reporting after the session ended does not bring it back.
    #[test]
    fn a_late_tool_signal_does_not_revive_an_ended_session() {
        use Event::*;
        let n = Kind::None;
        assert_eq!(
            run(&[(Start, n), (Prompt, n), (Stop, n), (End, n), (Tool, n)]),
            Some(State::Finished)
        );
    }

    #[test]
    fn a_new_session_in_the_same_pane_takes_over() {
        let mut tracker = Tracker::default();
        tracker.apply(&signal(Event::Start, Kind::None));
        tracker.apply(&signal(Event::Prompt, Kind::None));
        let other = "11111111-2222-3333-4444-555555555555";
        tracker.apply(&Signal {
            session_id: other.into(),
            ..signal(Event::Stop, Kind::None)
        });
        assert_eq!(
            tracker.state,
            Some(State::Working),
            "another session's stop"
        );
        tracker.apply(&Signal {
            session_id: other.into(),
            ..signal(Event::Start, Kind::None)
        });
        assert_eq!(tracker.session_id.as_deref(), Some(other));
        assert_eq!(tracker.state, Some(State::Waiting(Wait::YourMove)));
    }

    #[test]
    fn installing_writes_the_plugin_and_the_shim_once() {
        let dir = tempfile::tempdir().expect("a temp dir");
        install(dir.path()).expect("installed");
        for file in [
            "claude-plugin/.claude-plugin/plugin.json",
            "claude-plugin/hooks/hooks.json",
            "claude-plugin/hook.sh",
            "bin/claude",
            "shell/zsh/.zshenv",
            "shell/zsh/.zprofile",
            "shell/zsh/.zshrc",
            "shell/zsh/.zlogin",
            "shell/bash/berth.bash",
        ] {
            assert!(dir.path().join(file).is_file(), "{file}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("bin/claude"))
                .expect("shim")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "the shim is executable");
        }
        let before = std::fs::metadata(dir.path().join("claude-plugin/hook.sh"))
            .and_then(|m| m.modified())
            .expect("hook");
        install(dir.path()).expect("installed again");
        let after = std::fs::metadata(dir.path().join("claude-plugin/hook.sh"))
            .and_then(|m| m.modified())
            .expect("hook");
        assert_eq!(before, after, "an unchanged file is not rewritten");
    }

    #[test]
    fn the_shim_goes_first_in_path_once() {
        let pane = PaneEnv {
            shim_dir: Some(PathBuf::from("/data/berth/bin")),
            ..PaneEnv::new(None)
        };
        let path = pane
            .path_with_shim(Some("/usr/bin:/data/berth/bin:/bin".into()))
            .expect("joined");
        assert_eq!(
            path,
            std::ffi::OsString::from("/data/berth/bin:/usr/bin:/bin")
        );
        let without = PaneEnv {
            shim_dir: None,
            ..pane
        };
        assert_eq!(without.path_with_shim(Some("/usr/bin".into())), None);
    }

    #[test]
    fn only_a_strict_uuid_becomes_a_resume_line() {
        assert_eq!(
            resume_line(ID).as_deref(),
            Some("claude --resume 48efac83-05d7-447d-b460-e5bbe01e0c7e")
        );
        for bad in [
            "",
            "x; rm -rf ~",
            "48efac83-05d7-447d-b460-e5bbe01e0c7e; ls",
            "$(id)",
        ] {
            assert_eq!(resume_line(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn only_a_strict_uuid_is_handed_to_the_shell_to_resume() {
        assert_eq!(PaneEnv::new(None).resuming(ID).resume.as_deref(), Some(ID));
        assert_eq!(PaneEnv::new(None).resuming("x; rm -rf ~").resume, None);
    }

    #[test]
    fn claude_crosses_the_wire_and_back() {
        for state in [
            State::Working,
            State::Waiting(Wait::Permission),
            State::Waiting(Wait::Idle),
            State::Waiting(Wait::YourMove),
            State::Finished,
            State::Interrupted,
        ] {
            let tracker = Tracker {
                session_id: Some(ID.into()),
                cwd: Some(PathBuf::from("/a")),
                state: Some(state),
                settled: false,
            };
            let back = from_wire(&to_wire(&tracker).unwrap());
            assert_eq!(back.state, Some(state));
            assert_eq!(back.session_id.as_deref(), Some(ID));
            assert_eq!(back.cwd, Some(PathBuf::from("/a")));
        }
        assert!(to_wire(&Tracker::default()).is_none());
    }

    /// Only a strict UUID is ever kept, from the wire as from the terminal.
    #[test]
    fn a_doubtful_session_id_from_the_wire_is_dropped() {
        let claude = berth_proto::v1::Claude {
            state: berth_proto::v1::ClaudeState::Working,
            session_id: Some("x; rm -rf ~".into()),
            cwd: None,
        };
        assert_eq!(from_wire(&claude).session_id, None);
    }
}
