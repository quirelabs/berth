//! The supervisor: one per user. Owns the endpoint the app connects to, the
//! list of sessions, and starting workers. Holds no terminal, so a crash or a
//! restart of it never loses a shell: workers keep running, and the next
//! supervisor finds them again from their files in the runtime directory
//! (D50).
//!
//! One task owns the table and handles every event in turn; each client
//! connection and each worker link has its own reading and writing tasks
//! around it, talking to it over channels.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use berth_proto::core;
use berth_proto::v1::{self, SessionId, SessionInfo, ToApp, ToServer};
use tokio::sync::{mpsc, oneshot};

use super::link::{self, protocol};
use super::paths::{self, Places};
use super::transport::Listener;
use super::worker_host::{self, Launch};
use crate::error::{Error, Result};
use crate::worker::Shell;

/// How long a supervisor with no sessions and no clients stays up: about
/// ten seconds, so an idle host runs nothing of Berth's (D50, D55).
pub const IDLE_EXIT: Duration = Duration::from_secs(10);

/// Messages queued for one client or one worker link.
const QUEUE: usize = 1024;

/// How a supervisor starts a worker.
#[derive(Clone)]
pub enum Launcher {
    /// A process of its own, from the versioned copy of the binary (D50),
    /// detached so it outlives the supervisor.
    Process { exe: PathBuf },
    /// A task in this process, over the same endpoints. For tests: the
    /// handles let a test kill a worker the way a crash would.
    InProcess {
        shell: Option<Shell>,
        tasks: Arc<Mutex<HashMap<SessionId, tokio::task::AbortHandle>>>,
    },
}

pub struct Config {
    pub places: Places,
    pub launcher: Launcher,
    pub idle: Duration,
}

type ClientId = u64;

enum Event {
    ClientUp(ClientId, mpsc::Sender<ToApp>),
    ClientSaid(ClientId, ToServer),
    ClientGone(ClientId),
    WorkerUp {
        info: Box<SessionInfo>,
        to_worker: ToWorker,
        build: String,
        reply_to: Option<ClientId>,
    },
    WorkerFailed {
        session: SessionId,
        reply_to: Option<ClientId>,
        why: String,
        /// The worker's build, when it greeted and then could not be read.
        build: Option<String>,
    },
    /// The core (D54, D55): asked by any build, answered by this one.
    Core {
        client: ClientId,
        ask: core::Ask,
        reply: oneshot::Sender<core::Answer>,
    },
    WorkerSaid(SessionId, ToApp),
    WorkerGone(SessionId),
    /// A client that fell behind has emptied its queue.
    CaughtUp(ClientId),
}

/// The way to a worker. The table never waits on it: a worker busy sending
/// output may not be reading, and the table waiting on it would stop taking
/// that very output, so neither would ever move again (found 2026-10-02 for
/// confirmations, and 2026-10-10 for everything else: a key typed into a
/// flooded session froze it for good).
///
/// What must arrive (input, attach, kill and the rest) is queued, and what
/// each client has queued and not yet written is counted against that
/// client ([`MUST_AT_MOST`]): a freeze is not traded for memory without
/// limit. A client over its limit is disconnected, never the worker; it
/// connects again and attaches to a fresh screen. Confirmations are
/// cumulative, so one that finds its queue full is dropped and carried by
/// the next.
#[derive(Clone)]
struct ToWorker {
    must: mpsc::UnboundedSender<(ToServer, Owed)>,
    confirmations: mpsc::Sender<ToServer>,
}

/// The most one client may have queued for workers and not yet written, in
/// bytes. A megabyte is a long paste; past it the client is not typing, it
/// is flooding a worker that is not reading.
const MUST_AT_MOST: usize = 1024 * 1024;

/// A queued message's weight on its client's count, taken off when the
/// message is written to the worker or dropped with its queue.
struct Owed {
    by: Arc<std::sync::atomic::AtomicUsize>,
    bytes: usize,
}

impl Drop for Owed {
    fn drop(&mut self) {
        self.by
            .fetch_sub(self.bytes, std::sync::atomic::Ordering::Relaxed);
    }
}

