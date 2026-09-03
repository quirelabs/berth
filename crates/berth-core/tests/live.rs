//! Transport tests against a real host.
//!
//! Skipped unless `BERTH_TEST_HOST` names an alias in the user's ssh config,
//! so CI, which has no such host and no agent, stays green. Run with:
//!
//!     BERTH_TEST_HOST=berth-test cargo test -p berth-core --test live -- --nocapture

use berth_core::config::SshConfig;
use berth_core::known_hosts::{TrustPolicy, Verdict};
use berth_core::transport::Connection;

/// The host under test, or `None` when the suite should be skipped.
fn target() -> Option<berth_core::Host> {
    let alias = std::env::var("BERTH_TEST_HOST").ok()?;
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

async fn connect(host: &berth_core::Host) -> Connection {
    Connection::connect(host, TrustPolicy::KnownHostsOnly)
        .await
        .expect("should connect with the agent")
}

#[tokio::test]
async fn connects_and_runs_a_command() {
    let host = host_or_skip!();
    let conn = Connection::connect(&host, TrustPolicy::KnownHostsOnly)
        .await
        .expect("should connect using the OS agent");

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

/// tmux must be present for M1; failing here is a server problem, not a bug.
#[tokio::test]
async fn the_remote_has_tmux() {
    let host = host_or_skip!();
    let conn = connect(&host).await;

    let out = conn.exec("command -v tmux").await.unwrap();
    assert!(out.succeeded(), "tmux is missing on the test host");
    conn.disconnect().await.unwrap();
}
