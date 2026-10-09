//! A worker as a process of its own: one session, on its own endpoint,
//! serving whichever supervisor connects.
//!
//! The worker never depends on the supervisor that started it. It keeps
//! reading its shell whether or not a supervisor is connected, so the shell
//! never blocks on a full pipe, and a supervisor that comes back gets the
//! screen as it is now rather than what it missed. A newer connection replaces
//! an older one: that is a restarted supervisor finding its workers again.

use std::path::PathBuf;

use berth_proto::v1::{self, SessionId, ToApp, ToServer};
use tokio::sync::mpsc;

use super::link::{self, protocol};
use super::paths::{self, Places};
use super::sync;
use super::transport::Listener;
use crate::error::{Error, Result};
use crate::worker::{Shell, Spec, Worker};

/// How often the flow is asked for a screen while it wants one.
const SCREEN_TICK: std::time::Duration = std::time::Duration::from_millis(10);

/// When a worker looks at what has its terminal: soon after Enter is typed,
/// once output has paused, never more often than every two seconds, and at
/// least every thirty, so an idle session wakes for it twice a minute.
const LOOK_AFTER_ENTER: std::time::Duration = std::time::Duration::from_millis(300);
const LOOK_AFTER_OUTPUT: std::time::Duration = std::time::Duration::from_millis(500);
const LOOK_AT_MOST_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
const LOOK_AT_LEAST_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

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
    // Claude's hook reports here rather than to the terminal (D50). Bound
    // before the shell starts, so its first hook already finds it.
    let mut hooks = Listener::bind(&places.hook(launch.session)).await?;
    if let Ok(bin) = std::env::current_exe() {
        pane.hook = Some((places.hook(launch.session), bin));
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
    let worker = std::sync::Arc::new(worker);
    let mut listener = Listener::bind(&places.worker(launch.session)).await?;
    let pid_file = places.worker_pid(launch.session);
    paths::write_pid(&pid_file)?;
    let _pid = PidFile(pid_file);
    tracing::info!(session = launch.session, "worker ready");

    // Messages from the current supervisor, tagged with its connection, so a
    // replaced one's stragglers are ignored.
    let (inbox_tx, mut inbox) = mpsc::channel::<(u64, ToServer)>(64);
    let mut connection = 0u64;
    let mut to_supervisor: ToSupervisor = None;
    let mut _reader: Option<Owned> = None;
    // What has the terminal is looked at when something happened, and the
    // session list told when it changed: the app names sessions by it.
    let mut look = Look::new();
    // Raw output or screens, as the app keeps up (D49).
    let mut flow = sync::Flow::new();
    let mut in_alternate = false;
    // A fixed-rate timer, not a sleep made afresh each time round the loop:
    // during a flood output arrives more often than the tick, and a fresh
    // sleep would never get to fire, leaving a pane that fell behind with no
    // screens until the flood ended. Only polled while the flow wants it, so
    // an idle session does not wake for it.
    let mut screen_tick = tokio::time::interval(SCREEN_TICK);
    screen_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let (hook_tx, mut hook_lines) = mpsc::channel::<String>(16);

    loop {
        tokio::select! {
            accepted = hooks.accept() => {
                let Ok(mut stream) = accepted else { continue };
                let tx = hook_tx.clone();
                tokio::spawn(async move {
                    if let Some(line) = super::hook::read_line(&mut stream).await {
                        let _ = tx.send(line).await;
                    }
                });
            }
            Some(line) = hook_lines.recv() => {
                if let Some(claude) = worker.hook_line(&line) {
                    send(
                        &mut to_supervisor,
                        &ToApp::Event {
                            session: launch.session,
                            event: v1::Event::Claude(claude),
                        },
                    )
                    .await;
                }
            }
            _ = tokio::time::sleep_until(look.next) => {
                look.looked();
                let looking = worker.clone();
                let changed = tokio::task::spawn_blocking(move || looking.refresh())
                    .await
                    .ok()
                    .flatten();
                if let Some(info) = changed {
                    send(&mut to_supervisor, &ToApp::Sessions(vec![info])).await;
                }
            }
            message = output.recv() => {
                let Some(message) = message else { break };
                if let ToApp::Output { seq, bytes, .. } = &message {
                    look.output();
                    // A program leaving the alternate screen gets a screen,
                    // so what lies underneath is right (D49).
                    let alternate = worker.alternate_screen();
                    if in_alternate && !alternate {
                        flow.left_alternate_screen();
                    }
                    in_alternate = alternate;
                    if flow.output(*seq, bytes.len() as u64) == sync::Output::Drop {
                        continue;
                    }
                }
                send(&mut to_supervisor, &message).await;
            }
            _ = screen_tick.tick(), if flow.wants_tick() => {
                if let Some(kind) = flow.tick(std::time::Instant::now()) {
                    send_screen(&worker, &mut flow, kind, &mut to_supervisor).await;
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
                if let ToServer::Input { bytes, .. } = &message
                    && bytes.iter().any(|b| *b == b'\r' || *b == b'\n')
                {
                    look.enter();
                }
                match message {
                    ToServer::Confirmed { seq, .. } => flow.confirmed(seq),
                    ToServer::Visible { visible } => {
                        if let Some(kind) = flow.visible(visible) {
                            send_screen(&worker, &mut flow, kind, &mut to_supervisor).await;
                        }
                    }
                    ToServer::Attach { .. } => {
                        let _ = worker.handle(message).await;
                        let kind = flow.attached();
                        send_screen(&worker, &mut flow, kind, &mut to_supervisor).await;
                        // What Claude is doing, for an app that was not here
                        // when it last said.
                        if let Some(claude) = worker.claude() {
                            send(
                                &mut to_supervisor,
                                &ToApp::Event {
                                    session: launch.session,
                                    event: v1::Event::Claude(claude),
                                },
                            )
                            .await;
                        }
                    }
                    message => {
                        if let Some(reply) = worker.handle(message).await {
                            send(&mut to_supervisor, &reply).await;
                        }
                    }
                }
            }
        }
    }
    tracing::info!(session = launch.session, "worker's shell ended");
    Ok(())
}

/// When to look next at what has the terminal.
struct Look {
    next: tokio::time::Instant,
    last: tokio::time::Instant,
}

impl Look {
    fn new() -> Self {
        let now = tokio::time::Instant::now();
        Self {
            next: now,
            last: now,
        }
    }

    /// The earliest a look may come: not sooner than the floor allows.
    fn earliest(&self, after: std::time::Duration) -> tokio::time::Instant {
        (tokio::time::Instant::now() + after).max(self.last + LOOK_AT_MOST_EVERY)
    }

    /// Enter was typed: a program may have started. Look soon.
    fn enter(&mut self) {
        self.next = self.next.min(self.earliest(LOOK_AFTER_ENTER));
    }

    /// Output brings a look to just after it pauses: each chunk moves it
    /// to half a second after itself, so a burst is looked at once, at its
    /// end.
    fn output(&mut self) {
        self.next = self.earliest(LOOK_AFTER_OUTPUT);
    }

    fn looked(&mut self) {
        self.last = tokio::time::Instant::now();
        self.next = self.last + LOOK_AT_LEAST_EVERY;
    }
}

type ToSupervisor = Option<super::link::FrameWrite<tokio::io::WriteHalf<super::transport::Stream>>>;

/// Send to the supervisor, if one is connected. A failed send forgets it; the
/// next supervisor to connect gets a screen.
async fn send(to_supervisor: &mut ToSupervisor, message: &ToApp) {
    let Some(write) = to_supervisor else {
        return;
    };
    let sent = match message.encode() {
        Ok(payload) => write.send(&payload).await,
        Err(error) => Err(protocol(error)),
    };
    if sent.is_err() {
        *to_supervisor = None;
    }
}

/// Send the screen as it is now, and with it, when asked, every history line
/// above it. Taken off the async threads: both wait on the terminal.
async fn send_screen(
    worker: &std::sync::Arc<Worker>,
    flow: &mut sync::Flow,
    kind: sync::Screen,
    to_supervisor: &mut ToSupervisor,
) {
    let with_history = kind == sync::Screen::WithHistory;
    let taking = worker.clone();
    let Ok((screen, first_line)) =
        tokio::task::spawn_blocking(move || taking.screen(with_history)).await
    else {
        return;
    };
    let seq = match &screen {
        ToApp::Screen { seq, .. } => *seq,
        _ => 0,
    };
    send(to_supervisor, &screen).await;
    flow.screen_sent(seq, kind, std::time::Instant::now());
    if let (true, Some(first_line)) = (with_history, first_line) {
        let reading = worker.clone();
        if let Ok(ranges) =
            tokio::task::spawn_blocking(move || reading.history_below(first_line)).await
        {
            for range in &ranges {
                send(to_supervisor, range).await;
            }
        }
    }
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
