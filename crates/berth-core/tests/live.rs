//! Transport tests against a real host.
//!
//! Skipped unless `BERTH_TEST_HOST` names an alias in the user's ssh config,
//! so CI, which has no such host and no agent, stays green. Run with:
//!
//!     BERTH_TEST_HOST=berth-test cargo test -p berth-core --test live -- --nocapture

use berth_core::config::SshConfig;
use berth_core::known_hosts::{TrustPolicy, Verdict};
use berth_core::pty::PtyEvent;
use berth_core::session::Session;
use berth_core::transport::Connection;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

/// The host under test, or `None` when the suite should be skipped.
fn target() -> Option<berth_core::Host> {
    let alias = std::env::var(berth_core::env::TEST_HOST).ok()?;
    let config = SshConfig::load_default().expect("ssh config should be readable");
    Some(
        config
            .get(&alias)
            .unwrap_or_else(|| panic!("BERTH_TEST_HOST={alias} is not in ssh config"))
            .clone(),
    )
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

/// Caps how many of these tests may be mid-connect at once.
///
/// Every test here opens its own connection, and running them all in parallel
/// trips sshd's `MaxStartups` (10 unauthenticated connections by default),
/// which shows up as a `ConnectionReset` on a different, random test each run.
/// This is a property of the harness, not of Berth: the app opens one
/// connection per host and multiplexes channels over it, so it never presents
/// sshd with more than one handshake per host.
static CONNECT_LIMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

async fn connect(host: &berth_core::Host) -> Connection {
    let _permit = CONNECT_LIMIT
        .acquire()
        .await
        .expect("the semaphore is never closed");
    Connection::connect(host, TrustPolicy::KnownHostsOnly)
        .await
        .expect("should connect with the agent")
}

#[tokio::test]
async fn connects_and_runs_a_command() {
    let host = host_or_skip!();
    let conn = connect(&host).await;

    let out = conn.exec("echo hello from berth").await.unwrap();
    assert!(out.succeeded(), "exit was {:?}", out.exit_code);
    assert_eq!(out.stdout_string().trim(), "hello from berth");
    assert!(out.stderr.is_empty());
    conn.disconnect().await.unwrap();
}

/// The host is already in known_hosts, so the strict policy must accept it
/// without any prompt.
#[tokio::test]
async fn the_host_key_is_already_trusted() {
    let host = host_or_skip!();
    let conn = connect(&host).await;
    assert_eq!(conn.host_key_verdict(), Some(Verdict::Trusted));
    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn a_failing_command_reports_its_exit_code_and_stderr() {
    let host = host_or_skip!();
    let conn = connect(&host).await;

    let out = conn.exec("echo oops >&2; exit 3").await.unwrap();
    assert_eq!(out.exit_code, Some(3));
    assert!(!out.succeeded());
    assert_eq!(out.stderr_string().trim(), "oops");
    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn stdout_and_stderr_stay_separate() {
    let host = host_or_skip!();
    let conn = connect(&host).await;

    let out = conn.exec("echo to_out; echo to_err >&2").await.unwrap();
    assert_eq!(out.stdout_string().trim(), "to_out");
    assert_eq!(out.stderr_string().trim(), "to_err");
    conn.disconnect().await.unwrap();
}

/// Large outputs must arrive whole; this is the shape of the `cat` of a big
/// file that M0 requires not to stall the UI.
#[tokio::test]
async fn a_large_output_arrives_intact() {
    let host = host_or_skip!();
    let conn = connect(&host).await;

    // 4 MB of predictable bytes, enough to cross many SSH windows.
    let out = conn
        .exec("head -c 4194304 /dev/zero | tr '\\0' 'a'")
        .await
        .unwrap();
    assert!(out.succeeded());
    assert_eq!(out.stdout.len(), 4 * 1024 * 1024);
    assert!(out.stdout.iter().all(|b| *b == b'a'));
    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn stdin_is_delivered_to_the_remote_command() {
    let host = host_or_skip!();
    let conn = connect(&host).await;

    let out = conn.exec_with_stdin("cat", b"piped payload").await.unwrap();
    assert!(out.succeeded());
    assert_eq!(out.stdout_string(), "piped payload");
    conn.disconnect().await.unwrap();
}

/// Several channels on one connection, which is how Berth multiplexes shells,
/// probe execs and forwards.
#[tokio::test]
async fn one_connection_serves_many_channels() {
    let host = host_or_skip!();
    let conn = connect(&host).await;

    for i in 0..5 {
        let out = conn.exec(&format!("echo channel {i}")).await.unwrap();
        assert_eq!(out.stdout_string().trim(), format!("channel {i}"));
    }
    conn.disconnect().await.unwrap();
}

/// Read PTY output until `needle` appears, or give up. Terminal output arrives
/// in arbitrarily sized pieces, so anything that waits for a single read is a
/// flaky test waiting to happen.
async fn read_until(rx: &mut mpsc::Receiver<PtyEvent>, needle: &str) -> String {
    let mut seen = String::new();
    let deadline = Duration::from_secs(15);
    loop {
        match timeout(deadline, rx.recv()).await {
            Ok(Some(PtyEvent::Output(bytes))) => {
                seen.push_str(&String::from_utf8_lossy(&bytes));
                if seen.contains(needle) {
                    return seen;
                }
            }
            Ok(Some(
                PtyEvent::Exit(_)
                | PtyEvent::Rebuild(_)
                | PtyEvent::InputDropped
                | PtyEvent::HistoryWhole,
            )) => {}
            Ok(Some(PtyEvent::Closed)) | Ok(None) => {
                panic!("channel closed before {needle:?} appeared; saw: {seen:?}")
            }
            Err(_) => panic!("timed out waiting for {needle:?}; saw: {seen:?}"),
        }
    }
}

/// A tmux-backed interactive session: attach, type, see the result.
#[tokio::test]
async fn a_tmux_session_echoes_what_is_typed() {
    let host = host_or_skip!();
    let conn = connect(&host).await;
    let session = Session::new("livetest-echo", 80, 24);

    let (pty, mut rx) = conn.open_session(&session).await.unwrap();
    // A marker command, so the assertion cannot pass on shell noise.
    pty.write("echo berth_marker_9f3a\n").await;
    let seen = read_until(&mut rx, "berth_marker_9f3a").await;
    assert!(seen.contains("berth_marker_9f3a"), "{seen:?}");

    pty.close().await;
    kill_session(&conn, &session).await;
    conn.disconnect().await.unwrap();
}

/// The point of tmux: state survives losing the channel entirely.
#[tokio::test]
async fn session_state_survives_a_reattach() {
    let host = host_or_skip!();
    let conn = connect(&host).await;
    let session = Session::new("livetest-persist", 80, 24);

    let (pty, mut rx) = conn.open_session(&session).await.unwrap();
    pty.write("BERTH_PERSISTED=kept_9f3a\n").await;
    read_until(&mut rx, "$").await;
    pty.close().await;
    drop(rx);

    // Re-attach over a brand new channel; `new-session -A` finds the session.
    let (pty2, mut rx2) = conn.open_session(&session).await.unwrap();
    pty2.write("echo $BERTH_PERSISTED\n").await;
    let seen = read_until(&mut rx2, "kept_9f3a").await;
    assert!(
        seen.contains("kept_9f3a"),
        "the shell variable did not survive re-attach: {seen:?}"
    );

    pty2.close().await;
    kill_session(&conn, &session).await;
    conn.disconnect().await.unwrap();
}

#[tokio::test]
async fn resizing_changes_what_the_remote_reports() {
    let host = host_or_skip!();
    let conn = connect(&host).await;
    let session = Session::new("livetest-resize", 80, 24);

    let (pty, mut rx) = conn.open_session(&session).await.unwrap();
    pty.write("echo cols_$(tput cols)\n").await;
    let seen = read_until(&mut rx, "cols_80").await;
    assert!(seen.contains("cols_80"), "{seen:?}");

    pty.resize(120, 40).await;
    // tmux needs a moment to apply the new size to the pane.
    tokio::time::sleep(Duration::from_millis(500)).await;
    pty.write("echo cols_$(tput cols)\n").await;
    let seen = read_until(&mut rx, "cols_120").await;
    assert!(seen.contains("cols_120"), "resize did not take: {seen:?}");

    pty.close().await;
    kill_session(&conn, &session).await;
    conn.disconnect().await.unwrap();
}

/// The shape of the M0 throughput case, over a PTY rather than an exec channel.
#[tokio::test]
async fn a_large_burst_arrives_over_the_pty() {
    let host = host_or_skip!();
    let conn = connect(&host).await;
    let session = Session::new("livetest-burst", 200, 50);

    let (pty, mut rx) = conn.open_session(&session).await.unwrap();
    pty.write("head -c 2000000 /dev/zero | tr '\\0' 'x' | wc -c\n")
        .await;
    let seen = read_until(&mut rx, "2000000").await;
    assert!(seen.contains("2000000"), "{seen:?}");

    pty.close().await;
    kill_session(&conn, &session).await;
    conn.disconnect().await.unwrap();
}

/// Count everything a PTY delivers until it goes quiet.
async fn drain_bytes(rx: &mut mpsc::Receiver<PtyEvent>, quiet_for: Duration) -> u64 {
    let mut total = 0u64;
    loop {
        match timeout(quiet_for, rx.recv()).await {
            Ok(Some(PtyEvent::Output(bytes))) => total += bytes.len() as u64,
            Ok(Some(PtyEvent::Closed)) | Ok(None) => return total,
            Ok(Some(_)) => {}
            // Nothing for a while means the burst is over.
            Err(_) => return total,
        }
    }
}

/// tmux is a terminal emulator, not a pipe. For fast-scrolling line-structured
/// output it re-renders rather than forwarding verbatim, so a client behind
/// tmux parses fewer bytes than the command wrote. This matters because it is
/// exactly the gap between "cat 50 MB" and the bytes Berth's parser sees, and
/// every Berth session is tmux-backed.
///
/// Line structure is the point: a single enormous line gives tmux nothing to
/// collapse, and it then forwards slightly more than it received.
#[tokio::test]
async fn tmux_collapses_fast_scrolling_output() {
    let host = host_or_skip!();
    let conn = connect(&host).await;
    const SIZE: u64 = 8 * 1024 * 1024;
    // 76-column lines, the shape of the base64 benchmark file.
    let command = format!("head -c {SIZE} /dev/zero | tr '\\0' 'y' | fold -w 76\n");
    let quiet = Duration::from_secs(5);

    let (plain_pty, mut plain_rx) = conn.open_shell(200, 50).await.unwrap();
    plain_pty.write(command.clone()).await;
    let plain = drain_bytes(&mut plain_rx, quiet).await;
    plain_pty.close().await;

    let session = Session::new("livetest-bytes", 200, 50);
    let (tmux_pty, mut tmux_rx) = conn.open_session(&session).await.unwrap();
    tmux_pty.write(command).await;
    let through_tmux = drain_bytes(&mut tmux_rx, quiet).await;
    tmux_pty.close().await;

    println!(
        "plain shell delivered {plain} bytes, tmux delivered {through_tmux} ({:.0}%)",
        through_tmux as f64 / plain as f64 * 100.0
    );
    assert!(plain >= SIZE, "a plain PTY should forward everything");
    assert!(
        through_tmux < plain,
        "tmux delivered {through_tmux}, plain delivered {plain}"
    );

    kill_session(&conn, &session).await;
    conn.disconnect().await.unwrap();
}

/// Live tests must not leave sessions behind on a shared host.
async fn kill_session(conn: &Connection, session: &Session) {
    let _ = conn
        .exec(&format!(
            "tmux kill-session -t {} 2>/dev/null || true",
            berth_core::session::shell_quote(&session.tmux_name())
        ))
        .await;
}

/// tmux must be present for M1; failing here is a server problem, not a bug.
#[tokio::test]
async fn the_remote_has_tmux() {
    let host = host_or_skip!();
    let conn = connect(&host).await;

    let out = conn.exec("command -v tmux").await.unwrap();
    assert!(out.succeeded(), "tmux is missing on the test host");
    conn.disconnect().await.unwrap();
}
