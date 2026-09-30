//! Local sessions as the app sees them: the supervisor's sessions, each
//! looking like an ordinary pty to the view and the output pump (D50).
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
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use berth_proto::v1::{self, SessionId, SessionInfo, ToApp, ToServer};
use tokio::sync::{mpsc, oneshot};

use super::client;
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

#[derive(Default)]
struct Shared {
    routes: HashMap<SessionId, Route>,
    waiting: VecDeque<Waiting>,
    to_server: Option<mpsc::Sender<ToServer>>,
}

/// The app's handle on the local server. Cheap to clone.
#[derive(Clone)]
pub struct LocalServer {
    shared: Arc<Mutex<Shared>>,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Connect to the supervisor, starting it if nothing answers and `start`
/// allows.
async fn reach(places: &Places, start: &Start) -> Result<client::Client> {
    if let Ok(client) = client::connect(places).await {
        return Ok(client);
    }
    let Start::Binary(exe) = start else {
        return Err(Error::Server("no supervisor answers".into()));
    };
    places.prepare()?;
    let copy = paths::install_copy(&places.bin, exe)?;
    super::supervisor::spawn_detached(&copy, &["--supervisor".to_string()], &[])?;
    let deadline = tokio::time::Instant::now() + START_WITHIN;
    loop {
        match client::connect(places).await {
            Ok(client) => return Ok(client),
            Err(error) if tokio::time::Instant::now() >= deadline => return Err(error),
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

impl LocalServer {
    /// Connect, starting the supervisor if needed, and keep connected.
    pub async fn start(places: Places, start: Start) -> Result<Self> {
        let first = reach(&places, &start).await?;
        let server = Self {
            shared: Arc::new(Mutex::new(Shared {
                to_server: Some(first.to_server.clone()),
                ..Shared::default()
            })),
        };
        let shared = server.shared.clone();
        tokio::spawn(route(shared, places, start, first));
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
        let tx = lock(&self.shared)
            .to_server
            .clone()
            .ok_or_else(|| Error::Server("not connected to the local server".into()))?;
        tx.send(message)
            .await
            .map_err(|_| Error::Server("the local server went away".into()))
    }

    /// Every session the server holds.
    pub async fn list(&self) -> Result<Vec<SessionInfo>> {
        let (tx, rx) = oneshot::channel();
        lock(&self.shared).waiting.push_back(Waiting::Listed(tx));
        self.send(ToServer::ListSessions).await?;
        tokio::time::timeout(ANSWER_WITHIN, rx)
            .await
            .map_err(|_| Error::Server("no session list in time".into()))?
            .map_err(|_| Error::Server("the local server went away".into()))
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
            .map_err(|_| Error::Server("the local server went away".into()))?
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
        tokio::spawn(translate(from_server, events_tx));

        let (input, from_pty) = mpsc::channel(INPUT_QUEUE_SIZE);
        tokio::spawn(forward(self.clone(), session, from_pty));
        Ok((Pty { input, pid }, events))
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

/// The server's messages for one session, as pty events.
async fn translate(mut from_server: mpsc::Receiver<ToApp>, events: mpsc::Sender<PtyEvent>) {
    // Output at or below the last screen's number is already on it.
    let mut drawn = 0u64;
    while let Some(message) = from_server.recv().await {
        let event = match message {
            ToApp::Screen { seq, screen, .. } => match screen.unpack::<v1::Screen>() {
                Ok(screen) => {
                    drawn = seq;
                    PtyEvent::Output(berth_term::replay(&screen))
                }
                Err(error) => {
                    tracing::warn!(%error, "a screen from the local server did not unpack");
                    continue;
                }
            },
            ToApp::Output { seq, bytes, .. } => {
                if seq <= drawn {
                    continue;
                }
                PtyEvent::Output(bytes)
            }
            ToApp::Event {
                event: v1::Event::Exited { code },
                ..
            } => {
                let code = code.and_then(|c| u32::try_from(c).ok()).unwrap_or(0);
                let _ = events.send(PtyEvent::Exit(code)).await;
                break;
            }
            _ => continue,
        };
        if events.send(event).await.is_err() {
            return;
        }
    }
    let _ = events.send(PtyEvent::Closed).await;
}

/// What the view does to its pty, as messages for the session.
async fn forward(server: LocalServer, session: SessionId, mut from_pty: mpsc::Receiver<PtyInput>) {
    while let Some(input) = from_pty.recv().await {
        let message = match input {
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
async fn route(shared: Arc<Mutex<Shared>>, places: Places, start: Start, first: client::Client) {
    let mut client = first;
    loop {
        lock(&shared).to_server = Some(client.to_server.clone());
        while let Some(message) = client.from_server.recv().await {
            deliver(&shared, message);
        }
        // The connection is gone. Requests in flight will not be answered.
        {
            let mut shared = lock(&shared);
            shared.to_server = None;
            shared.waiting.clear();
        }
        tracing::warn!("lost the local server; connecting again");
        client = loop {
            tokio::time::sleep(RETRY_EVERY).await;
            if let Ok(client) = reach(&places, &start).await {
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