/// What a message weighs in a queue: its bytes, and a little for itself.
fn weight(message: &ToServer) -> usize {
    32 + match message {
        ToServer::Input { bytes, .. } => bytes.len(),
        _ => 0,
    }
}

impl ToWorker {
    /// Queue a message that must arrive. False if the worker is gone.
    fn send(&self, message: ToServer, owed: Owed) -> bool {
        self.must.send((message, owed)).is_ok()
    }

    /// Hand over a confirmation if there is room.
    fn confirm(&self, message: ToServer) {
        let _ = self.confirmations.try_send(message);
    }
}

struct Session {
    info: SessionInfo,
    to_worker: ToWorker,
    attached: HashSet<ClientId>,
    /// The size each attached client shows it at. A session has one size,
    /// a terminal's: the client that was last active sets it, and the
    /// others see the screen at that size (D55).
    sizes: HashMap<ClientId, (u16, u16)>,
    /// The client whose size the session has, and when it was last active.
    sized_by: Option<(ClientId, std::time::Instant)>,
    build: String,
}

/// How long a client that set a session's size holds it against another
/// typing too: two apps typing in the same second do not resize each other
/// back and forth (D55).
const SIZE_HELD: Duration = Duration::from_secs(1);

impl Session {
    /// `client` was active at its size: the session takes it.
    fn sized(&mut self, client: ClientId, (cols, rows): (u16, u16), now: std::time::Instant) {
        self.sizes.insert(client, (cols, rows));
        self.info.cols = cols;
        self.info.rows = rows;
        self.sized_by = Some((client, now));
    }

    /// `client` typed. The size the session should take first, if any: its
    /// own, unless that is the session's already, or another client set the
    /// size and was active within the last second, whose size then holds.
    fn typed(&mut self, client: ClientId, now: std::time::Instant) -> Option<(u16, u16)> {
        match self.sized_by {
            Some((by, _)) if by == client => {
                self.sized_by = Some((client, now));
                return None;
            }
            Some((_, at)) if now.duration_since(at) < SIZE_HELD => return None,
            _ => {}
        }
        let size = self.sizes.get(&client).copied()?;
        if size == (self.info.cols, self.info.rows) {
            self.sized_by = Some((client, now));
            return None;
        }
        self.sized(client, size, now);
        Some(size)
    }
}

/// One client's own bounded queue. The table never waits on a client: a full
/// queue marks that client alone as behind, and nothing is queued for it
/// while it is (D50). A slow client can so never hold up any other session or
/// client.
///
/// Once its queue has drained, each session it is attached to is asked for a
/// fresh screen, and that session's output is dropped for the client until
/// the screen has gone out, so it never draws output on top of a gap. The
/// sync piece refines what a client that falls behind is sent; this is what
/// keeps it from staying behind for good.
struct ClientQueue {
    tx: mpsc::Sender<ToApp>,
    /// Bytes this client has queued for workers that are not yet written.
    owed: Arc<std::sync::atomic::AtomicUsize>,
    behind: bool,
    /// Sessions whose next screen this client must get before their output.
    awaiting: HashSet<SessionId>,
}

struct State {
    config: Config,
    events: mpsc::Sender<Event>,
    sessions: HashMap<SessionId, Session>,
    clients: HashMap<ClientId, ClientQueue>,
    /// Workers found running that greeted and then could not be read, by
    /// the build they greeted as: another draft's (D54). They are not in the
    /// session list, which this build cannot fill in for them, and are in
    /// the core's, so they can still be seen and ended.
    unreadable: HashMap<SessionId, String>,
    /// Sessions being started, so an idle check does not exit under them.
    starting: usize,
    next_session: SessionId,
}

