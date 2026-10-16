//! Full-screen applications, mouse, paste and shell integration over a real PTY.
//!
//! These are the M0 criteria a glance at a window cannot settle. Output is fed
//! to the real terminal model, so assertions are about the rendered grid rather
//! than about bytes on the wire.
//!
//! Some tests deliberately run *without* tmux. tmux holds the alternate screen
//! for its own lifetime, so through a Berth session that flag is always on and
//! says nothing about the program inside the pane (decisions.md D22). Where the
//! alternate screen is the thing under test, a plain shell is used; where tmux
//! is the thing under test, assertions are on content instead.
//!
//! Skipped unless `BERTH_TEST_HOST` is set. Run with:
//!
//!     BERTH_TEST_HOST=berth-test cargo test -p berth-core --test interactive

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;

use berth_core::config::SshConfig;
use berth_core::known_hosts::TrustPolicy;
use berth_core::pty::PtyEvent;
use berth_core::session::Session;
use berth_core::transport::Connection;
use berth_term::Terminal;
use berth_term::size::{CellSize, TermSize};

/// See the note in `live.rs`: parallel connects trip sshd's MaxStartups.
static CONNECT_LIMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(3);

fn target() -> Option<berth_core::Host> {
    let alias = std::env::var(berth_core::env::TEST_HOST).ok()?;
    SshConfig::load_default().ok()?.get(&alias).cloned()
}

macro_rules! host_or_skip {
    () => {
        match target() {
            Some(host) => host,
            None => {
                eprintln!("skipping: BERTH_TEST_HOST is not set");
                return;
            }
        }
    };
}

async fn connect(host: &berth_core::Host) -> Connection {
    let _permit = CONNECT_LIMIT.acquire().await.unwrap();
    Connection::connect(host, TrustPolicy::KnownHostsOnly)
        .await
        .expect("should connect with the agent")
}

/// A terminal fed from a PTY, so tests can ask what is on screen.
struct Screen {
    terminal: Terminal,
}

impl Screen {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            terminal: Terminal::new(TermSize::new(cols, rows), CellSize::new(8.0, 16.0)),
        }
    }

    /// Pump output until `predicate` holds or the deadline passes.
    async fn until(
        &mut self,
        rx: &mut mpsc::Receiver<PtyEvent>,
        what: &str,
        predicate: impl Fn(&Terminal) -> bool,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if predicate(&self.terminal) {
                return;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                panic!(
                    "timed out waiting for {what}; screen was:\n{}",
                    self.terminal.visible_text()
                );
            }
            match timeout(left, rx.recv()).await {
                Ok(Some(PtyEvent::Output(bytes))) => {
                    self.terminal.advance(&bytes);
                }
                Ok(Some(PtyEvent::Closed)) | Ok(None) => {
                    if predicate(&self.terminal) {
                        return;
                    }
                    panic!("channel closed while waiting for {what}");
                }
                Ok(Some(_)) => {}
                Err(_) => panic!(
                    "timed out waiting for {what}; screen was:\n{}",
                    self.terminal.visible_text()
                ),
            }
        }
    }

    /// Take whatever is pending without blocking for long.
    async fn drain(&mut self, rx: &mut mpsc::Receiver<PtyEvent>, ms: u64) {
        while let Ok(Some(PtyEvent::Output(bytes))) =
            timeout(Duration::from_millis(ms), rx.recv()).await
        {
            self.terminal.advance(&bytes);
        }
    }

    fn text(&self) -> String {
        self.terminal.visible_text()
    }
}

