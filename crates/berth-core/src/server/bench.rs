//! The server speed floor (D49, D50): how fast a session's output gets
//! through the whole server path, a pty, a worker parsing it, the supervisor
//! and a client confirming it, per kind of output.
//!
//! D49 set the floor at twice tmux's rate per kind of output, measured on
//! `berth-test`: 28 MB/s plain, 32 MB/s coloured, 24 MB/s emoji. A run prints
//! each kind's rate and fails if any is under its floor.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use berth_proto::v1::{self, SessionId, ToApp, ToServer};

use super::client;
use super::paths::{self, Places};
use super::supervisor::{self, Config, Launcher};
use crate::error::{Error, Result};
use crate::worker::Shell;

const MB: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Coloured,
    Emoji,
    Cjk,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Plain, Kind::Coloured, Kind::Emoji, Kind::Cjk];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Plain => "plain",
            Kind::Coloured => "coloured",
            Kind::Emoji => "emoji",
            Kind::Cjk => "cjk",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }

    /// The floor in MB/s, where D49 set one.
    pub fn floor(self) -> Option<f64> {
        match self {
            Kind::Plain => Some(28.0),
            Kind::Coloured => Some(32.0),
            Kind::Emoji => Some(24.0),
            Kind::Cjk => None,
        }
    }

    /// One block of lines, repeated to the size asked for. The same shapes
    /// the D49 spike measured the engine with.
    fn block(self) -> Vec<u8> {
        let mut out = Vec::new();
        for n in 0..200 {
            let line = match self {
                Kind::Plain => {
                    format!("{n:>6} the quick brown fox jumps over the lazy dog, again\n")
                }
                Kind::Coloured => format!(
                    "\x1b[38;5;{}m{n:>6}\x1b[0m \x1b[1mthe quick\x1b[0m brown \x1b[3;4mfox\x1b[0m jumps\n",
                    n % 256
                ),
                Kind::Emoji => format!(
                    "{n:>6} \u{1f600} \u{1f44d}\u{1f3fd} \u{1f1f3}\u{1f1f1} e\u{301} a\u{308} combining and emoji\n"
                ),
                Kind::Cjk => format!(
                    "{n:>6} \u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30c6}\u{30ad}\u{30b9}\u{30c8} \u{4e2d}\u{6587} \u{d55c}\u{ad6d}\u{c5b4} wide text\n"
                ),
            };
            out.extend_from_slice(line.as_bytes());
        }
        out
    }
}

/// Write `megabytes` of a kind of output to `path`.
pub fn write_stream(kind: Kind, path: &Path, megabytes: usize) -> std::io::Result<usize> {
    let block = kind.block();
    let mut out = Vec::with_capacity(megabytes * MB + block.len());
    while out.len() < megabytes * MB {
        out.extend_from_slice(&block);
    }
    std::fs::write(path, &out)?;
    Ok(out.len())
}

#[derive(Debug, Clone)]
pub struct Rate {
    pub kind: Kind,
    pub bytes: usize,
    pub seconds: f64,
}

impl Rate {
    pub fn mb_per_s(&self) -> f64 {
        self.bytes as f64 / MB as f64 / self.seconds
    }

    pub fn passes(&self) -> bool {
        self.kind
            .floor()
            .is_none_or(|floor| self.mb_per_s() >= floor)
    }
}

/// The next message, confirmed as an app confirms it.
async fn next(app: &mut client::Client) -> Result<ToApp> {
    let message = tokio::time::timeout(Duration::from_secs(60), app.from_server.recv())
        .await
        .map_err(|_| Error::Server("the server went quiet".into()))?
        .ok_or_else(|| Error::Server("the server closed".into()))?;
    if let ToApp::Output { session, seq, .. } | ToApp::Screen { session, seq, .. } = &message {
        let _ = app
            .to_server
            .send(ToServer::Confirmed {
                session: *session,
                seq: *seq,
            })
            .await;
    }
    Ok(message)
}