/// Run until idle. Refuses to start if another supervisor answers at the
/// endpoint.
pub async fn run(config: Config) -> Result<()> {
    config.places.prepare()?;
    let mut listener = Listener::bind(&config.places.supervisor())
        .await
        .map_err(|e| Error::Server(format!("could not listen: {e}")))?;
    paths::write_pid(&config.places.supervisor_pid())?;

    let (events, mut inbox) = mpsc::channel(QUEUE);
    let found = paths::live_workers(&config.places);
    let mut state = State {
        next_session: found.iter().max().map_or(1, |m| m + 1),
        starting: found.len(),
        config,
        events,
        sessions: HashMap::new(),
        clients: HashMap::new(),
        unreadable: HashMap::new(),
    };
    for session in found {
        state.connect_worker(session, None);
    }
    tracing::info!(workers = state.starting, "supervisor ready");

    let mut next_client: ClientId = 0;
    let mut cleaned = false;
    loop {
        let idle = state.sessions.is_empty() && state.clients.is_empty() && state.starting == 0;
        let idle_for = state.config.idle;
        tokio::select! {
            accepted = listener.accept() => {
                let Ok(stream) = accepted else { continue };
                next_client += 1;
                tokio::spawn(client(next_client, stream, state.events.clone()));
            }
            Some(event) = inbox.recv() => {
                state.handle(event).await;
                if !cleaned && state.starting == 0 {
                    state.remove_unused_copies();
                    cleaned = true;
                }
            }
            _ = tokio::time::sleep(idle_for), if idle => {
                tracing::info!("no sessions and no clients, exiting");
                break;
            }
        }
    }
    let _ = std::fs::remove_file(state.config.places.supervisor_pid());
    Ok(())
}

/// `berth --supervisor`: the supervisor as the app starts it, running workers
/// from its own binary, which is the versioned copy (D50).
pub async fn run_process() -> Result<()> {
    let places = Places::standard()
        .ok_or_else(|| Error::Server("no data directory for the runtime files".into()))?;
    let exe = std::env::current_exe()?;
    run(Config {
        places,
        launcher: Launcher::Process { exe },
        idle: IDLE_EXIT,
    })
    .await
}

/// One client: handshake, then its messages to the table and the table's to
/// it. A peer that does not speak the protocol is dropped here.
async fn client(id: ClientId, stream: super::transport::Stream, events: mpsc::Sender<Event>) {
    let opened = match link::open(stream, &link::hello()).await {
        Ok(opened) => opened,
        Err(error) => {
            tracing::warn!(%error, "dropped a client that did not greet");
            return;
        }
    };
    let other = opened.other_build(&super::build());
    let (tx, mut rx) = mpsc::channel::<ToApp>(QUEUE);
    if events.send(Event::ClientUp(id, tx)).await.is_err() {
        return;
    }
    // Core answers go out as they are, beside the version's messages.
    let (core_tx, mut core_rx) = mpsc::channel::<Vec<u8>>(4);
    let mut write = opened.write;
    let mut writer = tokio::spawn(async move {
        loop {
            let payload = tokio::select! {
                // In this order every time: a core answer is rare and must
                // not wait behind a flood of output, and checking for one
                // costs the output nothing worth counting.
                biased;
                Some(payload) = core_rx.recv() => payload,
                // The table dropping this client's queue ends the writer,
                // and with it the connection, whatever the core is doing.
                message = rx.recv() => match message.map(|m| m.encode()) {
                    Some(Ok(payload)) => payload,
                    Some(Err(_)) => continue,
                    None => break,
                },
            };
            if write.send(&payload).await.is_err() {
                break;
            }
        }
    });
    let mut read = opened.read;
    loop {
        tokio::select! {
            // The table dropped this client's queue: the supervisor is going,
            // and the connection goes with it rather than lingering open.
            _ = &mut writer => break,
            received = read.recv() => {
                let Ok(Some(payload)) = received else { break };
                // Before anything of a version: the core is read whatever
                // else this client and this build make of each other.
                if core::is_core(&payload) {
                    let Ok(ask) = core::Ask::decode(&payload) else { continue };
                    let (reply, answer) = oneshot::channel();
                    if events.send(Event::Core { client: id, ask, reply }).await.is_err() {
                        break;
                    }
                    if let Ok(answer) = answer.await
                        && let Ok(payload) = answer.encode()
                    {
                        let _ = core_tx.send(payload).await;
                    }
                    continue;
                }
                match ToServer::decode(&payload) {
                    Ok(message) => {
                        if events.send(Event::ClientSaid(id, message)).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let error = link::unreadable(error, other.as_ref());
                        tracing::warn!(%error, "a client sent something unreadable");
                        break;
                    }
                }
            }
        }
    }
    writer.abort();
    let _ = events.send(Event::ClientGone(id)).await;
}

