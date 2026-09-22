//! A shell on this machine, behind the same handle as a remote one.
//!
//! The seam is [`Pty`] and [`PtyEvent`]: the view, the output pump and the
//! recording never learn where the bytes came from, so a local session is a
//! second constructor rather than a second architecture (D36).
//!
//! Local sessions are deliberately plain. No tmux, no reconnect machine, and
//! the shell ends when Berth quits (D36). What survives a quit is the layout,
//! the directory and the last screen, which Berth saves itself (D46); a new
//! shell starts there when asked.
//!
//! `portable-pty` does the platform work. It is blocking, so the two ends are
//! bridged to the async world by threads rather than by pretending otherwise.

use std::io::{Read, Write};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::pty::{INPUT_QUEUE_SIZE, OUTPUT_QUEUE_SIZE, Pty, PtyEvent, PtyInput};
use crate::session::TERM;

/// Read at most this much per wakeup. Matches the ssh side's appetite: output
/// arrives far faster than a UI can paint, and the pump coalesces anyway.
const READ_CHUNK: usize = 8 * 1024;

/// The shell to run, in the order it is decided.
///
/// `BERTH_SHELL` wins so a settings page has something to set. After that the
/// platform's own answer, and a fallback that exists on every machine so this
/// can never return nothing.
pub fn login_shell() -> String {
    if let Some(shell) = crate::env::shell() {
        return shell;
    }
    #[cfg(windows)]
    {
        std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

/// Open a shell on this machine.
///
/// Returns the same pair as the ssh path, so everything above it is unchanged.
pub async fn open(cols: u16, rows: u16) -> Result<(Pty, mpsc::Receiver<PtyEvent>)> {
    open_with(&login_shell(), true, None, cols, rows).await
}

/// Open a shell on this machine in a given directory.
///
/// A directory that has gone since it was saved falls back to home rather than
/// failing: the user asked for a shell, and a shell in the wrong place is
/// better than none.
pub async fn open_in(
    cwd: &std::path::Path,
    cols: u16,
    rows: u16,
) -> Result<(Pty, mpsc::Receiver<PtyEvent>)> {
    let cwd = cwd.is_dir().then_some(cwd);
    open_with(&login_shell(), true, cwd, cols, rows).await
}

/// The working directory of a local process, where the platform will say.
///
/// Linux answers through `/proc`. macOS has no file for it, and the call that
/// does answer needs `libc`, so this asks `lsof`, which ships with every macOS
/// and reads the same kernel record. Windows keeps another process's directory
/// in its memory, out of reach without reading it, so there it is `None` and a
/// restored shell starts at home.
///
/// Blocking: `lsof` is a process. Call it off the UI thread.
pub fn cwd_of(pid: u32) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/sbin/lsof")
            .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        parse_lsof_cwd(&String::from_utf8_lossy(&output.stdout))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// The path out of `lsof -Fn` output: the line that starts with `n`.
///
/// Anything else, or nothing, is `None`. The format is not ours, and a wrong
/// directory is worse than home.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_lsof_cwd(output: &str) -> Option<std::path::PathBuf> {
    output
        .lines()
        .find_map(|line| line.strip_prefix('n'))
        .filter(|path| path.starts_with('/'))
        .map(std::path::PathBuf::from)
}

/// Open a named shell, optionally as a login shell.
///
/// Split out from [`open`] so the tests can drive a plain `sh` in a known
/// state. A login shell runs the user's profile, which is right for a session
/// and wrong for a test: it can print, prompt, or take a second, and none of
/// that is what the tests below are about.
async fn open_with(
    shell: &str,
    login: bool,
    cwd: Option<&std::path::Path>,
    cols: u16,
    rows: u16,
) -> Result<(Pty, mpsc::Receiver<PtyEvent>)> {
    let size = PtySize {
        rows: rows.max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    };

    let pair = native_pty_system()
        .openpty(size)
        .map_err(|e| Error::Pty(format!("could not open a local pty: {e}")))?;

    let mut command = CommandBuilder::new(shell);
    // A login shell, because that is what sets PATH. Without it Homebrew is
    // missing on macOS and the first thing anyone types fails to be found,
    // which is the same class of problem as the non-interactive ssh PATH note
    // in the runbook. cmd.exe has no equivalent and needs no equivalent.
    #[cfg(not(windows))]
    if login {
        command.arg("-l");
    }
    #[cfg(windows)]
    let _ = login;
    // The parser is a real terminal, so say so. Without this, programs fall
    // back to line mode or refuse to run.
    command.env("TERM", TERM);
    match cwd {
        Some(cwd) => command.cwd(cwd),
        None => {
            if let Some(home) = dirs::home_dir() {
                command.cwd(home);
            }
        }
    }

    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|e| Error::Pty(format!("could not start {shell}: {e}")))?;
    let pid = child.process_id();

    // The slave must go now. Holding it keeps the write end open, so when the
    // shell exits the master never reaches EOF, the reader blocks forever and
    // the session looks alive after it has ended.
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| Error::Pty(format!("could not read the local pty: {e}")))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| Error::Pty(format!("could not write the local pty: {e}")))?;

    let (input_tx, mut input_rx) = mpsc::channel::<PtyInput>(INPUT_QUEUE_SIZE);
    let (output_tx, output_rx) = mpsc::channel::<PtyEvent>(OUTPUT_QUEUE_SIZE);

    // Reading is a blocking syscall with no async equivalent that works on all
    // three platforms, so it gets a thread. `blocking_send` applies the same
    // backpressure the ssh side gets from its bounded channel.
    let reads = output_tx.clone();
    std::thread::spawn(move || {
        let mut buffer = vec![0u8; READ_CHUNK];
        loop {
            match reader.read(&mut buffer) {
                // EOF: the shell is gone.
                Ok(0) => break,
                Ok(n) => {
                    if reads
                        .blocking_send(PtyEvent::Output(buffer[..n].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        // Report the exit before the close, in that order, matching ssh.
        if let Ok(status) = child.wait() {
            let _ = reads.blocking_send(PtyEvent::Exit(status.exit_code()));
        }
        let _ = reads.blocking_send(PtyEvent::Closed);
    });

    // Writes and resizes share a thread because both need the master, and
    // because a write that blocks must not stall the reader.
    let master = pair.master;
    std::thread::spawn(move || {
        while let Some(request) = input_rx.blocking_recv() {
            match request {
                PtyInput::Data(bytes) => {
                    if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
                PtyInput::Resize { cols, rows } => {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                PtyInput::Close => break,
            }
        }
        // Dropping the master hangs up the shell, which ends the reader.
        drop(master);
    });

    Ok((
        Pty {
            input: input_tx,
            pid,
        },
        output_rx,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    /// The shell the tests drive, and never the user's. `sh` exists on every
    /// unix, speaks one dialect, and starts in the same state on a laptop and
    /// on a CI runner with no `SHELL` set at all. Windows has no equivalent
    /// that both exists everywhere and speaks this dialect, so these three run
    /// where they can be trusted rather than everywhere.
    #[cfg(unix)]
    const TEST_SHELL: &str = "/bin/sh";

    /// Collect output until `needle` appears, or give up.
    #[cfg(unix)]
    async fn read_until(events: &mut mpsc::Receiver<PtyEvent>, needle: &str) -> String {
        let mut seen = String::new();
        let deadline = Duration::from_secs(10);
        while let Ok(Some(event)) = timeout(deadline, events.recv()).await {
            match event {
                PtyEvent::Output(bytes) => {
                    seen.push_str(&String::from_utf8_lossy(&bytes));
                    if seen.contains(needle) {
                        return seen;
                    }
                }
                PtyEvent::Closed => return seen,
                PtyEvent::Exit(_) => {}
            }
        }
        seen
    }

    #[test]
    fn a_shell_is_always_named() {
        assert!(!login_shell().is_empty());
    }

    /// The whole point: bytes typed here come back through the same handle the
    /// ssh path uses.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_local_shell_echoes_what_it_is_told() {
        let (pty, mut events) = open_with(TEST_SHELL, false, None, 80, 24)
            .await
            .expect("a local shell should open");
        assert!(pty.write("echo berth-local-ok\n").await);
        let seen = read_until(&mut events, "berth-local-ok").await;
        assert!(seen.contains("berth-local-ok"), "got {seen:?}");
        pty.close().await;
    }

    /// The slave-drop bug: if the write end is held, the shell exiting never
    /// reaches EOF and the session looks alive forever.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_exiting_shell_reports_closed() {
        let (pty, mut events) = open_with(TEST_SHELL, false, None, 80, 24)
            .await
            .expect("a local shell should open");
        assert!(pty.write("exit\n").await);

        let mut closed = false;
        while let Ok(Some(event)) = timeout(Duration::from_secs(10), events.recv()).await {
            if event == PtyEvent::Closed {
                closed = true;
                break;
            }
        }
        assert!(closed, "the shell exited and the session never closed");
    }

    /// `stty size` rather than `tput cols`: it asks the tty by ioctl, so it
    /// needs no terminfo database and answers the same on a bare runner.
    ///
    /// No sleep between the resize and the question. Both travel the same
    /// input channel and one thread drains it in order, so the ioctl has
    /// already happened by the time the shell reads the line. If that ever
    /// stops being true this test fails rather than passing slowly.
    #[cfg(unix)]
    #[tokio::test]
    async fn resizing_reaches_the_shell() {
        let (pty, mut events) = open_with(TEST_SHELL, false, None, 80, 24)
            .await
            .expect("a local shell should open");

        // Ask before resizing as well, so a test that stopped being able to
        // tell the two apart fails here rather than passing on both.
        assert!(pty.write("stty size\n").await);
        let before = read_until(&mut events, "24 80").await;
        assert!(before.contains("24 80"), "got {before:?}");

        assert!(pty.resize(120, 40).await);
        assert!(pty.write("stty size\n").await);
        let after = read_until(&mut events, "40 120").await;
        assert!(after.contains("40 120"), "got {after:?}");
        pty.close().await;
    }

    /// A shell started in a directory is found there, which is the round trip
    /// saving and restoring depends on. The directory is canonicalised because
    /// macOS reports `/tmp` as `/private/tmp`.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn a_shells_directory_can_be_read_back() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let (pty, mut events) = open_with(TEST_SHELL, false, Some(dir.path()), 80, 24)
            .await
            .expect("a local shell should open");
        // Once it answers, it is running and sitting in its directory.
        assert!(pty.write("echo ready\n").await);
        read_until(&mut events, "ready").await;

        let pid = pty.local_pid().expect("a local shell has a pid");
        let found = tokio::task::spawn_blocking(move || cwd_of(pid))
            .await
            .expect("the lookup ran")
            .expect("the directory is readable");
        assert_eq!(found.canonicalize().ok(), dir.path().canonicalize().ok());

        // And it follows a cd, since that is the directory worth saving.
        let sub = dir.path().join("inner");
        std::fs::create_dir(&sub).expect("a subdirectory");
        assert!(pty.write("cd inner && echo moved\n").await);
        read_until(&mut events, "moved").await;
        let found = tokio::task::spawn_blocking(move || cwd_of(pid))
            .await
            .expect("the lookup ran")
            .expect("the directory is readable");
        assert_eq!(found.canonicalize().ok(), sub.canonicalize().ok());
        pty.close().await;
    }

    #[test]
    fn lsof_output_gives_the_path() {
        let output = "p123\nfcwd\nn/Users/someone/work dir\n";
        assert_eq!(
            parse_lsof_cwd(output),
            Some(std::path::PathBuf::from("/Users/someone/work dir"))
        );
    }

    /// No answer, or one that is not a path, is no directory rather than a
    /// guess.
    #[test]
    fn odd_lsof_output_gives_nothing() {
        for output in ["", "p123\nfcwd\n", "nrelative/path\n", "garbage"] {
            assert_eq!(parse_lsof_cwd(output), None, "{output:?}");
        }
    }
}
