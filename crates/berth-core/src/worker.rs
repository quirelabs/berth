//! One session's worker (D49, D50): a shell on a pty, and the terminal that
//! keeps its screen and history, speaking Berth's wire format.
//!
//! This is the part of step 2 that owns a session. The supervisor starts one
//! per session in a process of its own and relays the app's messages to it;
//! here it is a plain value, so everything it does is testable in-process.
//!
//! Output is forwarded as it is read, numbered by bytes ([`v1::ToApp::Output`]),
//! and fed to the worker's own terminal in the same order. A screen or a
//! history range is always taken from that terminal with every byte so far on
//! it, so a screen numbered `seq` is exactly what `seq` bytes of output drew.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use berth_proto::Packed;
use berth_proto::v1::{self, SessionId, ToApp, ToServer};
use berth_term::size::{CellSize, TermSize};
use berth_term::{Scrollback, Terminal};
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::local::Environment;
use crate::pty::{Pty, PtyEvent};

/// A worker's history: the line count Berth means, and a byte ceiling so one
/// session of pathological output cannot take the machine's memory. Both
/// stated, as D49 requires. At 10,000 lines of a 200-column screen in a
/// different colour per cell the ceiling is not reached, so the line count is
/// the one that binds (tested below). libghostty prunes a page at a time, so
/// the count lands within a page of 10,000, either side: 9,905 in that test.
pub const SCROLLBACK: Scrollback = Scrollback {
    lines: Some(10_000),
    bytes: Some(64 * 1024 * 1024),
};

/// Output handed to the terminal and not yet parsed before the worker stops
/// reading the pty, as the app's pump does.
const MAX_BACKLOG: usize = 512 * 1024;

/// While output streams, the pump gathers the pty's reads into one handoff to
/// the engine and one message, up to this much or this long after the first
/// read, whichever comes first. A pty reads about a kilobyte at a time, half
/// of them 256 bytes or less (measured on `berth-test`, D50), and every
/// handoff wakes the engine thread and publishes once.
const BATCH_AT_MOST: usize = 64 * 1024;
const BATCH_WITHIN: std::time::Duration = std::time::Duration::from_millis(2);
/// How often the pump looks for more reads while it gathers.
const BATCH_POLL: std::time::Duration = std::time::Duration::from_micros(100);

/// Messages queued for the app before the pty is no longer read. Holding the
/// shell back is right when nobody is taking its output; the supervisor
/// drains this as fast as the socket takes it.
const QUEUE: usize = 256;

/// The history lines a single [`v1::ToApp::History`] carries at most, so one
/// range never holds the terminal's thread for long.
pub const HISTORY_RANGE: u32 = 500;

/// What to start.
#[derive(Debug, Clone)]
pub struct Spec {
    pub session: SessionId,
    /// What the session list calls it.
    pub name: String,
    /// Where the shell starts. Home if `None` or gone.
    pub cwd: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
    /// For Claude's hooks and the shim (D48).
    pub pane: Option<crate::claude::PaneEnv>,
    /// The user's login shell if `None`. Tests name a plain one.
    pub shell: Option<Shell>,
}

#[derive(Debug, Clone)]
pub struct Shell {
    pub program: String,
    pub login: bool,
}

/// What has a session's terminal now, and where.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Live {
    program: Option<String>,
    cwd: Option<String>,
}

pub struct Worker {
    session: SessionId,
    name: String,
    /// As last looked at; the starting directory until then.
    live: Mutex<Live>,
    /// The token Claude's hook presents, and what its signals said (D48, D50).
    token: Option<crate::claude::PaneToken>,
    claude: Mutex<crate::claude::Tracker>,
    size: Mutex<(u16, u16)>,
    pty: Pty,
    terminal: Arc<Terminal>,
    /// Bytes of output so far. Locked while a chunk is fed to the terminal
    /// and counted, and while a screen is taken, so the two always agree.
    seq: Arc<Mutex<u64>>,
    /// What the session's programs say they are doing (OSC 7501, D53), kept
    /// here so it outlives any app.
    status: Mutex<Status>,
    /// Told whenever the terminal has published something new, which is
    /// when there may be status reports to take.
    status_wake: Arc<tokio::sync::Notify>,
}

/// A session's program status, and what is still to be told about it.
#[derive(Default)]
struct Status {
    records: crate::status::Records,
    /// Changes made outside [`Worker::status_changes`], waiting to be sent.
    unsent: Vec<v1::Event>,
    /// What had the terminal when last looked at, to see a program end.
    foreground: Option<u32>,
}