impl State {
    fn send_client(&mut self, client: ClientId, message: ToApp) {
        let session = match &message {
            ToApp::Output { session, .. }
            | ToApp::Screen { session, .. }
            | ToApp::History { session, .. }
            | ToApp::Event { session, .. } => Some(*session),
            _ => None,
        };
        let screen = matches!(message, ToApp::Screen { .. });
        let events = self.events.clone();
        let Some(queue) = self.clients.get_mut(&client) else {
            return;
        };
        if queue.behind {
            return;
        }
        if let Some(session) = session
            && queue.awaiting.contains(&session)
        {
            if !screen {
                return;
            }
            queue.awaiting.remove(&session);
        }
        if let Err(mpsc::error::TrySendError::Full(_)) = queue.tx.try_send(message) {
            tracing::warn!(client, "a client fell behind");
            queue.behind = true;
            let tx = queue.tx.clone();
            tokio::spawn(async move {
                // Drained, or gone: either way this watch is over.
                while !tx.is_closed() && tx.capacity() < tx.max_capacity() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                drop(tx);
                let _ = events.send(Event::CaughtUp(client)).await;
            });
        }
    }

    fn send_attached(&mut self, session: SessionId, message: ToApp) {
        let attached: Vec<ClientId> = match self.sessions.get(&session) {
            Some(entry) => entry.attached.iter().copied().collect(),
            None => return,
        };
        for client in attached {
            self.send_client(client, message.clone());
        }
    }

