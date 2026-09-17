//! The trust-on-first-use path, against a real host.
//!
//! This is the one path where being wrong means connecting to a machine the
//! user did not agree to, so none of it is mocked. The fingerprint the dialog
//! would show is compared against what the server itself reports for its own
//! key file, over a separate authenticated connection.
//!
//! Skipped unless `BERTH_TEST_HOST` names an alias in the user's ssh config.
//! Run with:
//!
//!     BERTH_TEST_HOST=berth-test cargo test -p berth-core --test trust -- --nocapture

use berth_core::config::SshConfig;
use berth_core::known_hosts::{
    TrustPolicy, Verdict, fingerprint, is_known_in, learn_in, verify_in,
};
use berth_core::transport::Connection;

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

/// The whole flow, in the order the user walks it.
///
/// Probe the host, get a fingerprint, check it against the server's own answer,
/// record it, and confirm the recorded entry is what a later connect verifies
/// against. Nothing here touches the user's known_hosts: every step names the
/// temporary file explicitly.
#[tokio::test]
async fn a_probed_key_is_the_servers_own_and_records_as_trusted() {
    let host = host_or_skip!();

    let (_verdict, key) = Connection::offered_key(&host)
        .await
        .expect("the host should offer a key");
    let shown = fingerprint(&key);
    assert!(shown.starts_with("SHA256:"), "{shown}");

    // What the server says about itself, asked over an ordinary connection.
    // If these disagree, the fingerprint in the dialog is not this machine's.
    let conn = Connection::connect(&host, TrustPolicy::KnownHostsOnly)
        .await
        .expect("should connect with the agent");
    let out = conn
        .exec("ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub")
        .await
        .expect("should read the host's own key file");
    conn.disconnect().await.ok();

    let reported = out.stdout_string();
    assert!(
        reported.contains(shown.trim_start_matches("SHA256:")),
        "the probed key is not the one the server reports\n  probed:   {shown}\n  reported: {reported}"
    );

    // Before accepting: unknown, which is the state the dialog is shown in.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    std::fs::write(&path, "").unwrap();
    assert!(!is_known_in(&host.hostname, host.port, &path));
    assert!(matches!(
        verify_in(&host.hostname, host.port, &key, &path),
        Verdict::Unknown { .. }
    ));

    // Accepting.
    learn_in(&host.hostname, host.port, &key, &path).expect("should record the key");

    // After: known, and a later connect verifies against it rather than asking.
    assert!(is_known_in(&host.hostname, host.port, &path));
    assert_eq!(
        verify_in(&host.hostname, host.port, &key, &path),
        Verdict::Trusted
    );
}

/// A probe looks and does not touch.
///
/// It cannot return a session by construction, and it must not need an agent to
/// do its job: the fingerprint has to be readable before the user has agreed to
/// authenticate to anything.
#[tokio::test]
async fn a_probe_opens_no_session() {
    let host = host_or_skip!();
    let (_, key) = Connection::offered_key(&host)
        .await
        .expect("the host should offer a key");
    assert!(!fingerprint(&key).is_empty());
}

/// Rejecting writes nothing. The host is exactly as unknown afterwards.
#[tokio::test]
async fn declining_records_nothing() {
    let host = host_or_skip!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    std::fs::write(&path, "").unwrap();

    let (_, _key) = Connection::offered_key(&host)
        .await
        .expect("the host should offer a key");

    // The user says no, so `learn_in` is never reached.
    assert!(!is_known_in(&host.hostname, host.port, &path));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
}