fn shows(message: &ToApp, needle: &str, tail: &mut Vec<u8>) -> bool {
    match message {
        ToApp::Output { bytes, .. } => {
            tail.extend_from_slice(bytes);
            let found = tail.windows(needle.len()).any(|w| w == needle.as_bytes());
            let keep = needle.len() - 1;
            if tail.len() > keep {
                tail.drain(..tail.len() - keep);
            }
            found
        }
        ToApp::Screen { screen, .. } => screen.unpack::<v1::Screen>().is_ok_and(|s| {
            s.lines
                .iter()
                .flat_map(|r| r.cells.iter().map(|c| c.text.as_str()))
                .collect::<String>()
                .contains(needle)
        }),
        _ => false,
    }
}

/// How the server under measure runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Servers {
    /// A supervisor and its workers as tasks in this process, over the same
    /// sockets. For tests: quick, and needs no binary of its own.
    InProcess,
    /// A supervisor process started from a versioned copy of this binary, as
    /// the app starts it, starting worker processes for its sessions: what
    /// ships. Unix only.
    Processes,
}

/// CPU time and page faults of one process, from `/proc` (Linux only).
#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub user: f64,
    pub system: f64,
    pub minor_faults: u64,
    pub major_faults: u64,
}

impl Usage {
    /// `/proc/<pid>/stat`, where the kernel has one; `<pid>/task/<tid>` for
    /// one thread.
    fn of(pid: &str) -> Option<Self> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name may hold spaces; the fields after it do not.
        let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
        // Counted from the state field (stat(5)'s field 3) as 0. Ticks are
        // USER_HZ, which Linux fixes at 100 for every architecture.
        let number = |i: usize| fields.get(i).and_then(|f| f.parse::<u64>().ok());
        Some(Self {
            minor_faults: number(7)?,
            major_faults: number(9)?,
            user: number(11)? as f64 / 100.0,
            system: number(12)? as f64 / 100.0,
        })
    }

    fn since(self, before: Self) -> Self {
        Self {
            user: self.user - before.user,
            system: self.system - before.system,
            minor_faults: self.minor_faults.saturating_sub(before.minor_faults),
            major_faults: self.major_faults.saturating_sub(before.major_faults),
        }
    }
}