    /// A client caught up: every session it shows sends a fresh screen, which
    /// it gets before any more of their output.
    async fn caught_up(&mut self, client: ClientId) {
        let attached: Vec<(SessionId, u16, u16)> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.attached.contains(&client))
            .map(|(id, s)| (*id, s.info.cols, s.info.rows))
            .collect();
        let Some(queue) = self.clients.get_mut(&client) else {
            return;
        };
        queue.behind = false;
        queue.awaiting = attached.iter().map(|(id, _, _)| *id).collect();
        tracing::info!(client, sessions = attached.len(), "a client caught up");
        for (session, cols, rows) in attached {
            self.queue_for_worker(
                client,
                session,
                ToServer::Attach {
                    session,
                    cols,
                    rows,
                },
            )
            .await;
        }
    }

    /// Queue a client's message for a session's worker. False if there is
    /// no such session. Never waits. A client whose queued messages pass
    /// [`MUST_AT_MOST`] is disconnected here, and the message with it.
    async fn queue_for_worker(
        &mut self,
        client: ClientId,
        session: SessionId,
        message: ToServer,
    ) -> bool {
        use std::sync::atomic::Ordering;
        if !self.sessions.contains_key(&session) {
            return false;
        }
        let Some(owed) = self.clients.get(&client).map(|queue| queue.owed.clone()) else {
            // The client is gone; so is what it asked.
            return true;
        };
        let bytes = weight(&message);
        if owed.load(Ordering::Relaxed) + bytes > MUST_AT_MOST {
            tracing::warn!(
                client,
                session,
                "a client queued more than a worker is taking: disconnected"
            );
            self.drop_client(client);
            return true;
        }
        owed.fetch_add(bytes, Ordering::Relaxed);
        let owed = Owed { by: owed, bytes };
        self.sessions
            .get(&session)
            .is_some_and(|entry| entry.to_worker.send(message, owed))
    }

    /// Forget a client: its queue closes, which ends its connection, and no
    /// session counts it as attached. The sessions themselves are untouched.
    fn drop_client(&mut self, client: ClientId) {
        self.clients.remove(&client);
        for session in self.sessions.values_mut() {
            session.attached.remove(&client);
            session.sizes.remove(&client);
        }
    }

    async fn handle(&mut self, event: Event) {
        match event {
            Event::ClientUp(id, tx) => {
                self.clients.insert(
                    id,
                    ClientQueue {
                        tx,
                        owed: Arc::default(),
                        behind: false,
                        awaiting: HashSet::new(),
                    },
                );
            }
            Event::ClientGone(id) => self.drop_client(id),
            Event::ClientSaid(id, message) => self.client_said(id, message).await,
            Event::CaughtUp(id) => self.caught_up(id).await,
            Event::WorkerUp {
                info,
                to_worker,
                build,
                reply_to,
            } => {
                self.starting = self.starting.saturating_sub(1);
                let session = info.id;
                self.sessions.insert(
                    session,
                    Session {
                        info: *info,
                        to_worker,
                        attached: HashSet::new(),
                        sizes: HashMap::new(),
                        sized_by: None,
                        build,
                    },
                );
                if let Some(client) = reply_to {
                    self.send_client(client, ToApp::Created { session });
                }
                self.push_sessions();
            }
            Event::Core { client, ask, reply } => {
                if let core::Ask::End { session } = ask {
                    self.core_end(client, session).await;
                }
                let _ = reply.send(core::Answer::Sessions(self.core_sessions()));
            }
            Event::WorkerFailed {
                session,
                reply_to,
                why,
                build,
            } => {
                self.starting = self.starting.saturating_sub(1);
                tracing::warn!(session, %why, "a worker did not start");
                if let (None, Some(build)) = (reply_to, build) {
                    self.unreadable.insert(session, build);
                }
                if let Some(client) = reply_to {
                    self.send_client(
                        client,
                        ToApp::Refused {
                            what: format!("create session {session}"),
                            why,
                        },
                    );
                }
            }
            Event::WorkerSaid(session, message) => {
                let ended = matches!(
                    message,
                    ToApp::Event {
                        event: v1::Event::Exited { .. },
                        ..
                    }
                );
                // The session list says what Claude is doing, for sessions no
                // tab shows.
                let mut changed = false;
                if let ToApp::Event {
                    event: v1::Event::Claude(claude),
                    ..
                } = &message
                    && let Some(entry) = self.sessions.get_mut(&session)
                {
                    changed = entry.info.claude.as_ref() != Some(claude);
                    entry.info.claude = Some(claude.clone());
                }
                // A worker telling what has its terminal now.
                if let ToApp::Sessions(list) = message {
                    if let (Some(entry), Some(info)) =
                        (self.sessions.get_mut(&session), list.into_iter().next())
                        && (entry.info.program != info.program
                            || entry.info.cwd != info.cwd
                            || entry.info.status != info.status
                            || entry.info.title != info.title)
                    {
                        entry.info.program = info.program;
                        entry.info.cwd = info.cwd;
                        entry.info.status = info.status;
                        entry.info.title = info.title;
                        self.push_sessions();
                    }
                    return;
                }
                self.send_attached(session, message);
                if ended {
                    self.sessions.remove(&session);
                }
                if changed || ended {
                    self.push_sessions();
                }
            }
            Event::WorkerGone(session) => {
                // Gone without saying it exited: a crash. The tab says the
                // session ended.
                if self.sessions.contains_key(&session) {
                    self.send_attached(
                        session,
                        ToApp::Event {
                            session,
                            event: v1::Event::Exited { code: None },
                        },
                    );
                    self.sessions.remove(&session);
                    self.push_sessions();
                }
            }
        }
    }

    /// Every session here as any build can be told of it (D55): the ones in
    /// the session list, and workers of a build this one cannot read.
    fn core_sessions(&mut self) -> Vec<core::Session> {
        let places = &self.config.places;
        self.unreadable.retain(|session, _| {
            paths::read_pid(&places.worker_pid(*session)).is_some_and(paths::alive)
        });
        let mut list: Vec<core::Session> = self
            .sessions
            .values()
            .map(|s| core::Session {
                id: s.info.id,
                title: s.info.title.clone().unwrap_or_else(|| s.info.name.clone()),
                build: s.build.clone(),
                alive: true,
            })
            .chain(self.unreadable.iter().map(|(id, build)| core::Session {
                id: *id,
                title: String::new(),
                build: build.clone(),
                alive: true,
            }))
            .collect();
        list.sort_by_key(|s| s.id);
        list
    }

    /// End a session for the core. One this build reads is ended as any
    /// other; a worker it cannot read is ended as a process, which needs
    /// nothing of its build.
    async fn core_end(&mut self, client: ClientId, session: SessionId) {
        if self.sessions.contains_key(&session) {
            self.forward(client, session, ToServer::Kill { session })
                .await;
            return;
        }
        if !self.unreadable.contains_key(&session) {
            return;
        }
        let pid = paths::read_pid(&self.config.places.worker_pid(session));
        if let Some(pid) = pid {
            tracing::info!(session, pid, "ending a worker of another build");
            let ended = tokio::task::spawn_blocking(move || super::remove::end_worker(pid)).await;
            if matches!(ended, Ok(true)) {
                self.unreadable.remove(&session);
            }
        }
    }

    fn session_list(&self) -> Vec<SessionInfo> {
        let mut list: Vec<SessionInfo> = self.sessions.values().map(|s| s.info.clone()).collect();
        list.sort_by_key(|s| s.id);
        list
    }

    /// Tell every client the session list, as it changes: a session started
    /// or ended, or what runs in one, where, or what Claude is doing there
    /// changed. Apps keep their list from these rather than asking for it on
    /// a timer.
    fn push_sessions(&mut self) {
        let list = self.session_list();
        let clients: Vec<ClientId> = self.clients.keys().copied().collect();
        for client in clients {
            self.send_client(client, ToApp::Sessions(list.clone()));
        }
    }

    async fn client_said(&mut self, client: ClientId, message: ToServer) {
        match message {
            ToServer::ListSessions => {
                let list = self.session_list();
                self.send_client(client, ToApp::Sessions(list));
            }
            ToServer::Create {
                name,
                cwd,
                cols,
                rows,
                pane,
                resume,
            } => {
                let session = self.next_session;
                self.next_session += 1;
                self.starting += 1;
                let launch = Launch {
                    session,
                    name: name.unwrap_or_else(|| format!("session {session}")),
                    cwd: cwd.map(PathBuf::from),
                    cols,
                    rows,
                    shell: None,
                    pane,
                    resume,
                };
                self.start_worker(launch, client);
            }
            ToServer::Attach {
                session,
                cols,
                rows,
            } => {
                let Some(entry) = self.sessions.get_mut(&session) else {
                    self.refuse(client, format!("attach {session}"), "no such session");
                    return;
                };
                entry.attached.insert(client);
                entry.sized(client, (cols, rows), std::time::Instant::now());
                self.queue_for_worker(client, session, message).await;
            }
            ToServer::Detach { session } => {
                if let Some(entry) = self.sessions.get_mut(&session) {
                    entry.attached.remove(&client);
                    entry.sizes.remove(&client);
                }
            }
            ToServer::Resize {
                session,
                cols,
                rows,
            } => {
                if let Some(entry) = self.sessions.get_mut(&session) {
                    entry.sized(client, (cols, rows), std::time::Instant::now());
                }
                self.forward(client, session, message).await;
            }
            // Typing is being active: a client typing into a session that is
            // at another client's size takes it back to its own first, so
            // what it types meets the screen it sees, unless the other was
            // active within the second (D55). Scrolling stays each client's
            // own.
            ToServer::Input { session, .. } => {
                let now = std::time::Instant::now();
                let resize = self.sessions.get_mut(&session).and_then(|entry| {
                    let (cols, rows) = entry.typed(client, now)?;
                    Some(ToServer::Resize {
                        session,
                        cols,
                        rows,
                    })
                });
                if let Some(resize) = resize {
                    self.forward(client, session, resize).await;
                }
                self.forward(client, session, message).await;
            }
            ToServer::Kill { session } => {
                self.forward(client, session, message).await;
            }
            // Dropped rather than queued when the worker is not reading:
            // confirmations are cumulative, so the next carries it, and a
            // worker does not wait on a lost one for long.
            ToServer::Confirmed { session, .. } => {
                if let Some(entry) = self.sessions.get(&session) {
                    entry.to_worker.confirm(message);
                }
            }
            ToServer::SendHistory { session } | ToServer::StatusSeen { session } => {
                self.forward(client, session, message).await;
            }
            ToServer::Visible { .. } => {
                let attached: Vec<SessionId> = self
                    .sessions
                    .iter()
                    .filter(|(_, s)| s.attached.contains(&client))
                    .map(|(id, _)| *id)
                    .collect();
                for session in attached {
                    self.queue_for_worker(client, session, message.clone())
                        .await;
                }
            }
        }
    }

    async fn forward(&mut self, client: ClientId, session: SessionId, message: ToServer) {
        if !self.queue_for_worker(client, session, message).await {
            self.refuse(client, format!("session {session}"), "no such session");
        }
    }

    fn refuse(&mut self, client: ClientId, what: String, why: &str) {
        self.send_client(
            client,
            ToApp::Refused {
                what,
                why: why.into(),
            },
        );
    }

    fn start_worker(&self, launch: Launch, reply_to: ClientId) {
        let session = launch.session;
        match &self.config.launcher {
            Launcher::Process { exe } => {
                let env = [
                    (crate::env::WORKER_PANE, launch.pane.clone()),
                    (crate::env::WORKER_RESUME, launch.resume.clone()),
                ];
                if let Err(error) = spawn_detached(exe, &launch.args(), &env) {
                    let events = self.events.clone();
                    tokio::spawn(async move {
                        let _ = events
                            .send(Event::WorkerFailed {
                                session,
                                reply_to: Some(reply_to),
                                why: error.to_string(),
                                build: None,
                            })
                            .await;
                    });
                    return;
                }
            }
            Launcher::InProcess { shell, tasks } => {
                let places = self.config.places.clone();
                let launch = Launch {
                    shell: shell.clone(),
                    ..launch
                };
                let task = tokio::spawn(async move {
                    if let Err(error) = worker_host::serve(places, launch).await {
                        tracing::warn!(%error, "an in-process worker failed");
                    }
                });
                if let Ok(mut tasks) = tasks.lock() {
                    tasks.insert(session, task.abort_handle());
                }
            }
        }
        self.connect_worker(session, Some(reply_to));
    }

    /// Reach a worker's endpoint, learn what it is, and wire it to the table.
    fn connect_worker(&self, session: SessionId, reply_to: Option<ClientId>) {
        let places = self.config.places.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let failed = |why: String| Event::WorkerFailed {
                session,
                reply_to,
                why,
                build: None,
            };
            let stream = match worker_host::reach(&places, session).await {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = events.send(failed(error.to_string())).await;
                    return;
                }
            };
            let mut opened = match link::open(stream, &link::hello()).await {
                Ok(opened) => opened,
                Err(error) => {
                    let _ = events.send(failed(error.to_string())).await;
                    return;
                }
            };
            // A worker left by another build is still reached: its session
            // lives until it ends (D50). If the two differ on the wire, that
            // is named as what it is (D54).
            let other = opened.other_build(&super::build());
            if let Some(other) = &other {
                tracing::info!(session, theirs = %other.theirs, "a worker is another build");
            }
            let info = match describe(&mut opened, other.as_ref()).await {
                Ok(info) => info,
                Err(error) => {
                    // It greeted, so it is running and its build is known:
                    // kept for the core's list.
                    let _ = events
                        .send(Event::WorkerFailed {
                            session,
                            reply_to,
                            why: error.to_string(),
                            build: Some(opened.theirs.build.clone()),
                        })
                        .await;
                    return;
                }
            };
            let (must, mut must_go) = mpsc::unbounded_channel::<(ToServer, Owed)>();
            let (confirmations, mut confirmed) = mpsc::channel::<ToServer>(QUEUE);
            let to_worker = ToWorker {
                must,
                confirmations,
            };
            let mut write = opened.write;
            tokio::spawn(async move {
                loop {
                    // What must arrive first: a key typed is not kept behind
                    // a backlog of confirmations.
                    let (message, _owed) = tokio::select! {
                        biased;
                        Some((message, owed)) = must_go.recv() => (message, Some(owed)),
                        Some(message) = confirmed.recv() => (message, None),
                        else => break,
                    };
                    let Ok(payload) = message.encode() else {
                        continue;
                    };
                    // `_owed` is held until the message is written: only
                    // then is it off its client's count.
                    if write.send(&payload).await.is_err() {
                        break;
                    }
                }
            });
            let build = opened.theirs.build.clone();
            if events
                .send(Event::WorkerUp {
                    info: Box::new(info),
                    to_worker,
                    build,
                    reply_to,
                })
                .await
                .is_err()
            {
                return;
            }
            let mut read = opened.read;
            while let Ok(Some(payload)) = read.recv().await {
                match ToApp::decode(&payload) {
                    Ok(message) => {
                        if events
                            .send(Event::WorkerSaid(session, message))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        let error = link::unreadable(error, other.as_ref());
                        tracing::warn!(session, %error, "a worker sent something unreadable");
                        break;
                    }
                }
            }
            let _ = events.send(Event::WorkerGone(session)).await;
        });
    }

    /// Delete builds that neither this supervisor nor any running worker
    /// uses (D50).
    fn remove_unused_copies(&self) {
        let mut keep: HashSet<String> = self.sessions.values().map(|s| s.build.clone()).collect();
        keep.insert(super::build());
        if let Launcher::Process { exe } = &self.config.launcher
            && let Ok(build) = paths::build_of(exe)
        {
            keep.insert(build);
        }
        paths::remove_unused_builds(&self.config.places.bin, &keep);
    }
}

