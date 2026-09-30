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
}

struct Session {
    info: SessionInfo,
    to_worker: mpsc::Sender<ToServer>,
    attached: HashSet<ClientId>,
    build: String,
}

struct State {
    config: Config,
    events: mpsc::Sender<Event>,
    sessions: HashMap<SessionId, Session>,
    clients: HashMap<ClientId, mpsc::Sender<ToApp>>,
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
    let writer = tokio::spawn(async move {
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
    while let Ok(Some(payload)) = read.recv().await {
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
    async fn to_client(&self, client: ClientId, message: ToApp) {
        if let Some(tx) = self.clients.get(&client) {
            let _ = tx.send(message).await;
        }
    }

    async fn to_attached(&self, session: SessionId, message: ToApp) {
        let Some(entry) = self.sessions.get(&session) else {
            return;
        };
        for client in &entry.attached {
            if let Some(tx) = self.clients.get(client) {
                let _ = tx.send(message.clone()).await;
            }
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
                self.clients.insert(id, tx);
            }
            Event::ClientGone(id) => {
                self.clients.remove(&id);
                for session in self.sessions.values_mut() {
                    session.attached.remove(&id);
                }
            }
            Event::ClientSaid(id, message) => self.client_said(id, message).await,
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
                    self.to_client(client, ToApp::Created { session }).await;
                }
            }
            Event::WorkerFailed {
                session,
                reply_to,
                why,
            } => {
                self.starting = self.starting.saturating_sub(1);
                tracing::warn!(session, %why, "a worker did not start");
                if let Some(client) = reply_to {
                    self.to_client(
                        client,
                        ToApp::Refused {
                            what: format!("create session {session}"),
                            why,
                        },
                    )
                    .await;
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
                if let ToApp::Sessions(_) = message {
                    return;
                }
                self.to_attached(session, message).await;
                if ended {
                    self.sessions.remove(&session);
                }
            }
            Event::WorkerGone(session) => {
                // Gone without saying it exited: a crash. The tab says the
                // session ended.
                if self.sessions.contains_key(&session) {
                    self.to_attached(
                        session,
                        ToApp::Event {
                            session,
                            event: v1::Event::Exited { code: None },
                        },
                    )
                    .await;
                    self.sessions.remove(&session);
                }
            }
        }
    }

    async fn client_said(&mut self, client: ClientId, message: ToServer) {
        match message {
            ToServer::ListSessions => {
                let mut list: Vec<SessionInfo> =
                    self.sessions.values().map(|s| s.info.clone()).collect();
                list.sort_by_key(|s| s.id);
                self.to_client(client, ToApp::Sessions(list)).await;
            }
            ToServer::Create {
                name,
                cwd,
                cols,
                rows,
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
                };
                self.start_worker(launch, client);
            }
            ToServer::Attach {
                session,
                cols,
                rows,
            } => {
                let Some(entry) = self.sessions.get_mut(&session) else {
                    self.refuse(client, format!("attach {session}"), "no such session")
                        .await;
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
            ToServer::Input { session, .. }
            | ToServer::Kill { session }
            | ToServer::Confirmed { session, .. } => {
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

    async fn forward(&self, client: ClientId, session: SessionId, message: ToServer) {
        if !self.to_worker(session, message).await {
            self.refuse(client, format!("session {session}"), "no such session")
                .await;
        }
    }

    async fn refuse(&self, client: ClientId, what: String, why: &str) {
        self.to_client(
            client,
            ToApp::Refused {
                what,
                why: why.into(),
            },
        )
        .await;
    }

    fn start_worker(&self, launch: Launch, reply_to: ClientId) {
        let session = launch.session;
        match &self.config.launcher {
            Launcher::Process { exe } => {
                if let Err(error) = spawn_detached(exe, &launch.args()) {
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
pub fn spawn_detached(exe: &std::path::Path, args: &[String]) -> std::io::Result<()> {
    let mut command = std::process::Command::new(exe);
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