fn lock(seq: &Mutex<u64>) -> MutexGuard<'_, u64> {
    seq.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Worker {
    /// Start the shell. Messages for the app arrive on the receiver until the
    /// shell has exited and its last output is through.
    pub async fn start(spec: Spec) -> Result<(Worker, mpsc::Receiver<ToApp>)> {
        let (program, login) = match &spec.shell {
            Some(shell) => (shell.program.clone(), shell.login),
            None => (crate::local::login_shell(), true),
        };
        let cwd = spec.cwd.as_deref().filter(|dir| dir.is_dir());
        let (pty, events) = crate::local::open_shell(
            &program,
            login,
            cwd,
            spec.pane.as_ref(),
            spec.cols,
            spec.rows,
            Environment::Clean,
        )
        .await?;
        // Nobody draws a worker's terminal: it parses, and builds a screen or
        // history only when one is asked for. It is the session's own, so it
        // is the one terminal that answers what programs ask (device
        // attributes, the cursor position, XTVERSION, the OSC 7501 query):
        // the app's copy never does, or programs would get every answer
        // twice (D50). Never blocks the engine: a reply that finds the pty's
        // queue full is dropped, as a real terminal's would be on a hung tty.
        let to_pty = pty.input.clone();
        let terminal = Arc::new(Terminal::server(
            TermSize::new(spec.cols, spec.rows),
            SCROLLBACK,
            Box::new(move |bytes| {
                if to_pty
                    .try_send(crate::pty::PtyInput::Data(bytes.to_vec()))
                    .is_err()
                {
                    tracing::warn!("a terminal reply did not fit the pty's queue");
                }
            }),
        ));
        let status_wake = Arc::new(tokio::sync::Notify::new());
        let wake = status_wake.clone();
        terminal.on_publish(move || wake.notify_one());
        let seq = Arc::new(Mutex::new(0));
        let (tx, rx) = mpsc::channel(QUEUE);
        let pumped = (spec.session, terminal.clone(), seq.clone());
        std::thread::Builder::new()
            .name("berth-worker".into())
            .spawn(move || pump(pumped.0, pumped.1, pumped.2, events, tx))
            .map_err(|e| Error::Pty(format!("could not start the worker's reader: {e}")))?;
        Ok((
            Worker {
                session: spec.session,
                name: spec.name.clone(),
                live: Mutex::new(Live {
                    program: None,
                    cwd: spec
                        .cwd
                        .as_ref()
                        .map(|dir| dir.to_string_lossy().to_string()),
                }),
                size: Mutex::new((spec.cols, spec.rows)),
                token: spec.pane.as_ref().map(|p| p.token.clone()),
                claude: Mutex::new(crate::claude::Tracker::default()),
                pty,
                terminal,
                seq,
                status: Mutex::new(Status::default()),
                status_wake,
            },
            rx,
        ))
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    /// The shell's process, where the platform says.
    pub fn pid(&self) -> Option<u32> {
        self.pty.local_pid()
    }

    /// Act on a message for this session. Returns the reply, if it has one.
    /// Messages about the session list belong to the supervisor, and are
    /// refused here by name.
    pub async fn handle(&self, message: ToServer) -> Option<ToApp> {
        match message {
            ToServer::Input { bytes, .. } => {
                self.pty.write(bytes).await;
                None
            }
            ToServer::Resize { cols, rows, .. } => {
                self.resize(cols, rows).await;
                None
            }
            // The size only: the host sends the screen and its history,
            // off the async threads (see `worker_host`).
            ToServer::Attach { cols, rows, .. } => {
                self.resize(cols, rows).await;
                None
            }
            ToServer::Kill { .. } => {
                self.pty.close().await;
                None
            }
            // Nothing to do: output keeps flowing whether anyone reads it.
            ToServer::Detach { .. }
            | ToServer::Confirmed { .. }
            | ToServer::Visible { .. }
            | ToServer::SendHistory { .. } => None,
            // Asked by a supervisor finding its workers again: this session.
            ToServer::ListSessions => Some(ToApp::Sessions(vec![self.info()])),
            ToServer::Create { .. } => Some(ToApp::Refused {
                what: format!("{message:?}"),
                why: "a worker holds one session; ask the supervisor".into(),
            }),
        }
    }

    /// Wakes whoever sends this session's events when there may be program
    /// status changes to send ([`Worker::status_changes`]).
    pub fn status_wake(&self) -> Arc<tokio::sync::Notify> {
        self.status_wake.clone()
    }

    /// Take what the session's programs reported since the last call (OSC
    /// 7501), apply it to the records, and return what changed as events:
    /// each record set or replaced, and the ids of those gone. A burst of
    /// reports to one record comes out as its last state only.
    ///
    /// Display only (D53): the caller sends these on and does nothing else
    /// with them.
    pub fn status_changes(&self) -> Vec<v1::Event> {
        let reported = self.terminal.drain_status_events();
        let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
        let mut events = std::mem::take(&mut status.unsent);
        if reported.is_empty() {
            return events;
        }
        let before = status.records.to_wire();
        for event in reported {
            match event {
                berth_term::StatusEvent::Report(pairs) => {
                    if let Some(report) = crate::status::parse(&pairs) {
                        status.records.apply(report);
                    }
                }
                berth_term::StatusEvent::PromptStart => {
                    status.records.program_ended();
                }
                berth_term::StatusEvent::Reset => {
                    status.records.reset();
                }
            }
        }
        events.extend(changes(&before, &status.records.to_wire()));
        events
    }

    /// Every program status record the session holds now, for an app that
    /// was not here when they were reported.
    pub fn statuses(&self) -> Vec<v1::Status> {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .records
            .to_wire()
    }

    /// What Claude is doing in this session, as its hook last said.
    pub fn claude(&self) -> Option<v1::Claude> {
        crate::claude::to_wire(&self.claude.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// A line from Claude's hook: believed if it carries this session's
    /// token, as the app believed it from the terminal (D48). Returns the new
    /// state when it changed.
    pub fn hook_line(&self, line: &str) -> Option<v1::Claude> {
        let token = self.token.as_ref()?;
        let payload = line.trim().strip_prefix("7717;berth;")?;
        let signal = crate::claude::parse(payload, token)?;
        let mut tracker = self.claude.lock().unwrap_or_else(|p| p.into_inner());
        let before = tracker.clone();
        tracker.apply(&signal);
        (*tracker != before)
            .then(|| crate::claude::to_wire(&tracker))
            .flatten()
    }

    /// Whether a full-screen program has the terminal, as of the screen last
    /// published. Never waits.
    pub fn alternate_screen(&self) -> bool {
        self.terminal.latest().modes.alternate_screen
    }

    /// This session as the session list shows it.
    pub fn info(&self) -> v1::SessionInfo {
        let (cols, rows) = *self.size.lock().unwrap_or_else(|p| p.into_inner());
        let live = self.live.lock().unwrap_or_else(|p| p.into_inner()).clone();
        v1::SessionInfo {
            id: self.session,
            name: self.name.clone(),
            cwd: live.cwd,
            program: live.program,
            cols,
            rows,
            claude: self.claude(),
            pid: self.pty.local_pid(),
            status: crate::status::foremost(&self.statuses()).cloned(),
        }
    }

    /// Look again at what has the terminal and where. Returns the session's
    /// description if either changed, for the session list. Blocking: it
    /// asks `ps` and `lsof`.
    pub fn refresh(&self) -> Option<v1::SessionInfo> {
        let pid = self.pty.foreground_pid()?;
        self.note_foreground(pid);
        let now = Live {
            program: crate::local::program_of(pid),
            cwd: crate::local::cwd_of(pid).map(|d| d.to_string_lossy().to_string()),
        };
        let mut live = self.live.lock().unwrap_or_else(|p| p.into_inner());
        // Something the machine would not say is not a change.
        let merged = Live {
            program: now.program.or_else(|| live.program.clone()),
            cwd: now.cwd.or_else(|| live.cwd.clone()),
        };
        if merged == *live {
            return None;
        }
        *live = merged;
        drop(live);
        Some(self.info())
    }

    /// The terminal going back to the shell means the program that had it
    /// exited: its records of something under way go, as the protocol asks,
    /// also for a shell that marks no prompts.
    fn note_foreground(&self, pid: u32) {
        let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
        let before = status.foreground.replace(pid);
        let back_at_the_shell =
            before.is_some_and(|was| was != pid) && self.pty.local_pid() == Some(pid);
        if !back_at_the_shell {
            return;
        }
        let records = status.records.to_wire();
        if status.records.program_ended() {
            let gone = changes(&records, &status.records.to_wire());
            status.unsent.extend(gone);
            drop(status);
            self.status_wake.notify_one();
        }
    }

    async fn resize(&self, cols: u16, rows: u16) {
        let size = TermSize::new(cols, rows);
        *self.size.lock().unwrap_or_else(|p| p.into_inner()) = (size.cols, size.rows);
        self.terminal.resize(size, CellSize::new(8.0, 16.0));
        self.pty.resize(size.cols, size.rows).await;
    }

    /// The screen as of the output so far, packed, and the number of its
    /// first row, below which [`Worker::history_below`] reaches.
    ///
    /// Output chunks at or below its `seq` may still be on their way to the
    /// app behind it; the app skips them, as they are already on this screen.
    pub fn screen(&self, history_follows: bool) -> (ToApp, Option<u64>) {
        let seq = lock(&self.seq);
        let screen = self.terminal.wire_screen();
        let at = *seq;
        drop(seq);
        let first_line = screen.as_ref().map(|s| s.first_line);
        let message = match screen.map(|s| Packed::pack(&s)) {
            Some(Ok(screen)) => ToApp::Screen {
                session: self.session,
                seq: at,
                screen,
                history_follows,
            },
            Some(Err(error)) => self.refused("screen", error.to_string()),
            None => self.refused("screen", "the terminal did not answer".into()),
        };
        (message, first_line)
    }

    /// Every history line above a screen whose first row is `first_line`,
    /// newest first, in ranges of [`HISTORY_RANGE`], the last marked
    /// complete. Lines below a screen never change, only the oldest are
    /// pruned, so this needs no lock against the output: a line pruned
    /// meanwhile is simply not sent.
    pub fn history_below(&self, first_line: u64) -> Vec<ToApp> {
        let mut out = Vec::new();
        let mut before = first_line;
        loop {
            let history = self.terminal.wire_history(Some(before), HISTORY_RANGE);
            let complete = history.reaches_oldest || history.lines.is_empty();
            let newest = history.newest;
            let count = history.lines.len() as u64;
            match Packed::pack(&history.lines) {
                Ok(lines) => out.push(ToApp::History {
                    session: self.session,
                    newest,
                    lines,
                    complete,
                }),
                Err(error) => {
                    out.push(self.refused("history", error.to_string()));
                    break;
                }
            }
            if complete {
                break;
            }
            before = newest + 1 - count;
        }
        out
    }

    fn refused(&self, what: &str, why: String) -> ToApp {
        ToApp::Refused {
            what: format!("{what} of session {}", self.session),
            why,
        }
    }
}

/// What turned `before` into `after`, as events: records set or replaced,
/// then the ids of records gone.
fn changes(before: &[v1::Status], after: &[v1::Status]) -> Vec<v1::Event> {
    let mut events: Vec<v1::Event> = after
        .iter()
        .filter(|record| !before.contains(record))
        .cloned()
        .map(v1::Event::Status)
        .collect();
    let gone: Vec<String> = before
        .iter()
        .filter(|was| !after.iter().any(|now| now.id == was.id))
        .map(|was| was.id.clone())
        .collect();
    if !gone.is_empty() {
        events.push(v1::Event::StatusGone(gone));
    }
    events
}

/// Read the pty until the shell is gone: feed each chunk to the terminal,
/// count it, and forward it.
fn pump(
    session: SessionId,
    terminal: Arc<Terminal>,
    seq: Arc<Mutex<u64>>,
    mut events: mpsc::Receiver<PtyEvent>,
    tx: mpsc::Sender<ToApp>,
) {
    let mut last_handoff: Option<std::time::Instant> = None;
    let mut pending: Option<PtyEvent> = None;
    while let Some(event) = pending.take().or_else(|| events.blocking_recv()) {
        let message = match event {
            PtyEvent::Output(first) => {
                // Streaming if the last handoff was moments ago: then gather.
                // A keystroke's echo after a pause goes on at once.
                let streaming = last_handoff.is_some_and(|at| at.elapsed() < BATCH_WITHIN);
                let (bytes, after) = gather(first, &mut events, streaming);
                pending = after;
                let mut total = lock(&seq);
                terminal.advance(&bytes);
                *total += bytes.len() as u64;
                let at = *total;
                drop(total);
                terminal.wait_for_room(MAX_BACKLOG);
                last_handoff = Some(std::time::Instant::now());
                ToApp::Output {
                    session,
                    seq: at,
                    bytes,
                }
            }
            PtyEvent::Exit(code) => ToApp::Event {
                session,
                event: v1::Event::Exited {
                    code: i32::try_from(code).ok(),
                },
            },
            PtyEvent::Closed => break,
            PtyEvent::Rebuild(_) => continue,
        };
        if tx.blocking_send(message).is_err() {
            // Nobody is listening any more. The shell keeps running only as
            // long as someone holds the worker.
            break;
        }
    }
}

/// The reads already waiting after `first`, and while `streaming` those that
/// come within [`BATCH_WITHIN`], up to [`BATCH_AT_MOST`]. Anything else that
/// arrives meanwhile, such as the exit, is handed back to go next.
fn gather(
    first: Vec<u8>,
    events: &mut mpsc::Receiver<PtyEvent>,
    streaming: bool,
) -> (Vec<u8>, Option<PtyEvent>) {
    let started = std::time::Instant::now();
    let mut bytes = first;
    while bytes.len() < BATCH_AT_MOST {
        match events.try_recv() {
            Ok(PtyEvent::Output(more)) => bytes.extend_from_slice(&more),
            Ok(other) => return (bytes, Some(other)),
            Err(mpsc::error::TryRecvError::Empty)
                if streaming && started.elapsed() < BATCH_WITHIN =>
            {
                std::thread::sleep(BATCH_POLL);
            }
            Err(_) => break,
        }
    }
    (bytes, None)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    fn spec(dir: Option<PathBuf>) -> Spec {
        Spec {
            session: 1,
            name: "test".into(),
            cwd: dir,
            cols: 40,
            rows: 10,
            pane: None,
            shell: Some(Shell {
                program: "/bin/sh".into(),
                login: false,
            }),
        }
    }

    /// Everything the worker has said until `needle` appears in its output.
    async fn until(rx: &mut mpsc::Receiver<ToApp>, needle: &str) -> Vec<ToApp> {
        let mut seen = Vec::new();
        let mut text = Vec::new();
        let found = timeout(Duration::from_secs(10), async {
            while let Some(message) = rx.recv().await {
                if let ToApp::Output { bytes, .. } = &message {
                    text.extend_from_slice(bytes);
                }
                seen.push(message);
                if String::from_utf8_lossy(&text).contains(needle) {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(
            found,
            Ok(true),
            "{needle:?} never came: {:?}",
            String::from_utf8_lossy(&text)
        );
        seen
    }

    fn screen_text(message: &ToApp) -> (u64, String) {
        let ToApp::Screen { seq, screen, .. } = message else {
            panic!("not a screen: {message:?}");
        };
        let screen: v1::Screen = screen.unpack().unwrap();
        let text = screen
            .lines
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|c| c.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (*seq, text)
    }

    #[test]
    fn gathering_takes_what_is_waiting_and_hands_back_the_rest() {
        let (tx, mut rx) = mpsc::channel(16);
        for chunk in [b"b".to_vec(), b"c".to_vec()] {
            tx.try_send(PtyEvent::Output(chunk)).unwrap();
        }
        tx.try_send(PtyEvent::Exit(0)).unwrap();
        tx.try_send(PtyEvent::Output(b"d".to_vec())).unwrap();
        let (bytes, after) = gather(b"a".to_vec(), &mut rx, false);
        assert_eq!(bytes, b"abc");
        assert_eq!(after, Some(PtyEvent::Exit(0)));
    }

    /// A keystroke's echo after a pause is not held back waiting for more.
    #[test]
    fn not_streaming_gathering_never_waits() {
        let (_tx, mut rx) = mpsc::channel::<PtyEvent>(16);
        let started = std::time::Instant::now();
        let (bytes, after) = gather(b"x".to_vec(), &mut rx, false);
        assert_eq!((bytes, after), (b"x".to_vec(), None));
        assert!(started.elapsed() < BATCH_WITHIN);
    }

    /// Streaming, reads that come within the batch's time join it, up to
    /// its size.
    #[test]
    fn streaming_gathering_waits_for_more_up_to_the_cap() {
        let (tx, mut rx) = mpsc::channel(64);
        std::thread::spawn(move || {
            for _ in 0..40 {
                std::thread::sleep(std::time::Duration::from_micros(20));
                if tx
                    .blocking_send(PtyEvent::Output(vec![b'x'; 4096]))
                    .is_err()
                {
                    break;
                }
            }
        });
        let (bytes, _) = gather(vec![b'x'; 4096], &mut rx, true);
        assert!(bytes.len() > 4096, "nothing joined: {}", bytes.len());
        assert!(bytes.len() <= BATCH_AT_MOST + 4096, "{}", bytes.len());
    }

    #[tokio::test]
    async fn output_is_numbered_by_bytes_and_the_screen_agrees() {
        let (worker, mut rx) = Worker::start(spec(None)).await.unwrap();
        worker
            .handle(ToServer::Input {
                session: 1,
                bytes: b"echo berth_$((6*7))\n".to_vec(),
            })
            .await;
        let seen = until(&mut rx, "berth_42").await;
        let mut total = 0;
        for message in &seen {
            if let ToApp::Output { seq, bytes, .. } = message {
                total += bytes.len() as u64;
                assert_eq!(*seq, total, "seq counts bytes");
            }
        }
        let (seq, text) = screen_text(&worker.screen(false).0);
        assert!(seq >= total);
        assert!(text.contains("berth_42"), "{text}");
    }

    /// Nothing from whatever started the worker leaks into the shell, and
    /// Berth's own terminal identity is set (D51).
    /// A program that asks for device attributes and blocks until they come,
    /// as programs do at startup to learn what the terminal is: the worker
    /// answers, so it does not sit out a deadline.
    #[tokio::test]
    async fn a_program_blocked_on_device_attributes_gets_its_answer() {
        let (worker, mut rx) = Worker::start(spec(None)).await.unwrap();
        // Raw and silent, ask, read the nine bytes of the answer, show them
        // without the escape.
        let script = "stty raw -echo; printf '\\033[c'; r=$(dd bs=1 count=9 2>/dev/null); \
                      stty sane; echo \"got_$(printf '%s' \"$r\" | tr -d '\\033')\"\n";
        worker
            .handle(ToServer::Input {
                session: 1,
                bytes: script.as_bytes().to_vec(),
            })
            .await;
        until(&mut rx, "got_[?62;22c").await;
    }

    /// The OSC 7501 query, then device attributes: the 7501 answer reaches
    /// the program first, so it reads the terminal as supporting it.
    #[tokio::test]
    async fn the_status_query_is_answered_first_on_a_real_pty() {
        let (worker, mut rx) = Worker::start(spec(None)).await.unwrap();
        let script = "stty raw -echo; printf '\\033]7501;?\\033\\\\\\033[c'; \
                      r=$(dd bs=1 count=19 2>/dev/null); stty sane; \
                      echo \"got_$(printf '%s' \"$r\" | tr -d '\\033')\"\n";
        worker
            .handle(ToServer::Input {
                session: 1,
                bytes: script.as_bytes().to_vec(),
            })
            .await;
        until(&mut rx, "got_]7501;?\\[?62;22c").await;
    }

    /// Status changes until `wanted` of them have come, or time runs out.
    async fn status_events(worker: &Worker, wanted: usize) -> Vec<v1::Event> {
        let wake = worker.status_wake();
        let mut events = Vec::new();
        let _ = timeout(Duration::from_secs(10), async {
            while events.len() < wanted {
                events.extend(worker.status_changes());
                if events.len() < wanted {
                    wake.notified().await;
                }
            }
        })
        .await;
        events
    }

    async fn type_in(worker: &Worker, line: &str) {
        worker
            .handle(ToServer::Input {
                session: 1,
                bytes: format!("{line}\n").into_bytes(),
            })
            .await;
    }

    /// A program reports what it is doing (OSC 7501): the worker keeps the
    /// record, shows the session by it, and drops it at the next prompt
    /// unless it is something left to see.
    #[tokio::test]
    async fn program_status_is_kept_and_dropped_as_the_protocol_says() {
        let (worker, _rx) = Worker::start(spec(None)).await.unwrap();
        // "Building" in base64.
        type_in(
            &worker,
            "printf '\\033]7501;state=working:app=make:progress=40:msg=QnVpbGRpbmc=\\033\\\\'",
        )
        .await;
        let events = status_events(&worker, 1).await;
        let expected = v1::Status {
            id: String::new(),
            state: v1::StatusState::Working,
            kind: None,
            progress: Some(40),
            app: Some("make".into()),
            title: None,
            msg: Some("Building".into()),
        };
        assert_eq!(events, [v1::Event::Status(expected.clone())]);
        assert_eq!(worker.info().status, Some(expected));

        // A finished record beside it, then a new prompt: the working one
        // goes, the finished one stays to be seen.
        type_in(
            &worker,
            "printf '\\033]7501;state=done:id=tests\\033\\\\\\033]133;A\\007'",
        )
        .await;
        let events = status_events(&worker, 2).await;
        assert!(
            events.contains(&v1::Event::StatusGone(vec![String::new()])),
            "{events:?}"
        );
        let left = worker.statuses();
        assert_eq!(left.len(), 1, "{left:?}");
        assert_eq!(
            (left[0].id.as_str(), left[0].state),
            ("tests", v1::StatusState::Done)
        );

        // A full reset clears the rest.
        type_in(&worker, "printf '\\033c'").await;
        let events = status_events(&worker, 1).await;
        assert_eq!(events, [v1::Event::StatusGone(vec!["tests".into()])]);
        assert_eq!(worker.info().status, None);
    }

    /// A report that breaks the protocol's rules changes nothing.
    #[tokio::test]
    async fn a_bad_status_report_is_ignored() {
        let (worker, _rx) = Worker::start(spec(None)).await.unwrap();
        // A control character in the message ("a\nb"), then a good report.
        type_in(
            &worker,
            "printf '\\033]7501;state=working:msg=YQpi\\033\\\\\\033]7501;state=idle:id=ok\\007'",
        )
        .await;
        let events = status_events(&worker, 1).await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(worker.statuses().len(), 1);
        assert_eq!(worker.statuses()[0].id, "ok");
    }

    #[tokio::test]
    async fn the_shell_gets_a_clean_environment() {
        // SAFETY: set before the shell is started, and read only by it.
        unsafe { std::env::set_var("WORKER_LEAK_TEST_9F3A", "leaked") };
        let (worker, mut rx) = Worker::start(spec(None)).await.unwrap();
        worker
            .handle(ToServer::Input {
                session: 1,
                bytes: b"echo \"[$TERM|$COLORTERM|$TERM_PROGRAM|${WORKER_LEAK_TEST_9F3A:-clean}|${HOME:+home}]\"\n"
                    .to_vec(),
            })
            .await;
        until(&mut rx, "[xterm-256color|truecolor|Berth|clean|home]").await;
    }

    #[tokio::test]
    async fn the_shell_starts_where_it_was_asked() {
        let dir = tempfile::tempdir().unwrap();
        let (worker, mut rx) = Worker::start(spec(Some(dir.path().to_path_buf())))
            .await
            .unwrap();
        worker
            .handle(ToServer::Input {
                session: 1,
                bytes: b"pwd\n".to_vec(),
            })
            .await;
        let name = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        until(&mut rx, &name).await;
    }

    #[tokio::test]
    async fn attaching_resizes_the_session() {
        let (worker, mut rx) = Worker::start(spec(None)).await.unwrap();
        worker
            .handle(ToServer::Input {
                session: 1,
                bytes: b"stty size\n".to_vec(),
            })
            .await;
        until(&mut rx, "10 40").await;
        let reply = worker
            .handle(ToServer::Attach {
                session: 1,
                cols: 60,
                rows: 12,
            })
            .await;
        assert!(
            reply.is_none(),
            "the host sends the screen, off the async threads"
        );
        let ToApp::Screen { screen, .. } = &worker.screen(true).0 else {
            panic!("not a screen");
        };
        let screen: v1::Screen = screen.unpack().unwrap();
        assert_eq!((screen.cols, screen.rows), (60, 12));
        worker
            .handle(ToServer::Input {
                session: 1,
                bytes: b"stty size\n".to_vec(),
            })
            .await;
        until(&mut rx, "12 60").await;
    }

    #[tokio::test]
    async fn history_arrives_in_numbered_ranges() {
        let (worker, mut rx) = Worker::start(spec(None)).await.unwrap();
        worker
            .handle(ToServer::Input {
                session: 1,
                bytes:
                    b"i=0; while [ $i -lt 40 ]; do echo row$i; i=$((i+1)); done; echo do\"\"ne_\n"
                        .to_vec(),
            })
            .await;
        // Quoted apart so the shell's echo of the command does not match.
        until(&mut rx, "done_").await;
        let (_, first_line) = worker.screen(true);
        let ranges = worker.history_below(first_line.unwrap());
        let Some(ToApp::History {
            newest,
            lines,
            complete,
            ..
        }) = ranges.last()
        else {
            panic!("no history");
        };
        assert!(*complete, "the last range is marked complete");
        let rows: Vec<v1::Row> = lines.unpack().unwrap();
        assert!(!rows.is_empty());
        assert!(
            *newest < first_line.unwrap(),
            "history lies above the screen"
        );
        let text: String = rows
            .iter()
            .flat_map(|r| r.cells.iter().map(|c| c.text.as_str()))
            .collect();
        assert!(text.contains("row0"), "{text}");
    }

    /// The session list names a session by what has its terminal and where,
    /// so the description follows the shell into a program and back.
    #[tokio::test]
    async fn the_description_follows_what_has_the_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let (worker, mut rx) = Worker::start(spec(Some(dir.path().to_path_buf())))
            .await
            .unwrap();
        let worker = std::sync::Arc::new(worker);
        let program = |worker: &std::sync::Arc<Worker>| {
            let worker = worker.clone();
            async move {
                for _ in 0..50 {
                    let w = worker.clone();
                    let _ = tokio::task::spawn_blocking(move || w.refresh()).await;
                    let info = worker.info();
                    if info.program.is_some() {
                        return info;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                worker.info()
            }
        };
        let at_prompt = program(&worker).await;
        assert_eq!(at_prompt.program.as_deref(), Some("sh"));
        let real = dir.path().canonicalize().unwrap();
        assert_eq!(at_prompt.cwd.as_deref(), real.to_str());

        worker
            .handle(ToServer::Input {
                session: 1,
                bytes: b"sleep 5\n".to_vec(),
            })
            .await;
        until(&mut rx, "sleep 5").await;
        let mut running = None;
        for _ in 0..50 {
            let w = worker.clone();
            if let Ok(Some(info)) = tokio::task::spawn_blocking(move || w.refresh()).await
                && info.program.as_deref() == Some("sleep")
            {
                running = Some(info);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(running.is_some(), "never saw sleep take the terminal");
    }

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    const UUID: &str = "0b9c5f5e-7b39-4a55-8a53-2b8f0d1c9e11";

    fn with_token() -> Spec {
        let mut pane = crate::claude::PaneEnv::new(None);
        pane.token = crate::claude::PaneToken::parse(TOKEN).unwrap();
        Spec {
            pane: Some(pane),
            ..spec(None)
        }
    }

    /// The hook's line, as it would have reached the terminal (D48), is
    /// believed by the worker on the same terms (D50).
    #[tokio::test]
    async fn a_hook_line_with_the_token_tells_the_worker_what_claude_does() {
        let (worker, _rx) = Worker::start(with_token()).await.unwrap();
        let line = format!("7717;berth;1;{TOKEN};prompt;{UUID};-;L3RtcA");
        let claude = worker.hook_line(&line).expect("believed");
        assert_eq!(claude.state, v1::ClaudeState::Working);
        assert_eq!(claude.session_id.as_deref(), Some(UUID));
        assert_eq!(claude.cwd.as_deref(), Some("/tmp"));
        assert_eq!(worker.info().claude, Some(claude));
        assert!(
            worker.hook_line(&line).is_none(),
            "the same again is no change"
        );
    }

    #[tokio::test]
    async fn a_hook_line_without_the_token_is_not_believed() {
        let (worker, _rx) = Worker::start(with_token()).await.unwrap();
        let wrong = "ffffffffffffffffffffffffffffffff";
        assert!(
            worker
                .hook_line(&format!("7717;berth;1;{wrong};prompt;{UUID};-;L3RtcA"))
                .is_none()
        );
        assert!(
            worker
                .hook_line(&format!("1;{TOKEN};prompt;{UUID};-;L3RtcA"))
                .is_none(),
            "only the hook's own line"
        );
        assert_eq!(worker.info().claude, None);
    }

    #[tokio::test]
    async fn the_exit_is_reported_and_the_stream_ends() {
        let (worker, mut rx) = Worker::start(spec(None)).await.unwrap();
        worker
            .handle(ToServer::Input {
                session: 1,
                bytes: b"exit 3\n".to_vec(),
            })
            .await;
        let exited = timeout(Duration::from_secs(10), async {
            let mut code = None;
            while let Some(message) = rx.recv().await {
                if let ToApp::Event {
                    event: v1::Event::Exited { code: c },
                    ..
                } = message
                {
                    code = c;
                }
            }
            code
        })
        .await;
        assert_eq!(exited, Ok(Some(3)));
    }

    #[tokio::test]
    async fn a_worker_describes_itself_and_refuses_to_create() {
        let (worker, _rx) = Worker::start(spec(None)).await.unwrap();
        let Some(ToApp::Sessions(list)) = worker.handle(ToServer::ListSessions).await else {
            panic!("no session list");
        };
        assert_eq!(list.len(), 1);
        assert_eq!((list[0].id, list[0].name.as_str()), (1, "test"));
        assert!(matches!(
            worker
                .handle(ToServer::Create {
                    name: None,
                    cwd: None,
                    cols: 80,
                    rows: 24,
                    pane: None,
                    resume: None,
                })
                .await,
            Some(ToApp::Refused { .. })
        ));
    }

    /// Both limits are set, and at a wide terminal full of colour the byte
    /// ceiling holds back nothing the line count would keep.
    #[test]
    fn the_line_limit_binds_before_the_byte_ceiling() {
        let held = |scrollback| {
            let terminal = Terminal::with_scrollback(
                TermSize::new(200, 50),
                CellSize::new(8.0, 16.0),
                scrollback,
            );
            let line: String = (0..200)
                .map(|i| format!("\x1b[38;5;{}m\u{2588}", i % 256))
                .collect();
            let mut out = String::new();
            for _ in 0..14_000 {
                out.push_str(&line);
                out.push_str("\x1b[0m\r\n");
            }
            terminal.advance(out.as_bytes());
            terminal.history_held()
        };
        let lines_only = held(Scrollback {
            bytes: None,
            ..SCROLLBACK
        });
        assert_eq!(held(SCROLLBACK), lines_only);
        assert!(lines_only > 9_500, "{lines_only}");
    }
}