/// Ask a freshly reached worker what it is. Output it sends first is dropped:
/// whoever attaches gets a screen.
async fn describe<S>(
    opened: &mut link::Opened<S>,
    other: Option<&link::OtherBuild>,
) -> Result<SessionInfo>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite,
{
    let ask = ToServer::ListSessions.encode().map_err(protocol)?;
    opened.write.send(&ask).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let payload = tokio::time::timeout_at(deadline, opened.read.recv())
            .await
            .map_err(|_| Error::Server("a worker did not describe itself".into()))??
            .ok_or_else(|| Error::Server("a worker closed before describing itself".into()))?;
        if let ToApp::Sessions(mut list) =
            ToApp::decode(&payload).map_err(|e| link::unreadable(e, other))?
            && let Some(info) = list.pop()
        {
            return Ok(info);
        }
    }
}

/// Detaching, through the C library (`libc`, D55).
#[cfg(unix)]
mod detach {
    /// The most descriptors looked at, whatever the limit says: a process
    /// started from an ssh channel holds a handful.
    const AT_MOST: libc::c_int = 65_536;

    /// Start a session of its own. Async-signal-safe, for between fork and
    /// exec.
    pub fn setsid() -> libc::pid_t {
        // SAFETY: no arguments, no memory.
        unsafe { libc::setsid() }
    }

