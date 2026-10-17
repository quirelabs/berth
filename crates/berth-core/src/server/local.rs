//! A server's sessions as the app sees them: the supervisor's sessions, each
//! looking like an ordinary pty to the view and the output pump (D50). The
//! supervisor is this machine's, on its local socket, or a host's, through
//! its relay on an ssh channel (D55): one handle, [`Server`], over either,
//! so a host's sessions, status, title and directory arrive as local ones
//! do.
//!
//! One connection to the supervisor carries every session; a router hands
//! each session its own messages. A screen from the worker is replayed into
//! the app's terminal as the escape sequences that draw it, and output that
//! screen already includes is skipped. Closing a pane detaches: the session
//! keeps running. If the connection drops, the router connects again,
//! starting the supervisor if it has to, and re-attaches every open session,
//! so a crashed supervisor costs the tabs nothing.
//!
//! The router never waits on a pane. A pane still parsing a flood that lets
//! its queue fill is marked behind; its messages are dropped until it has
//! emptied the queue, and then it attaches again for the screen as it is now.
//! Heavy output so turns into screens rather than holding up the connection,
//! which would hold up every other local tab. The sync piece refines what is
//! sent to a client that falls behind; this is the app's own half of it.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use berth_proto::v1::{self, SessionId, SessionInfo, ToApp, ToServer};
use tokio::sync::{mpsc, oneshot};

use super::client;
use super::link;
use super::paths::{self, Places};
use crate::error::{Error, Result};
use crate::pty::{INPUT_QUEUE_SIZE, OUTPUT_QUEUE_SIZE, Pty, PtyEvent, PtyInput};

/// How long to wait for a supervisor that was just started to answer.
const START_WITHIN: Duration = Duration::from_secs(10);
/// How often a lost connection is retried.
const RETRY_EVERY: Duration = Duration::from_millis(250);
/// How long a request waits for its answer.
const ANSWER_WITHIN: Duration = Duration::from_secs(15);

/// How the supervisor is brought up when nothing answers.
#[derive(Clone)]
pub enum Start {
    /// Start `berth --supervisor` from the versioned copy of this binary (D50).
    Binary(PathBuf),
    /// Someone else starts it (tests). Only reconnect.
    External,
}

/// A fresh stream to a supervisor, each time one is asked for: what a
/// connection, and every reconnection, rides on.
pub type Opener = Arc<
    dyn Fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Box<dyn client::Duplex>>> + Send>,
        > + Send
        + Sync,
>;

/// How the supervisor is reached.
#[derive(Clone)]
enum Dial {
    /// This machine's, on its local socket, started if need be.
    Local { places: Places, start: Start },
    /// Any other, over whatever stream `Opener` gives: a host's relay.
    Over(Opener),
}

/// What a request is waiting for.
enum Waiting {
    Created(oneshot::Sender<std::result::Result<SessionId, String>>),
    Listed(oneshot::Sender<Vec<SessionInfo>>),
}

/// An attached session: where its messages go, and its size, to attach again
/// at after a reconnect.
struct Route {
    tx: mpsc::Sender<ToApp>,
    cols: u16,
    rows: u16,
    /// Messages dropped since the queue filled; cleared by the next screen.
    behind: bool,
}

/// Told what Claude is doing in a session, as its worker reports it (D50).
type ClaudeListener = Arc<dyn Fn(SessionId, v1::Claude) + Send + Sync>;
/// Told the session list whenever the supervisor says it changed.
type SessionsListener = Arc<dyn Fn(Vec<SessionInfo>) + Send + Sync>;

#[derive(Default)]
struct Shared {
    routes: HashMap<SessionId, Route>,
    waiting: VecDeque<Waiting>,
    to_server: Option<mpsc::Sender<ToServer>>,
    /// Set while the connection is down because the supervisor, another
    /// build, said something this one could not read (D54).
    other_build: Option<link::OtherBuild>,
    claude: Option<ClaudeListener>,
    sessions: Option<SessionsListener>,
}

/// The app's handle on a server, this machine's or a host's. Cheap to
/// clone.
#[derive(Clone)]
pub struct Server {
    shared: Arc<Mutex<Shared>>,
}

