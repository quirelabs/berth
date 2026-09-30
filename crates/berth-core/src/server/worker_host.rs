//! A worker as a process of its own: one session, on its own endpoint,
//! serving whichever supervisor connects.
//!
//! The worker never depends on the supervisor that started it. It keeps
//! reading its shell whether or not a supervisor is connected, so the shell
//! never blocks on a full pipe, and a supervisor that comes back gets the
//! screen as it is now rather than what it missed. A newer connection replaces
//! an older one: that is a restarted supervisor finding its workers again.

use std::path::PathBuf;

use berth_proto::v1::{SessionId, ToServer};
use tokio::sync::mpsc;

use super::link::{self, protocol};
use super::paths::{self, Places};
use super::transport::Listener;
use crate::error::{Error, Result};
use crate::worker::{Shell, Spec, Worker};

/// What a worker process is started with.
#[derive(Debug, Clone)]
pub struct Launch {
    pub session: SessionId,
    pub name: String,
    pub cwd: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
    /// Tests name a plain shell; a real session runs the login shell.
    pub shell: Option<Shell>,
    /// The pane's Claude token and a conversation to resume (D48). Passed to
    /// a worker process in its environment, never its arguments.
    pub pane: Option<String>,
    pub resume: Option<String>,
}

impl Launch {
    /// As arguments for `berth --worker`.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            "--worker".into(),
            self.session.to_string(),
            "--name".into(),
            self.name.clone(),
            "--size".into(),
            format!("{}x{}", self.cols, self.rows),
        ];
        if let Some(cwd) = &self.cwd {
            args.push("--cwd".into());
            args.push(cwd.to_string_lossy().to_string());
        }
        args
    }

    /// From the arguments after the program name, as [`Launch::args`] wrote
    /// them.
    pub fn parse(args: &[String]) -> Option<Self> {
        let mut launch = Launch {
            session: 0,
            name: String::new(),
            cwd: None,
            cols: 80,
            rows: 24,
            shell: None,
            pane: crate::env::worker_pane(),
            resume: crate::env::worker_resume(),
        };
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--worker" => launch.session = it.next()?.parse().ok()?,
                "--name" => launch.name = it.next()?.clone(),
                "--cwd" => launch.cwd = Some(PathBuf::from(it.next()?)),
                "--size" => {
                    let (cols, rows) = it.next()?.split_once('x')?;
                    launch.cols = cols.parse().ok()?;
                    launch.rows = rows.parse().ok()?;
                }
                _ => return None,
            }
        }
        (launch.session != 0).then_some(launch)
    }
}

/// A task that ends with the value holding it, so a connection's reader never
/// outlives the worker and holds its socket open.
struct Owned(tokio::task::JoinHandle<()>);

impl Drop for Owned {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Removes the pid file when the worker ends, however it ends.
struct PidFile(PathBuf);

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Run one session until its shell exits.
pub async fn serve(places: Places, launch: Launch) -> Result<()> {
    let mut pane = crate::claude::PaneEnv::new(crate::claude::base_dir().as_deref());
    if let Some(token) = launch
        .pane
        .as_deref()
        .and_then(crate::claude::PaneToken::parse)
    {
        pane.token = token;
    }
    if let Some(session) = &launch.resume {
        pane = pane.resuming(session);
    }
    let (worker, mut output) = Worker::start(Spec {
        session: launch.session,
        name: launch.name.clone(),
        cwd: launch.cwd.clone(),
        cols: launch.cols,
        rows: launch.rows,
        pane: Some(pane),
        shell: launch.shell.clone(),
    })
    .await?;
    let mut listener = Listener::bind(&places.worker(launch.session)).await?;
    let pid_file = places.worker_pid(launch.session);
    paths::write_pid(&pid_file)?;
    let _pid = PidFile(pid_file);
    tracing::info!(session = launch.session, "worker ready");

    // Messages from the current supervisor, tagged with its connection, so a
    // replaced one's stragglers are ignored.
    let (inbox_tx, mut inbox) = mpsc::channel::<(u64, ToServer)>(64);
    let mut connection = 0u64;
    let mut to_supervisor = None;
    let mut _reader: Option<Owned> = None;

    loop {
        tokio::select! {
            message = output.recv() => {
                let Some(message) = message else { break };
                if let Some(write) = &mut to_supervisor {
                    let sent = match message.encode() {
                        Ok(payload) => super::link::FrameWrite::send(write, &payload).await,
                        Err(e) => Err(protocol(e)),
                    };
                    if sent.is_err() {
                        to_supervisor = None;
                    }
                }
            }
            accepted = listener.accept() => {
                let Ok(stream) = accepted else { continue };
                let Ok(opened) = link::open(stream, &link::hello()).await else { continue };
                connection += 1;
                to_supervisor = Some(opened.write);
                let tx = inbox_tx.clone();
                let tag = connection;
                let mut read = opened.read;
                _reader = Some(Owned(tokio::spawn(async move {
                    while let Ok(Some(payload)) = read.recv().await {
                        match ToServer::decode(&payload) {
                            Ok(message) => {
                                if tx.send((tag, message)).await.is_err() {
                                    break;
                                }
                            }
                            Err(error) => {
                                tracing::warn!(%error, "a supervisor sent something unreadable");
                                break;
                            }
                        }
                    }
                })));
            }
            Some((tag, message)) = inbox.recv() => {
                if tag != connection {
                    continue;
                }
                if let Some(reply) = worker.handle(message).await
                    && let Some(write) = &mut to_supervisor
                {
                    let payload = reply.encode().map_err(protocol)?;
                    if write.send(&payload).await.is_err() {
                        to_supervisor = None;
                    }
                }
            }
        }
    }
    tracing::info!(session = launch.session, "worker's shell ended");
    Ok(())
}

/// `berth --worker ...`: a worker process, as the supervisor starts one.
pub async fn run_process(args: &[String]) -> Result<()> {
    let launch = Launch::parse(args)
        .ok_or_else(|| Error::Server(format!("bad worker arguments: {args:?}")))?;
    let places = Places::standard()
        .ok_or_else(|| Error::Server("no data directory for the runtime files".into()))?;
    places.prepare()?;
    serve(places, launch).await
}

/// Connect to a worker's endpoint, waiting for it to come up.
pub async fn reach(places: &Places, session: SessionId) -> Result<super::transport::Stream> {
    let endpoint = places.worker(session);
    let mut last = None;
    for _ in 0..100 {
        match super::transport::connect(&endpoint).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Err(Error::Server(format!(
        "worker {session} did not come up: {}",
        last.map(|e| e.to_string()).unwrap_or_default()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_arguments_round_trip() {
        let launch = Launch {
            session: 12,
            name: "my shell".into(),
            cwd: Some(PathBuf::from("/tmp/a dir")),
            cols: 120,
            rows: 40,
            shell: None,
            pane: None,
            resume: None,
        };
        let back = Launch::parse(&launch.args()).unwrap();
        assert_eq!(back.session, 12);
        assert_eq!(back.name, "my shell");
        assert_eq!(back.cwd, launch.cwd);
        assert_eq!((back.cols, back.rows), (120, 40));
    }

    #[test]
    fn nonsense_arguments_are_refused() {
        assert!(Launch::parse(&["--worker".into(), "x".into()]).is_none());
        assert!(Launch::parse(&["--worker".into(), "3".into(), "--evil".into()]).is_none());
        assert!(Launch::parse(&[]).is_none());
    }
}
