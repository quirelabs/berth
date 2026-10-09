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

use berth_proto::v1::{self, SessionId, SessionInfo, ToApp, ToServer};
use tokio::sync::mpsc;

use super::link::{self, protocol};
use super::paths::{self, Places};
use super::transport::Listener;
use super::worker_host::{self, Launch};
use crate::error::{Error, Result};
use crate::worker::Shell;

/// How long a supervisor with no sessions and no clients stays up (D50).
pub const IDLE_EXIT: Duration = Duration::from_secs(60);

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
        info: SessionInfo,
        to_worker: mpsc::Sender<ToServer>,
        build: String,
        reply_to: Option<ClientId>,
    },
    WorkerFailed {
        session: SessionId,
        reply_to: Option<ClientId>,
        why: String,
    },
    WorkerSaid(SessionId, ToApp),
    WorkerGone(SessionId),
    /// A client that fell behind has emptied its queue.
    CaughtUp(ClientId),
}

struct Session {
    info: SessionInfo,
    to_worker: mpsc::Sender<ToServer>,
    attached: HashSet<ClientId>,
    build: String,
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
    behind: bool,
    /// Sessions whose next screen this client must get before their output.
    awaiting: HashSet<SessionId>,
}

struct State {
    config: Config,
    events: mpsc::Sender<Event>,
    sessions: HashMap<SessionId, Session>,
    clients: HashMap<ClientId, ClientQueue>,
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
    let (tx, mut rx) = mpsc::channel::<ToApp>(QUEUE);
    if events.send(Event::ClientUp(id, tx)).await.is_err() {
        return;
    }
    let mut write = opened.write;
    let mut writer = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            let Ok(payload) = message.encode() else {
                continue;
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
                match ToServer::decode(&payload) {
                    Ok(message) => {
                        if events.send(Event::ClientSaid(id, message)).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => {
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

/// The copy-of-the-binary name a build string carries: `berth 0.1.0 (name)`.
fn copy_name(build: &str) -> Option<String> {
    let start = build.rfind('(')?;
    let end = build.rfind(')')?;
    (start < end).then(|| build[start + 1..end].to_string())
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
            self.to_worker(
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

    async fn to_worker(&self, session: SessionId, message: ToServer) -> bool {
        match self.sessions.get(&session) {
            Some(entry) => entry.to_worker.send(message).await.is_ok(),
            None => false,
        }
    }

    async fn handle(&mut self, event: Event) {
        match event {
            Event::ClientUp(id, tx) => {
                self.clients.insert(
                    id,
                    ClientQueue {
                        tx,
                        behind: false,
                        awaiting: HashSet::new(),
                    },
                );
            }
            Event::ClientGone(id) => {
                self.clients.remove(&id);
                for session in self.sessions.values_mut() {
                    session.attached.remove(&id);
                }
            }
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
                        info,
                        to_worker,
                        attached: HashSet::new(),
                        build,
                    },
                );
                if let Some(client) = reply_to {
                    self.send_client(client, ToApp::Created { session });
                }
                self.push_sessions();
            }
            Event::WorkerFailed {
                session,
                reply_to,
                why,
            } => {
                self.starting = self.starting.saturating_sub(1);
                tracing::warn!(session, %why, "a worker did not start");
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
                            || entry.info.status != info.status)
                    {
                        entry.info.program = info.program;
                        entry.info.cwd = info.cwd;
                        entry.info.status = info.status;
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
                entry.info.cols = cols;
                entry.info.rows = rows;
                self.to_worker(session, message).await;
            }
            ToServer::Detach { session } => {
                if let Some(entry) = self.sessions.get_mut(&session) {
                    entry.attached.remove(&client);
                }
            }
            ToServer::Resize {
                session,
                cols,
                rows,
            } => {
                if let Some(entry) = self.sessions.get_mut(&session) {
                    entry.info.cols = cols;
                    entry.info.rows = rows;
                }
                self.forward(client, session, message).await;
            }
            ToServer::Input { session, .. } | ToServer::Kill { session } => {
                self.forward(client, session, message).await;
            }
            // Never waited on: a worker busy sending output may not be
            // reading, and waiting here would wait on the output this table
            // is the one to take (found 2026-10-02). Confirmations are
            // cumulative, so one dropped is carried by the next, and a worker
            // does not wait on a lost one for long.
            ToServer::Confirmed { session, .. } => {
                if let Some(entry) = self.sessions.get(&session) {
                    let _ = entry.to_worker.try_send(message);
                }
            }
            ToServer::SendHistory { session } => {
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
                    self.to_worker(session, message.clone()).await;
                }
            }
        }
    }

    async fn forward(&mut self, client: ClientId, session: SessionId, message: ToServer) {
        if !self.to_worker(session, message).await {
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
            let info = match describe(&mut opened).await {
                Ok(info) => info,
                Err(error) => {
                    let _ = events.send(failed(error.to_string())).await;
                    return;
                }
            };
            let (to_worker, mut outbox) = mpsc::channel::<ToServer>(QUEUE);
            let mut write = opened.write;
            tokio::spawn(async move {
                while let Some(message) = outbox.recv().await {
                    let Ok(payload) = message.encode() else {
                        continue;
                    };
                    if write.send(&payload).await.is_err() {
                        break;
                    }
                }
            });
            let build = opened.theirs.build.clone();
            if events
                .send(Event::WorkerUp {
                    info,
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
                        tracing::warn!(session, %error, "a worker sent something unreadable");
                        break;
                    }
                }
            }
            let _ = events.send(Event::WorkerGone(session)).await;
        });
    }

    /// Delete worker binaries that neither this build nor any running worker
    /// uses (D50).
    fn remove_unused_copies(&self) {
        let mut keep: HashSet<String> = self
            .sessions
            .values()
            .filter_map(|s| copy_name(&s.build))
            .collect();
        if let Some(own) = copy_name(&super::build()) {
            keep.insert(own);
        }
        if let Launcher::Process { exe } = &self.config.launcher
            && let Some(name) = exe.file_name()
        {
            keep.insert(name.to_string_lossy().to_string());
        }
        paths::remove_unused_copies(&self.config.places.bin, &keep);
    }
}

/// Ask a freshly reached worker what it is. Output it sends first is dropped:
/// whoever attaches gets a screen.
async fn describe<S>(opened: &mut link::Opened<S>) -> Result<SessionInfo>
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
        if let ToApp::Sessions(mut list) = ToApp::decode(&payload).map_err(protocol)?
            && let Some(info) = list.pop()
        {
            return Ok(info);
        }
    }
}

/// Start a process that outlives this one: its own process group on Unix,
/// detached and out of the job object on Windows (D49).
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
        command.process_group(0);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_copy_name_comes_out_of_a_build_string() {
        assert_eq!(
            copy_name("berth 0.1.0 (berth-0.1.0-ab12cd34)").as_deref(),
            Some("berth-0.1.0-ab12cd34")
        );
        assert_eq!(copy_name("berth 0.1.0"), None);
    }
}