async fn settle(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

fn has_prompt(t: &Terminal) -> bool {
    let text = t.visible_text();
    text.contains('$') || text.contains('#')
}

async fn kill_session(conn: &Connection, session: &Session) {
    let _ = conn
        .exec(&format!(
            "tmux kill-session -t {} 2>/dev/null || true",
            berth_core::session::shell_quote(&session.tmux_name())
        ))
        .await;
}

/// vim must take the alternate screen and give it back on `:q`.
///
/// Without tmux on purpose: this asserts Berth's own parser tracks the mode,
/// which is what M4 needs once it has a usable source for it.
#[tokio::test]
async fn vim_takes_and_releases_the_alternate_screen() {
    let host = host_or_skip!();
    let conn = connect(&host).await;
    let (pty, mut rx) = conn.open_shell(100, 30).await.unwrap();
    let mut screen = Screen::new(100, 30);

    screen.until(&mut rx, "a shell prompt", has_prompt).await;
    assert!(!screen.terminal.alternate_screen());

    pty.write("mkdir -p ~/berth-bench && vim -u NONE ~/berth-bench/berth_vim_test.txt\n")
        .await;
    screen
        .until(&mut rx, "vim to take the alternate screen", |t| {
            t.alternate_screen()
        })
        .await;

    // Type something so the buffer is genuinely vim's, not a blank screen.
    pty.write("iberth_in_vim_9f3a\x1b").await;
    screen
        .until(&mut rx, "typed text to appear", |t| {
            t.visible_text().contains("berth_in_vim_9f3a")
        })
        .await;

    pty.write(":q!\n").await;
    screen
        .until(&mut rx, "vim to release the alternate screen", |t| {
            !t.alternate_screen()
        })
        .await;

    pty.close().await;
    let _ = conn.exec("rm -f ~/berth-bench/berth_vim_test.txt").await;
    conn.disconnect().await.unwrap();
}

/// htop is the other full-screen case, and unlike vim it repaints continuously.
#[tokio::test]
async fn htop_renders_and_quits() {
    let host = host_or_skip!();
    let conn = connect(&host).await;
    let (pty, mut rx) = conn.open_shell(100, 30).await.unwrap();
    let mut screen = Screen::new(100, 30);

    screen.until(&mut rx, "a shell prompt", has_prompt).await;

    pty.write("htop\n").await;
    screen
        .until(&mut rx, "htop to take the alternate screen", |t| {
            t.alternate_screen()
        })
        .await;
    screen
        .until(&mut rx, "htop's own chrome", |t| {
            let text = t.visible_text();
            text.contains("Tasks") || text.contains("Load average") || text.contains("Mem")
        })
        .await;

    pty.write("q").await;
    screen
        .until(&mut rx, "htop to exit", |t| !t.alternate_screen())
        .await;

    pty.close().await;
    conn.disconnect().await.unwrap();
}

/// A full-screen program asks for mouse events with a DECSET, and Berth must
/// notice: the click has to be forwarded rather than starting a local selection.
#[tokio::test]
async fn a_program_can_enable_and_release_mouse_reporting() {
    let host = host_or_skip!();
    let conn = connect(&host).await;
    let (pty, mut rx) = conn.open_shell(100, 30).await.unwrap();
    let mut screen = Screen::new(100, 30);

    screen.until(&mut rx, "a shell prompt", has_prompt).await;
    assert!(!screen.terminal.mouse_reporting());

    pty.write("htop\n").await;
    screen
        .until(&mut rx, "htop to request mouse reporting", |t| {
            t.mouse_reporting()
        })
        .await;

    pty.write("q").await;
    screen
        .until(&mut rx, "htop to exit", |t| !t.alternate_screen())
        .await;
    assert!(
        !screen.terminal.mouse_reporting(),
        "mouse reporting must be released when the program exits"
    );

    pty.close().await;
    conn.disconnect().await.unwrap();
}

/// Resizing while a full-screen program runs must reach that program, not just
/// tmux. Asserted on what vim reports, since the mode flag is tmux's here.
#[tokio::test]
async fn vim_sees_a_resize_while_it_is_open() {
    let host = host_or_skip!();
    let conn = connect(&host).await;
    let session = Session::new("interactive-resize", 80, 24);
    let (pty, mut rx) = conn.open_session(&session).await.unwrap();
    let mut screen = Screen::new(80, 24);

    screen.until(&mut rx, "a shell prompt", has_prompt).await;

    pty.write("vim -u NONE\n").await;
    settle(1500).await;

    pty.write(":echo 'cols_' . &columns\n").await;
    screen
        .until(&mut rx, "vim to report 80 columns", |t| {
            t.visible_text().contains("cols_80")
        })
        .await;

    pty.resize(120, 40).await;
    settle(1200).await;
    pty.write(":echo 'cols_' . &columns\n").await;
    screen
        .until(&mut rx, "vim to report the new width", |t| {
            t.visible_text().contains("cols_120")
        })
        .await;

    pty.write(":q!\n").await;
    settle(600).await;
    pty.write("echo resized_ok_9f3a\n").await;
    screen
        .until(&mut rx, "the shell to respond after vim quit", |t| {
            t.visible_text().contains("resized_ok_9f3a")
        })
        .await;

    pty.close().await;
    kill_session(&conn, &session).await;
    conn.disconnect().await.unwrap();
}

/// The reconnect case: vim is still running inside tmux after the channel is
/// gone, and `:q` on a brand new channel must still quit it cleanly.
#[tokio::test]
async fn vim_quits_cleanly_after_a_reattach() {
    let host = host_or_skip!();
    let conn = connect(&host).await;
    let session = Session::new("interactive-reattach", 100, 30);

    let (pty, mut rx) = conn.open_session(&session).await.unwrap();
    let mut screen = Screen::new(100, 30);
    screen.until(&mut rx, "a shell prompt", has_prompt).await;

    pty.write("vim -u NONE\n").await;
    settle(1500).await;
    pty.write("isurvived_9f3a\x1b").await;
    screen
        .until(&mut rx, "typed text", |t| {
            t.visible_text().contains("survived_9f3a")
        })
        .await;

    // Drop the channel entirely, as a dropped connection would.
    pty.close().await;
    drop(rx);
    settle(600).await;

    // Re-attach. tmux repaints, so vim is still there with its buffer.
    let (pty2, mut rx2) = conn.open_session(&session).await.unwrap();
    let mut screen2 = Screen::new(100, 30);
    screen2
        .until(&mut rx2, "vim to still be running after reattach", |t| {
            t.visible_text().contains("survived_9f3a")
        })
        .await;

    // The point: :q on a brand new channel quits the vim started on the old
    // one, and the shell comes back.
    pty2.write(":q!\n").await;
    settle(700).await;
    pty2.write("echo back_9f3a\n").await;
    screen2
        .until(&mut rx2, "the shell to respond after vim quit", |t| {
            t.visible_text().contains("back_9f3a")
        })
        .await;

    pty2.close().await;
    kill_session(&conn, &session).await;
    conn.disconnect().await.unwrap();
}

/// Through tmux the alternate screen flag is tmux's own, so it cannot tell
/// whether a full-screen program is running inside the pane.
///
/// Written as a test because M4 forbids predictive echo inside full-screen apps
/// and names the alternate screen as the detector. Through tmux that rule would
/// always fire; the usable source is tmux's own `#{alternate_on}`.
#[tokio::test]
async fn the_alternate_screen_is_not_usable_through_tmux() {
    let host = host_or_skip!();
    let conn = connect(&host).await;
    let session = Session::new("interactive-altscreen", 100, 30);
    let (pty, mut rx) = conn.open_session(&session).await.unwrap();
    let mut screen = Screen::new(100, 30);

    screen
        .until(&mut rx, "the attach to paint", has_prompt)
        .await;
    assert!(
        screen.terminal.alternate_screen(),
        "tmux takes the alternate screen on attach, before any program runs"
    );

    // tmux itself knows what the pane is doing, which is the usable signal.
    let out = conn
        .exec("tmux display-message -p -t 'berth/interactive-altscreen' '#{alternate_on}'")
        .await
        .unwrap();
    assert_eq!(
        out.stdout_string().trim(),
        "0",
        "tmux reports the pane is not in the alternate screen"
    );

    pty.close().await;
    kill_session(&conn, &session).await;
    conn.disconnect().await.unwrap();
}

/// The shell integration snippet, sourced into a real bash inside tmux, must
/// produce all four markers with a usable exit code.
///
/// tmux drops OSC sequences it does not recognise, so this only works because
/// the snippet wraps them in tmux passthrough and Berth enables that on its own
/// sessions. Static assertions on the snippet's text cannot show any of that.
#[tokio::test]
async fn the_shell_integration_emits_all_four_markers_through_tmux() {
    use berth_term::ShellEventKind;

    let host = host_or_skip!();
    let conn = connect(&host).await;
    let session = Session::new("interactive-osc133", 100, 30);
    let (pty, mut rx) = conn.open_session(&session).await.unwrap();
    let mut screen = Screen::new(100, 30);

    let push = format!(
        "mkdir -p ~/berth-bench && cat > ~/berth-bench/berth_shell_integration.sh <<'BERTH_EOF'\n{}\nBERTH_EOF",
        berth_probe::SHELL_INTEGRATION
            .replace("\r\n", "\n")
            .trim_end()
    );
    assert!(conn.exec(&push).await.unwrap().succeeded());

    pty.write("bash --norc --noprofile -i\n").await;
    settle(1000).await;
    pty.write("source ~/berth-bench/berth_shell_integration.sh\n")
        .await;
    settle(1000).await;
    screen.until(&mut rx, "the sourced shell", has_prompt).await;
    screen.drain(&mut rx, 400).await;
    screen.terminal.drain_shell_events();

    pty.write("(exit 7)\n").await;
    settle(1500).await;
    screen.drain(&mut rx, 400).await;

    let kinds: Vec<_> = screen
        .terminal
        .drain_shell_events()
        .iter()
        .map(|e| e.kind)
        .collect();
    println!("shell events through tmux: {kinds:?}");

    for expected in [
        ShellEventKind::PromptStart,
        ShellEventKind::CommandStart,
        ShellEventKind::OutputStart,
    ] {
        assert!(kinds.contains(&expected), "missing {expected:?}: {kinds:?}");
    }
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, ShellEventKind::CommandEnd { exit_code: Some(7) })),
        "the exit code must survive to the D marker: {kinds:?}"
    );

    pty.write("exit\n").await;
    pty.close().await;
    let _ = conn
        .exec("rm -f ~/berth-bench/berth_shell_integration.sh")
        .await;
    kill_session(&conn, &session).await;
    conn.disconnect().await.unwrap();
}

