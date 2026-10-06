//! The server speed floor (D49, D50): how fast a session's output gets
//! through the whole server path, a pty, a worker parsing it, the supervisor
//! and a client confirming it, per kind of output.
//!
//! D49 set the floor at twice tmux's rate per kind of output, measured on
//! `berth-test`: 28 MB/s plain, 32 MB/s coloured, 24 MB/s emoji. A run prints
//! each kind's rate and fails if any is under its floor.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use berth_proto::v1::{self, ToApp, ToServer};

use super::client;
use super::paths::Places;
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
            let found = String::from_utf8_lossy(tail).contains(needle);
            if tail.len() > 8192 {
                tail.drain(..tail.len() - 4096);
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

/// One kind's rate through a supervisor and worker in this process, over the
/// same sockets as processes use.
pub async fn measure(kind: Kind, megabytes: usize, scratch: &Path) -> Result<Rate> {
    let places = Places {
        run: scratch.join("run"),
        bin: scratch.join("bin"),
    };
    let file = scratch.join(format!("{}.txt", kind.name()));
    let bytes = write_stream(kind, &file, megabytes)?;
    let server = tokio::spawn(supervisor::run(Config {
        places: places.clone(),
        launcher: Launcher::InProcess {
            shell: Some(Shell {
                program: "/bin/sh".into(),
                login: false,
            }),
            tasks: Default::default(),
        },
        idle: Duration::from_secs(60),
    }));
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
        loop {
            let message = next(&mut app).await?;
            if shows(&message, "BENCH_DONE", &mut tail) {
                break;
            }
        }
        let seconds = started.elapsed().as_secs_f64();
        let _ = app.to_server.send(ToServer::Kill { session }).await;
        Ok(Rate {
            kind,
            bytes,
            seconds,
        })
    }
    .await;
    server.abort();
    let _ = std::fs::remove_file(&file);
    result
}

/// `--bench-throughput [kind ...] [--mb N]`: each kind's rate, against its
/// floor. Fails if any is under it.
pub async fn run_process(args: &[String]) -> Result<bool> {
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
    let mut all = true;
    for kind in kinds {
        let rate = measure(kind, megabytes, &scratch).await;
        match rate {
            Ok(rate) => {
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
        let rate = measure(Kind::Plain, 2, dir.path()).await.unwrap();
        assert!(rate.mb_per_s() > 0.0);
        assert!(rate.bytes >= 2 * MB);
    }
}