    /// Mark every descriptor past stderr to close on exec, so a child
    /// started next holds none of them: what this process was handed, by
    /// an ssh channel or anything else, stays here. Rust opens its own that
    /// way already.
    pub fn close_on_exec() {
        // SAFETY: plain integers in and out.
        let limit = unsafe { libc::getdtablesize() }.clamp(3, AT_MOST);
        for fd in 3..limit {
            // SAFETY: sets one flag on a descriptor; one that is not open
            // fails harmlessly.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
    }
}

/// Start a process that outlives this one: a session of its own on Unix,
/// holding none of this process's descriptors but the standard three, sent
/// to /dev/null; detached and out of the job object on Windows (D49, D55).
///
/// `env` sets or, where `None`, removes variables, so a value the supervisor
/// happened to inherit never reaches a worker by accident.
pub fn spawn_detached(
    exe: &std::path::Path,
    args: &[String],
    env: &[(&str, Option<String>)],
) -> std::io::Result<()> {
    let mut command = std::process::Command::new(exe);
    for (name, value) in env {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Nothing past the standard three goes with it, so it holds open
        // nothing this process was handed: an exec channel waits for all of
        // its descriptors to close (D55).
        detach::close_on_exec();
        // A session of its own, so nothing that ends the session it was
        // started from (an ssh channel closing, a terminal hanging up)
        // reaches it.
        // SAFETY: between fork and exec only setsid is called, which is
        // async-signal-safe, and nothing is allocated.
        unsafe {
            command.pre_exec(|| {
                if detach::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        command.creation_flags(
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB,
        );
        if command.spawn().is_ok() {
            return Ok(());
        }
        // A job that forbids breaking away: still detached, if not from it.
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command.spawn().map(|_| ())
}