/// Bracketed paste: the remote turns it on, and pasted text must be wrapped so
/// a shell treats it as literal input rather than as typed commands.
#[tokio::test]
async fn bracketed_paste_is_negotiated_and_wrapped() {
    let host = host_or_skip!();
    let conn = connect(&host).await;
    let (pty, mut rx) = conn.open_shell(100, 30).await.unwrap();
    let mut screen = Screen::new(100, 30);

    pty.write("bash --norc --noprofile -i\n").await;
    screen
        .until(&mut rx, "an interactive bash", has_prompt)
        .await;
    settle(700).await;
    screen.drain(&mut rx, 400).await;

    assert!(
        screen.terminal.bracketed_paste(),
        "bash's line editor should have enabled bracketed paste"
    );

    // A paste containing a newline must not run as a command.
    let payload = "echo pasted_9f3a\nnot_a_command_9f3a";
    pty.write(berth_term::paste_bytes(payload, true)).await;
    settle(900).await;
    screen
        .until(&mut rx, "the paste to appear on the line", |t| {
            t.visible_text().contains("pasted_9f3a")
        })
        .await;
    assert!(
        !screen.text().contains("command not found"),
        "a bracketed paste must not execute; screen:\n{}",
        screen.text()
    );

    pty.write("\x03").await;
    settle(300).await;
    pty.write("exit\n").await;
    pty.close().await;
    conn.disconnect().await.unwrap();
}