/// This machine's server: the name the app has used for it since D50.
pub type LocalServer = Server;

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Connect to the supervisor, starting this machine's if nothing answers
/// and `start` allows.
async fn reach(dial: &Dial, expected: &str) -> Result<client::Client> {
    let (places, start) = match dial {
        Dial::Local { places, start } => (places, start),
        Dial::Over(open) => return client::over(open().await?, expected).await,
    };
    if let Ok(client) = client::connect_expecting(places, expected).await {
        return Ok(client);
    }
    let Start::Binary(exe) = start else {
        return Err(Error::Server("no supervisor answers".into()));
    };
    places.prepare()?;
    let copy = paths::install_copy(&places.bin, exe)?;
    super::start::ensure_running(places, &copy).await?;
    let deadline = tokio::time::Instant::now() + START_WITHIN;
    loop {
        match client::connect_expecting(places, expected).await {
            Ok(client) => return Ok(client),
            Err(error) if tokio::time::Instant::now() >= deadline => return Err(error),
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

impl Server {
    /// This machine's server: connect, starting the supervisor if needed,
    /// and keep connected.
    pub async fn start(places: Places, start: Start) -> Result<Self> {
        // The supervisor should be the build this side would start.
        let expected = match &start {
            Start::Binary(exe) => paths::build_of(exe)?,
            Start::External => super::build(),
        };
        Self::dial(Dial::Local { places, start }, expected).await
    }

    /// A server reached over whatever stream `open` gives, each time it
    /// connects: a host's, through its relay (D55). `expected` is the build
    /// installed there.
    pub async fn over(open: Opener, expected: String) -> Result<Self> {
        Self::dial(Dial::Over(open), expected).await
    }

    async fn dial(dial: Dial, expected: String) -> Result<Self> {
        let first = reach(&dial, &expected).await?;
        let server = Self {
            shared: Arc::new(Mutex::new(Shared {
                to_server: Some(first.to_server.clone()),
                ..Shared::default()
            })),
        };
        let shared = server.shared.clone();
        tokio::spawn(route(shared, dial, expected, first));
        Ok(server)
    }

    /// The platform's supervisor, from this binary.
    pub async fn standard() -> Result<Self> {
        let places = Places::standard()
            .ok_or_else(|| Error::Server("no data directory for the runtime files".into()))?;
        let exe = std::env::current_exe()?;
        Self::start(places, Start::Binary(exe)).await
    }

    async fn send(&self, message: ToServer) -> Result<()> {
        let tx = lock(&self.shared).to_server.clone();
        let Some(tx) = tx else {
            return Err(self.gone("not connected to the local server"));
        };
        tx.send(message)
            .await
            .map_err(|_| self.gone("the local server went away"))
    }

    /// Why the server is not answering: by name when it is another build
    /// that this one could not read, and otherwise `what`.
    fn gone(&self, what: &str) -> Error {
        match lock(&self.shared).other_build.clone() {
            Some(other) => Error::DifferentBuild {
                theirs: other.theirs,
                expected: other.expected,
            },
            None => Error::Server(what.into()),
        }
    }

    /// Every session the server holds.
    pub async fn list(&self) -> Result<Vec<SessionInfo>> {
        let (tx, rx) = oneshot::channel();
        lock(&self.shared).waiting.push_back(Waiting::Listed(tx));
        self.send(ToServer::ListSessions).await?;
        tokio::time::timeout(ANSWER_WITHIN, rx)
            .await
            .map_err(|_| Error::Server("no session list in time".into()))?
            .map_err(|_| self.gone("the local server went away"))
    }

    /// Start a session. `pane` and `resume` are its Claude token and a
    /// conversation to resume (D48).
    pub async fn create(
        &self,
        cwd: Option<PathBuf>,
        cols: u16,
        rows: u16,
        pane: Option<String>,
        resume: Option<String>,
    ) -> Result<SessionId> {
        let (tx, rx) = oneshot::channel();
        lock(&self.shared).waiting.push_back(Waiting::Created(tx));
        self.send(ToServer::Create {
            name: None,
            cwd: cwd.map(|c| c.to_string_lossy().to_string()),
            cols,
            rows,
            pane,
            resume,
        })
        .await?;
        tokio::time::timeout(ANSWER_WITHIN, rx)
            .await
            .map_err(|_| Error::Server("no answer to create in time".into()))?
            .map_err(|_| self.gone("the local server went away"))?
            .map_err(Error::Server)
    }

    /// Show a session: a pty whose output starts with its screen. Closing the
    /// pty detaches; the session keeps running.
    pub async fn attach(
        &self,
        session: SessionId,
        cols: u16,
        rows: u16,
        pid: Option<u32>,
    ) -> Result<(Pty, mpsc::Receiver<PtyEvent>)> {
        let (to_route, from_server) = mpsc::channel(OUTPUT_QUEUE_SIZE);
        lock(&self.shared).routes.insert(
            session,
            Route {
                tx: to_route,
                cols,
                rows,
                behind: false,
            },
        );
        self.send(ToServer::Attach {
            session,
            cols,
            rows,
        })
        .await?;

        let (events_tx, events) = mpsc::channel(OUTPUT_QUEUE_SIZE);
        let missing = Arc::new(AtomicU8::new(HISTORY_WHOLE));
        tokio::spawn(translate(
            self.clone(),
            session,
            from_server,
            events_tx,
            missing.clone(),
        ));

        let (input, from_pty) = mpsc::channel(INPUT_QUEUE_SIZE);
        tokio::spawn(forward(self.clone(), session, from_pty, missing));
        Ok((
            Pty {
                input,
                pid,
                foreground: None,
                // The worker holds the buffer and says when it drops.
                queued: None,
            },
            events,
        ))
    }

    /// Be told whenever a worker reports what Claude is doing in its
    /// session. Called on the connection's task: cheap work only.
    pub fn on_claude(&self, listener: impl Fn(SessionId, v1::Claude) + Send + Sync + 'static) {
        lock(&self.shared).claude = Some(Arc::new(listener));
    }

    /// Be told the session list whenever it changes; the supervisor pushes
    /// it, so nobody polls. Called on the connection's task: cheap work only.
    pub fn on_sessions(&self, listener: impl Fn(Vec<SessionInfo>) + Send + Sync + 'static) {
        lock(&self.shared).sessions = Some(Arc::new(listener));
    }

    /// The app's window was hidden or shown: while hidden, workers send
    /// nothing, and shown again each attached session sends its screen (D50).
    pub async fn set_visible(&self, visible: bool) -> Result<()> {
        self.send(ToServer::Visible { visible }).await
    }

    /// The user has looked at a session: its pane was focused for about a
    /// second. Its finished and failed status records go (D53).
    pub async fn status_seen(&self, session: SessionId) -> Result<()> {
        self.send(ToServer::StatusSeen { session }).await
    }

    /// End a session for good. The app asks the user first (D32).
    pub async fn kill(&self, session: SessionId) -> Result<()> {
        self.send(ToServer::Kill { session }).await
    }

    fn resized(&self, session: SessionId, cols: u16, rows: u16) {
        if let Some(route) = lock(&self.shared).routes.get_mut(&session) {
            route.cols = cols;
            route.rows = rows;
        }
    }

    fn detached(&self, session: SessionId) {
        lock(&self.shared).routes.remove(&session);
    }
}

/// Confirm output to the worker every this many bytes handed on (D49), or
/// every this many messages, whichever comes first: the worker counts both
/// (`sync::MAX_IN_FLIGHT`), and a pty's small chunks reach the count first.
const CONFIRM_EVERY: u64 = 64 * 1024;
const CONFIRM_EVERY_MESSAGES: u32 = 32;
/// How long a pane waits for the history after a screen, and how much output
/// it keeps for the replacement meanwhile, before giving the rebuild up.
const HISTORY_WITHIN: Duration = Duration::from_secs(10);
const KEEP_AT_MOST: usize = 4 * 1024 * 1024;

/// Whether a pane's scrollback has the history the worker holds: whole, with
/// a gap from falling behind, or with a gap the worker was asked to fill. The
/// view's scrolling asks at most once per gap.
const HISTORY_WHOLE: u8 = 0;
const HISTORY_GAP: u8 = 1;
const HISTORY_ASKED: u8 = 2;

/// A screen whose history is on its way.
struct Rebuilding {
    screen: v1::Screen,
    /// Ranges as they came, each newest first.
    ranges: Vec<Vec<v1::Row>>,
    /// Output since the screen, which the pane already shows, kept for the
    /// replacement.
    kept: Vec<u8>,
    since: std::time::Instant,
}

/// A replacement terminal built beside the pane's from `ranges` of history
/// (each newest first, the newest range first), the screen above them and
/// the output since, swapped in at once.
fn rebuilt(out: &mut Vec<PtyEvent>, ranges: &[Vec<v1::Row>], screen: &v1::Screen, kept: &[u8]) {
    let mut history: Vec<v1::Row> = ranges.iter().flatten().cloned().collect();
    history.reverse();
    out.push(PtyEvent::Rebuild(berth_term::Shadow::Begin));
    out.push(PtyEvent::Rebuild(berth_term::Shadow::Write(
        berth_term::rebuild(&history, screen),
    )));
    if !kept.is_empty() {
        out.push(PtyEvent::Rebuild(berth_term::Shadow::Write(kept.to_vec())));
    }
    out.push(PtyEvent::Rebuild(berth_term::Shadow::Commit));
}

/// The server's messages for one session, as pty events.
///
/// A screen is drawn as soon as it arrives, and output keeps going to the
/// pane as it comes. When history follows the screen, a replacement terminal
/// is built beside the one on screen from the history, the screen and every
/// output since, and swapped in at once when the history is complete, so the
/// pane never stops, and its scrollback then equals the worker's line for
/// line under the worker's numbers; the app never numbers lines itself (D50,
/// D51). Output is confirmed back as it is handed on, which is how the worker
/// knows whether the app keeps up.
async fn translate(
    server: LocalServer,
    session: SessionId,
    mut from_server: mpsc::Receiver<ToApp>,
    events: mpsc::Sender<PtyEvent>,
    missing: Arc<AtomicU8>,
) {
    // Output at or below the last screen's number is already on it.
    let mut drawn = 0u64;
    let mut confirmed = 0u64;
    let mut unconfirmed_messages = 0u32;
    let mut rebuilding: Option<Rebuilding> = None;
    while let Some(message) = from_server.recv().await {
        let mut out: Vec<PtyEvent> = Vec::new();
        let screen_now = matches!(message, ToApp::Screen { .. });
        match message {
            ToApp::Screen {
                seq,
                screen,
                history_follows,
                ..
            } => match screen.unpack::<v1::Screen>() {
                Ok(screen) => {
                    drawn = seq;
                    out.push(PtyEvent::Output(berth_term::replay(&screen)));
                    // A screen without its history means output was dropped:
                    // the scrollback has a gap until history comes.
                    if !history_follows {
                        let _ = missing.compare_exchange(
                            HISTORY_WHOLE,
                            HISTORY_GAP,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        );
                    }
                    // A new screen supersedes a rebuild still waiting: output
                    // was dropped in between, so what was kept no longer
                    // follows on from its screen.
                    rebuilding = history_follows.then(|| Rebuilding {
                        screen,
                        ranges: Vec::new(),
                        kept: Vec::new(),
                        since: std::time::Instant::now(),
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "a screen from the local server did not unpack");
                    continue;
                }
            },
            ToApp::History {
                lines, complete, ..
            } => {
                let Some(pending) = rebuilding.as_mut() else {
                    continue;
                };
                match lines.unpack::<Vec<v1::Row>>() {
                    Ok(rows) => pending.ranges.push(rows),
                    Err(error) => {
                        tracing::warn!(%error, "history from the local server did not unpack");
                    }
                }
                // History the user scrolled up and asked for: its newest range
                // is put in at once, so the lines just above the screen show
                // while the rest is on its way, and the whole history replaces
                // it when complete. History nobody asked for keeps the pane's
                // scrollback as it is until it is all here.
                let first =
                    pending.ranges.len() == 1 && missing.load(Ordering::Relaxed) == HISTORY_ASKED;
                if complete && let Some(done) = rebuilding.take() {
                    rebuilt(&mut out, &done.ranges, &done.screen, &done.kept);
                    out.push(PtyEvent::HistoryWhole);
                    missing.store(HISTORY_WHOLE, Ordering::Relaxed);
                } else if first && let Some(pending) = rebuilding.as_ref() {
                    rebuilt(&mut out, &pending.ranges, &pending.screen, &pending.kept);
                }
            }
            ToApp::Output { seq, bytes, .. } => {
                if seq <= drawn {
                    continue;
                }
                drawn = seq;
                unconfirmed_messages += 1;
                if let Some(pending) = rebuilding.as_mut() {
                    if pending.since.elapsed() < HISTORY_WITHIN
                        && pending.kept.len() + bytes.len() <= KEEP_AT_MOST
                    {
                        pending.kept.extend_from_slice(&bytes);
                    } else {
                        // The history is not coming in time: the pane carries
                        // on as it is, without the rebuilt scrollback.
                        tracing::warn!(session, "history did not arrive in time");
                        rebuilding = None;
                        // Still missing; scrolling up may ask again.
                        missing.store(HISTORY_GAP, Ordering::Relaxed);
                    }
                }
                out.push(PtyEvent::Output(bytes));
            }
            ToApp::Event {
                event: v1::Event::InputDropped,
                ..
            } => out.push(PtyEvent::InputDropped),
            ToApp::Event {
                event: v1::Event::Exited { code },
                ..
            } => {
                let code = code.and_then(|c| u32::try_from(c).ok()).unwrap_or(0);
                let _ = events.send(PtyEvent::Exit(code)).await;
                break;
            }
            _ => continue,
        }
        for event in out {
            if events.send(event).await.is_err() {
                return;
            }
        }
        // Every so many bytes, and as soon as a screen is drawn: a worker
        // waiting on it sends nothing more until it is confirmed.
        if drawn >= confirmed + CONFIRM_EVERY
            || unconfirmed_messages >= CONFIRM_EVERY_MESSAGES
            || (screen_now && drawn > confirmed)
        {
            confirmed = drawn;
            unconfirmed_messages = 0;
            let _ = server
                .send(ToServer::Confirmed {
                    session,
                    seq: confirmed,
                })
                .await;
        }
    }
    let _ = events.send(PtyEvent::Closed).await;
}

/// What the view does to its pty, as messages for the session.
async fn forward(
    server: LocalServer,
    session: SessionId,
    mut from_pty: mpsc::Receiver<PtyInput>,
    missing: Arc<AtomicU8>,
) {
    while let Some(input) = from_pty.recv().await {
        let message = match input {
            PtyInput::WantHistory => {
                let asked = missing.compare_exchange(
                    HISTORY_GAP,
                    HISTORY_ASKED,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                if asked.is_err() {
                    continue;
                }
                ToServer::SendHistory { session }
            }
            PtyInput::Data(bytes) => ToServer::Input { session, bytes },
            PtyInput::Resize { cols, rows } => {
                server.resized(session, cols, rows);
                ToServer::Resize {
                    session,
                    cols,
                    rows,
                }
            }
            PtyInput::Close => break,
        };
        let _ = server.send(message).await;
    }
    // Closed, or every handle dropped: detach, never kill.
    server.detached(session);
    let _ = server.send(ToServer::Detach { session }).await;
}

/// Hand the server's messages to whoever is waiting, and keep connected.
async fn route(shared: Arc<Mutex<Shared>>, dial: Dial, expected: String, first: client::Client) {
    let mut client = first;
    loop {
        lock(&shared).to_server = Some(client.to_server.clone());
        while let Some(message) = client.from_server.recv().await {
            deliver(&shared, message);
        }
        // The connection is gone. Requests in flight will not be answered,
        // and are told why when the supervisor is another build that could
        // not be read.
        {
            let mut shared = lock(&shared);
            shared.to_server = None;
            shared.other_build = client
                .unreadable
                .load(Ordering::Relaxed)
                .then(|| client.other_build.clone())
                .flatten();
            shared.waiting.clear();
        }
        tracing::warn!("lost the local server; connecting again");
        client = loop {
            tokio::time::sleep(RETRY_EVERY).await;
            if let Ok(client) = reach(&dial, &expected).await {
                break client;
            }
        };
        // Everything that was on screen is attached again, and gets its
        // screen as it is now.
        let reattach: Vec<(SessionId, u16, u16)> = lock(&shared)
            .routes
            .iter()
            .map(|(id, r)| (*id, r.cols, r.rows))
            .collect();
        for (session, cols, rows) in reattach {
            let _ = client
                .to_server
                .send(ToServer::Attach {
                    session,
                    cols,
                    rows,
                })
                .await;
        }
    }
}

fn deliver(shared: &Arc<Mutex<Shared>>, message: ToApp) {
    let session = match &message {
        ToApp::Created { session } => {
            if let Some(Waiting::Created(tx)) = take(shared, |w| matches!(w, Waiting::Created(_))) {
                let _ = tx.send(Ok(*session));
            }
            return;
        }
        ToApp::Sessions(list) => {
            if let Some(Waiting::Listed(tx)) = take(shared, |w| matches!(w, Waiting::Listed(_))) {
                let _ = tx.send(list.clone());
            }
            // Answers and unasked pushes alike: the list as it is now.
            let listener = lock(shared).sessions.clone();
            if let Some(listener) = listener {
                listener(list.clone());
            }
            return;
        }
        ToApp::Refused { what, why } => {
            tracing::warn!(%what, %why, "the local server refused");
            if what.starts_with("create")
                && let Some(Waiting::Created(tx)) =
                    take(shared, |w| matches!(w, Waiting::Created(_)))
            {
                let _ = tx.send(Err(why.clone()));
            }
            return;
        }
        ToApp::Event {
            session,
            event: v1::Event::Claude(claude),
        } => {
            let listener = lock(shared).claude.clone();
            if let Some(listener) = listener {
                listener(*session, claude.clone());
            }
            return;
        }
        ToApp::Output { session, .. }
        | ToApp::Screen { session, .. }
        | ToApp::History { session, .. }
        | ToApp::Event { session, .. } => *session,
    };
    let mut guard = lock(shared);
    let Some(route) = guard.routes.get_mut(&session) else {
        return;
    };
    let screen = matches!(message, ToApp::Screen { .. });
    if route.behind && !screen {
        return;
    }
    match route.tx.try_send(message) {
        Ok(()) => route.behind = false,
        Err(mpsc::error::TrySendError::Full(_)) => {
            if !route.behind {
                route.behind = true;
                tracing::debug!(
                    session,
                    "a pane fell behind; asking for a screen when it drains"
                );
                tokio::spawn(catch_up(shared.clone(), session));
            }
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            guard.routes.remove(&session);
        }
    }
}

/// Once a pane that fell behind has emptied its queue, attach it again: the
/// worker answers with the screen as it is now.
async fn catch_up(shared: Arc<Mutex<Shared>>, session: SessionId) {
    loop {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let ready = {
            let guard = lock(&shared);
            let Some(route) = guard.routes.get(&session) else {
                return;
            };
            (route.tx.capacity() == route.tx.max_capacity()).then(|| {
                (
                    guard.to_server.clone(),
                    ToServer::Attach {
                        session,
                        cols: route.cols,
                        rows: route.rows,
                    },
                )
            })
        };
        if let Some((to_server, attach)) = ready {
            if let Some(to_server) = to_server {
                let _ = to_server.send(attach).await;
            }
            return;
        }
    }
}

/// The oldest request of a kind.
fn take(shared: &Arc<Mutex<Shared>>, kind: impl Fn(&Waiting) -> bool) -> Option<Waiting> {
    let mut shared = lock(shared);
    let at = shared.waiting.iter().position(kind)?;
    shared.waiting.remove(at)
}
