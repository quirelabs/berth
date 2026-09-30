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

pub struct Worker {
    session: SessionId,
    name: String,
    cwd: Option<String>,
    size: Mutex<(u16, u16)>,
    pty: Pty,
    terminal: Arc<Terminal>,
    /// Bytes of output so far. Locked while a chunk is fed to the terminal
    /// and counted, and while a screen is taken, so the two always agree.
    seq: Arc<Mutex<u64>>,
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
        let terminal = Arc::new(Terminal::with_scrollback(
            TermSize::new(spec.cols, spec.rows),
            CellSize::new(8.0, 16.0),
            SCROLLBACK,
        ));
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
                cwd: spec
                    .cwd
                    .as_ref()
                    .map(|dir| dir.to_string_lossy().to_string()),
                size: Mutex::new((spec.cols, spec.rows)),
                pty,
                terminal,
                seq,
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
            ToServer::Attach { cols, rows, .. } => {
                self.resize(cols, rows).await;
                Some(self.screen())
            }
            ToServer::Kill { .. } => {
                self.pty.close().await;
                None
            }
            // Nothing to do: output keeps flowing whether anyone reads it.
            ToServer::Detach { .. } | ToServer::Confirmed { .. } | ToServer::Visible { .. } => None,
            // Asked by a supervisor finding its workers again: this session.
            ToServer::ListSessions => Some(ToApp::Sessions(vec![self.info()])),
            ToServer::Create { .. } => Some(ToApp::Refused {
                what: format!("{message:?}"),
                why: "a worker holds one session; ask the supervisor".into(),
            }),
        }
    }

    /// This session as the session list shows it.
    pub fn info(&self) -> v1::SessionInfo {
        let (cols, rows) = *self.size.lock().unwrap_or_else(|p| p.into_inner());
        v1::SessionInfo {
            id: self.session,
            name: self.name.clone(),
            cwd: self.cwd.clone(),
            cols,
            rows,
            claude: None,
            pid: self.pty.local_pid(),
        }
    }

    async fn resize(&self, cols: u16, rows: u16) {
        let size = TermSize::new(cols, rows);
        *self.size.lock().unwrap_or_else(|p| p.into_inner()) = (size.cols, size.rows);
        self.terminal.resize(size, CellSize::new(8.0, 16.0));
        self.pty.resize(size.cols, size.rows).await;
    }

    /// The screen as of the output so far, packed.
    ///
    /// Output chunks at or below its `seq` may still be on their way to the
    /// app behind it; the app skips them, as they are already on this screen.
    pub fn screen(&self) -> ToApp {
        let seq = lock(&self.seq);
        let screen = self.terminal.wire_screen();
        let at = *seq;
        drop(seq);
        match screen.map(|s| Packed::pack(&s)) {
            Some(Ok(screen)) => ToApp::Screen {
                session: self.session,
                seq: at,
                screen,
            },
            Some(Err(error)) => self.refused("screen", error.to_string()),
            None => self.refused("screen", "the terminal did not answer".into()),
        }
    }

    /// Up to [`HISTORY_RANGE`] history lines older than `before`, or the
    /// newest. `None` when there are none.
    pub fn history(&self, before: Option<u64>) -> Option<ToApp> {
        let history = self.terminal.wire_history(before, HISTORY_RANGE);
        if history.lines.is_empty() {
            return None;
        }
        Some(match Packed::pack(&history.lines) {
            Ok(lines) => ToApp::History {
                session: self.session,
                newest: history.newest,
                lines,
            },
            Err(error) => self.refused("history", error.to_string()),
        })
    }

    fn refused(&self, what: &str, why: String) -> ToApp {
        ToApp::Refused {
            what: format!("{what} of session {}", self.session),
            why,
        }
    }
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
    while let Some(event) = events.blocking_recv() {
        let message = match event {
            PtyEvent::Output(bytes) => {
                let mut total = lock(&seq);
                terminal.advance(&bytes);
                *total += bytes.len() as u64;
                let at = *total;
                drop(total);
                terminal.wait_for_room(MAX_BACKLOG);
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
        };
        if tx.blocking_send(message).is_err() {
            // Nobody is listening any more. The shell keeps running only as
            // long as someone holds the worker.
            break;
        }
    }
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
        let (seq, text) = screen_text(&worker.screen());
        assert!(seq >= total);
        assert!(text.contains("berth_42"), "{text}");
    }

    /// Nothing from whatever started the worker leaks into the shell, and
    /// Berth's own terminal identity is set (D51).
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
    async fn attaching_resizes_and_answers_with_the_screen() {
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
            .await
            .unwrap();
        let ToApp::Screen { screen, .. } = &reply else {
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
        let Some(ToApp::History { newest, lines, .. }) = worker.history(None) else {
            panic!("no history");
        };
        let rows: Vec<v1::Row> = lines.unpack().unwrap();
        assert!(!rows.is_empty());
        let Some(ToApp::History { newest: older, .. }) = worker.history(Some(newest)) else {
            panic!("no older history");
        };
        assert!(older < newest);
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