/// One kind's rate, and what each process spent on it where that can be told.
#[derive(Debug, Clone)]
pub struct Report {
    pub rate: Rate,
    pub usage: Vec<(&'static str, Usage)>,
    pub delivered: Delivered,
}

/// What reached the client: the output as it came, and screens in its place
/// while the client fell behind (D50's sync).
#[derive(Debug, Clone, Copy, Default)]
pub struct Delivered {
    pub output: usize,
    pub screens: usize,
    /// How raw output ran up to the first screen, if one came.
    pub switch: Option<Switch>,
}

/// The run-up to the worker's switch from raw output to screens. The worker
/// switches when, as it sends, confirmations trail by more than
/// [`super::sync::BEHIND`]. The client confirms each chunk as it arrives, so
/// the time it took to receive that last window bounds the round trip the
/// confirmations had: far under a millisecond, the sender outran any round
/// trip; tens of milliseconds, confirmations were held up on their way back.
#[derive(Debug, Clone, Copy)]
pub struct Switch {
    /// Since the command went, when the last raw output arrived.
    pub at_ms: f64,
    /// Raw output received before it, in bytes.
    pub raw: u64,
    /// How long the last window of raw output took to arrive.
    pub last_window_ms: f64,
    /// The longest wait between two raw chunks.
    pub longest_gap_ms: f64,
}

impl Switch {
    fn of(arrivals: &[(Instant, u64)], started: Instant) -> Option<Self> {
        let &(last_at, last_seq) = arrivals.last()?;
        let &(first_at, first_seq) = arrivals.first()?;
        let window_start = last_seq.saturating_sub(super::sync::BEHIND);
        let window_at = arrivals
            .iter()
            .find(|(_, seq)| *seq >= window_start)
            .map_or(first_at, |(at, _)| *at);
        let longest_gap = arrivals
            .windows(2)
            .map(|w| w[1].0.duration_since(w[0].0))
            .max()
            .unwrap_or_default();
        Some(Self {
            at_ms: last_at.duration_since(started).as_secs_f64() * 1000.0,
            raw: last_seq - first_seq,
            last_window_ms: last_at.duration_since(window_at).as_secs_f64() * 1000.0,
            longest_gap_ms: longest_gap.as_secs_f64() * 1000.0,
        })
    }
}

/// The processes worth accounting for, by role, as pids.
fn roles(places: &Places, session: SessionId, servers: Servers) -> Vec<(&'static str, String)> {
    let mut roles = vec![("client", "self".to_string())];
    if servers == Servers::Processes {
        if let Some(pid) = paths::read_pid(&places.supervisor_pid()) {
            roles.push(("supervisor", pid.to_string()));
        }
        if let Some(pid) = paths::read_pid(&places.worker_pid(session)) {
            roles.push(("worker", pid.to_string()));
            // Its two named threads: the pty's reader and the engine. The
            // rest of the worker's time is its runtime's threads.
            for (thread, role) in [
                ("berth-worker", "  pty read"),
                ("berth-terminal", "  engine"),
            ] {
                if let Some(tid) = thread_named(&pid.to_string(), thread) {
                    roles.push((role, format!("{pid}/task/{tid}")));
                }
            }
        }
    }
    roles
}

/// The thread of a process with this name, from `/proc` (Linux only).
fn thread_named(pid: &str, name: &str) -> Option<String> {
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .ok()?
        .flatten()
        .find(|task| {
            std::fs::read_to_string(task.path().join("comm")).is_ok_and(|comm| comm.trim() == name)
        })
        .map(|task| task.file_name().to_string_lossy().to_string())
}

fn snapshot(roles: &[(&'static str, String)]) -> Vec<(&'static str, Option<Usage>)> {
    roles
        .iter()
        .map(|(role, pid)| (*role, Usage::of(pid)))
        .collect()
}

/// Start the server for a run, and how to stop it afterwards.
async fn start_servers(places: &Places, scratch: &Path, servers: Servers) -> Result<Running> {
    match servers {
        Servers::InProcess => Ok(Running::Task(tokio::spawn(supervisor::run(Config {
            places: places.clone(),
            launcher: Launcher::InProcess {
                shell: Some(Shell {
                    program: "/bin/sh".into(),
                    login: false,
                }),
                tasks: Default::default(),
            },
            idle: Duration::from_secs(60),
        })))),
        Servers::Processes => {
            places.prepare()?;
            let copy = paths::install_copy(&places.bin, &std::env::current_exe()?)?;
            // The supervisor and the workers it starts find their places from
            // this, as tests move them.
            let data = scratch.to_string_lossy().to_string();
            supervisor::spawn_detached(
                &copy,
                &["--supervisor".to_string()],
                &[(crate::env::DATA, Some(data))],
            )?;
            Ok(Running::Process)
        }
    }
}

enum Running {
    Task(tokio::task::JoinHandle<Result<()>>),
    Process,
}

impl Running {
    fn stop(self, places: &Places) {
        match self {
            Running::Task(task) => task.abort(),
            Running::Process => {
                // Its sessions are killed already; without clients it would
                // idle a minute before going.
                if let Some(pid) = paths::read_pid(&places.supervisor_pid()) {
                    let _ = std::process::Command::new("kill")
                        .arg(pid.to_string())
                        .status();
                }
            }
        }
    }
}

/// One kind's rate through a supervisor and a worker.
pub async fn measure(
    kind: Kind,
    megabytes: usize,
    scratch: &Path,
    servers: Servers,
) -> Result<Report> {
    let places = Places::under(scratch);
    let file = scratch.join(format!("{}.txt", kind.name()));
    let bytes = write_stream(kind, &file, megabytes)?;
    let running = start_servers(&places, scratch, servers).await?;
    let result = async {
        let mut app = None;
        for _ in 0..200 {
            if let Ok(connected) = client::connect(&places).await {
                app = Some(connected);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut app = app.ok_or_else(|| Error::Server("no supervisor".into()))?;
        app.to_server
            .send(ToServer::Create {
                name: None,
                cwd: Some(scratch.to_string_lossy().to_string()),
                cols: 100,
                rows: 30,
                pane: None,
                resume: None,
            })
            .await
            .map_err(|_| Error::Server("closed".into()))?;
        let session = loop {
            if let ToApp::Created { session } = next(&mut app).await? {
                break session;
            }
        };
        app.to_server
            .send(ToServer::Attach {
                session,
                cols: 100,
                rows: 30,
            })
            .await
            .map_err(|_| Error::Server("closed".into()))?;
        loop {
            if let ToApp::History { complete: true, .. } = next(&mut app).await? {
                break;
            }
        }
        // Let a login shell finish starting before the clock does.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let roles = roles(&places, session, servers);
        let before = snapshot(&roles);
        let command = format!("cat '{}'; echo BENCH_DO\"\"NE\n", file.display());
        let started = Instant::now();
        app.to_server
            .send(ToServer::Input {
                session,
                bytes: command.into_bytes(),
            })
            .await
            .map_err(|_| Error::Server("closed".into()))?;
        let mut tail = Vec::new();
        let mut delivered = Delivered::default();
        // Raw output as it arrived, until the first screen: when and how far.
        let mut arrivals: Vec<(Instant, u64)> = Vec::new();
        loop {
            let message = next(&mut app).await?;
            match &message {
                ToApp::Output { bytes, seq, .. } => {
                    delivered.output += bytes.len();
                    if delivered.screens == 0 {
                        arrivals.push((Instant::now(), *seq));
                    }
                }
                ToApp::Screen { .. } => {
                    if delivered.screens == 0 {
                        delivered.switch = Switch::of(&arrivals, started);
                    }
                    delivered.screens += 1;
                }
                _ => {}
            }
            if shows(&message, "BENCH_DONE", &mut tail) {
                break;
            }
        }
        let seconds = started.elapsed().as_secs_f64();
        let after = snapshot(&roles);
        let usage = before
            .into_iter()
            .zip(after)
            .filter_map(|((role, before), (_, after))| Some((role, after?.since(before?))))
            .collect();
        let _ = app.to_server.send(ToServer::Kill { session }).await;
        // Give the worker the moment it needs to go before its supervisor.
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(Report {
            rate: Rate {
                kind,
                bytes,
                seconds,
            },
            usage,
            delivered,
        })
    }
    .await;
    running.stop(&places);
    let _ = std::fs::remove_file(&file);
    result
}

/// `--bench-throughput [kind ...] [--mb N] [--in-process] [--write DIR]`:
/// each kind's rate, against its floor, and on Linux each process's CPU time
/// and page faults. Fails if any rate is under its floor. The server runs as
/// processes, as it ships, unless `--in-process`. With `--write`, only writes
/// the streams into DIR instead, so tmux can be timed on the same bytes.
pub async fn run_process(args: &[String]) -> Result<bool> {
    let mut kinds = Vec::new();
    let mut megabytes = 64;
    let mut write = None;
    let mut servers = if cfg!(unix) {
        Servers::Processes
    } else {
        Servers::InProcess
    };
    let mut it = args.iter().skip(1);
    while let Some(arg) = it.next() {
        if arg == "--mb" {
            megabytes = it
                .next()
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| Error::Server("--mb needs a number".into()))?;
        } else if arg == "--in-process" {
            servers = Servers::InProcess;
        } else if arg == "--write" {
            write =
                Some(PathBuf::from(it.next().ok_or_else(|| {
                    Error::Server("--write needs a directory".into())
                })?));
        } else {
            kinds.push(
                Kind::parse(arg).ok_or_else(|| Error::Server(format!("unknown kind {arg}")))?,
            );
        }
    }
    if kinds.is_empty() {
        kinds = Kind::ALL.to_vec();
    }
    if let Some(dir) = write {
        std::fs::create_dir_all(&dir)?;
        for kind in kinds {
            let path = dir.join(format!("{}.txt", kind.name()));
            write_stream(kind, &path, megabytes)?;
            println!("{}", path.display());
        }
        return Ok(true);
    }
    let scratch: PathBuf = std::env::temp_dir().join(format!("berth-bench-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;
    let mut all = true;
    for kind in kinds {
        match measure(kind, megabytes, &scratch, servers).await {
            Ok(Report {
                rate,
                usage,
                delivered,
            }) => {
                let verdict = match kind.floor() {
                    Some(floor) if rate.passes() => format!("PASS (floor {floor:.0})"),
                    Some(floor) => format!("FAIL (floor {floor:.0})"),
                    None => "no floor".to_string(),
                };
                println!(
                    "{:<9} {:>6.1} MB/s  {} MB in {:.2} s  {verdict}",
                    kind.name(),
                    rate.mb_per_s(),
                    rate.bytes / MB,
                    rate.seconds
                );
                println!(
                    "  delivered  {:.1} MB as output, {} screens",
                    delivered.output as f64 / MB as f64,
                    delivered.screens
                );
                if let Some(switch) = delivered.switch {
                    println!(
                        "  switched   at {:.1} ms after {:.2} MB raw; last 256 KB took {:.2} ms, longest gap {:.2} ms",
                        switch.at_ms,
                        switch.raw as f64 / MB as f64,
                        switch.last_window_ms,
                        switch.longest_gap_ms
                    );
                }
                for (role, used) in usage {
                    println!(
                        "  {role:<10} user {:>5.2} s  sys {:>5.2} s  faults {:>8} minor {:>4} major",
                        used.user, used.system, used.minor_faults, used.major_faults
                    );
                }
                all &= rate.passes();
            }
            Err(error) => {
                println!("{:<9} did not finish: {error}", kind.name());
                all = false;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(all)
}

/// One rung of the engine ladder: a rate, and what this process spent on it.
pub struct Rung {
    pub name: &'static str,
    pub seconds: f64,
    pub usage: Option<Usage>,
}

/// The same bytes through each layer between the spike's measure and the
/// worker's terminal, in this process: libghostty alone with the spike's
/// limits, alone with the worker's, then berth-term's terminal headless (as
/// the worker runs it) and drawn (as the app does). The output as the pty
/// hands it over, with `\r\n` line ends.
pub fn ladder(kind: Kind, megabytes: usize) -> Vec<Rung> {
    use berth_term::{Scrollback, TermSize, Terminal, engine};
    let mut bytes = Vec::new();
    let mut plain = Vec::new();
    let block = kind.block();
    while plain.len() < megabytes * MB {
        plain.extend_from_slice(&block);
    }
    for &b in &plain {
        if b == b'\n' {
            bytes.push(b'\r');
        }
        bytes.push(b);
    }
    let size = TermSize::new(100, 30);
    let spike = Scrollback {
        lines: Some(10_000),
        bytes: None,
    };
    let worker = crate::worker::SCROLLBACK;
    let timed = |name: &'static str, run: &dyn Fn() -> Option<Duration>| {
        let before = Usage::of("self");
        let seconds = run().map(|d| d.as_secs_f64()).unwrap_or(f64::NAN);
        let usage = Usage::of("self").zip(before).map(|(a, b)| a.since(b));
        Rung {
            name,
            seconds,
            usage,
        }
    };
    let through = |terminal: Terminal| {
        let started = Instant::now();
        let mut last = 0;
        for chunk in bytes.chunks(8192) {
            last = terminal.advance(chunk);
            terminal.wait_for_room(512 * 1024);
        }
        while terminal.snapshot().seq < last {}
        Some(started.elapsed())
    };
    let mut rungs = vec![
        timed("alone, spike limits", &|| {
            engine::time_parse_alone(&bytes, size, spike)
        }),
        timed("alone, worker limits", &|| {
            engine::time_parse_alone(&bytes, size, worker)
        }),
        timed("headless terminal", &|| {
            through(Terminal::headless(size, worker))
        }),
        timed("drawn terminal", &|| {
            through(Terminal::with_scrollback(
                size,
                berth_term::CellSize::new(8.0, 16.0),
                worker,
            ))
        }),
    ];
    // The engine thread's own work, one step at a time, on this thread.
    let steps = engine::time_parse_steps(&bytes, size, worker);
    rungs.extend(steps.into_iter().map(|(name, took)| Rung {
        name,
        seconds: took.map_or(f64::NAN, |d| d.as_secs_f64()),
        usage: None,
    }));
    rungs
}

/// The worker on its own: a shell in a pty running `cat`, the pump and the
/// headless engine, as a worker runs them, with its output read and dropped
/// here instead of going to a supervisor. Between the ladder's headless
/// terminal and the whole server path: what the pty and `cat` add.
pub async fn worker_alone(kind: Kind, megabytes: usize, scratch: &Path) -> Result<Rung> {
    let file = scratch.join(format!("{}.txt", kind.name()));
    write_stream(kind, &file, megabytes)?;
    let (worker, mut output) = crate::worker::Worker::start(crate::worker::Spec {
        session: 1,
        name: "bench".into(),
        cwd: Some(scratch.to_path_buf()),
        cols: 100,
        rows: 30,
        pane: None,
        shell: Some(Shell {
            program: "/bin/sh".into(),
            login: false,
        }),
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    while output.try_recv().is_ok() {}
    let engine = thread_named("self", "berth-terminal").map(|tid| format!("self/task/{tid}"));
    let before = engine.as_deref().and_then(Usage::of);
    let started = Instant::now();
    worker
        .handle(ToServer::Input {
            session: 1,
            bytes: format!("cat '{}'; echo BENCH_DO\"\"NE\n", file.display()).into_bytes(),
        })
        .await;
    let mut tail = Vec::new();
    loop {
        let message = tokio::time::timeout(Duration::from_secs(60), output.recv())
            .await
            .map_err(|_| Error::Server("the worker went quiet".into()))?
            .ok_or_else(|| Error::Server("the worker ended".into()))?;
        if shows(&message, "BENCH_DONE", &mut tail) {
            break;
        }
    }
    let seconds = started.elapsed().as_secs_f64();
    let usage = engine
        .as_deref()
        .and_then(Usage::of)
        .zip(before)
        .map(|(a, b)| a.since(b));
    let _ = std::fs::remove_file(&file);
    Ok(Rung {
        name: "worker, no server",
        seconds,
        usage,
    })
}

/// `--bench-engine [kind ...] [--mb N]`: [`ladder`] for each kind, then
/// [`worker_alone`].
pub async fn run_engine(args: &[String]) -> Result<bool> {
    let mut kinds = Vec::new();
    let mut megabytes = 64;
    let mut it = args.iter().skip(1);
    while let Some(arg) = it.next() {
        if arg == "--mb" {
            megabytes = it
                .next()
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| Error::Server("--mb needs a number".into()))?;
        } else {
            kinds.push(
                Kind::parse(arg).ok_or_else(|| Error::Server(format!("unknown kind {arg}")))?,
            );
        }
    }
    if kinds.is_empty() {
        kinds = Kind::ALL.to_vec();
    }
    let scratch: PathBuf = std::env::temp_dir().join(format!("berth-bench-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;
    for kind in kinds {
        println!("{}", kind.name());
        let mut rungs = ladder(kind, megabytes);
        match worker_alone(kind, megabytes, &scratch).await {
            Ok(rung) => rungs.push(rung),
            Err(error) => println!("  worker, no server: did not finish: {error}"),
        }
        for rung in rungs {
            let rate = megabytes as f64 / rung.seconds;
            match rung.usage {
                Some(used) => println!(
                    "  {:<21} {rate:>6.1} MB/s  user {:>5.2} s  sys {:>5.2} s  faults {:>8}",
                    rung.name, used.user, used.system, used.minor_faults
                ),
                None => println!("  {:<21} {rate:>6.1} MB/s", rung.name),
            }
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(true)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn streams_are_the_size_asked_and_the_right_shape() {
        let dir = tempfile::tempdir().unwrap();
        for kind in Kind::ALL {
            let path = dir.path().join(kind.name());
            let written = write_stream(kind, &path, 1).unwrap();
            assert!((MB..MB + 64 * 1024).contains(&written));
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.lines().count() > 1000);
        }
    }

    /// A small run through the whole path finishes and reports a rate.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_small_run_reports_a_rate() {
        let dir = tempfile::tempdir().unwrap();
        let report = measure(Kind::Plain, 2, dir.path(), Servers::InProcess)
            .await
            .unwrap();
        assert!(report.rate.mb_per_s() > 0.0);
        assert!(report.rate.bytes >= 2 * MB);
    }

    #[test]
    fn every_rung_of_the_ladder_parses() {
        for rung in ladder(Kind::Coloured, 1) {
            assert!(
                rung.seconds.is_finite() && rung.seconds > 0.0,
                "{}",
                rung.name
            );
        }
    }

    #[test]
    fn usage_reads_a_stat_line() {
        // Only Linux has /proc; elsewhere there is nothing to read.
        if cfg!(target_os = "linux") {
            let usage = Usage::of("self").unwrap();
            assert!(usage.minor_faults > 0);
        } else {
            assert!(Usage::of("self").is_none());
        }
    }
}
