//! Only the user who started the server can connect to it (D50), checked from
//! a real second account, not only in a unit test: anything that can reach
//! the socket can type into every shell.
//!
//! Skipped unless `BERTH_TEST_OTHER_USER` names a local account this user can
//! run commands as without a password (`sudo -n -u <account>`), with
//! `python3` on its PATH. Both checks are tested separately:
//!
//! 1. The runtime directory is private, so the other account cannot reach the
//!    supervisor's socket at all.
//! 2. Where it can reach a socket anyway, the peer check drops it before a
//!    byte of a session is sent.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use berth_core::server::paths::Places;
use berth_core::server::supervisor::{self, Config, Launcher};
use berth_core::server::{Listener, client};

fn other_user() -> Option<String> {
    let user = std::env::var(berth_core::env::TEST_OTHER_USER).ok()?;
    (!user.is_empty()).then_some(user)
}

/// A directory in `/tmp` that any account can walk into, so what stops the
/// other account is Berth's own protection and not the parent's.
struct Shared(PathBuf);

impl Shared {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(format!("/tmp/berth-other-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self(dir)
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Connect to `socket` as the other account, send a byte, and report what
/// happened: "refused <error>", or "closed <n>" with the bytes it read back.
fn connect_as(user: &str, socket: &Path) -> String {
    let script = r#"
import socket, sys
s = socket.socket(socket.AF_UNIX)
try:
    s.connect(sys.argv[1])
except OSError as e:
    print("refused", e.errno); sys.exit(0)
s.settimeout(5)
try:
    s.sendall(b"BRTH")
    got = s.recv(4096)
except OSError as e:
    got = b""
print("closed", len(got))
"#;
    let output = Command::new("sudo")
        .args(["-n", "-u", user, "python3", "-c", script])
        .arg(socket)
        .output()
        .expect("sudo runs");
    assert!(
        output.status.success(),
        "could not run as {user}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn another_account_cannot_reach_the_supervisor() {
    let Some(user) = other_user() else {
        eprintln!("skipped: set BERTH_TEST_OTHER_USER");
        return;
    };
    let shared = Shared::new("dir");
    let places = Places {
        run: shared.0.join("run"),
        bin: shared.0.join("bin"),
    };
    let supervisor = tokio::spawn(supervisor::run(Config {
        places: places.clone(),
        launcher: Launcher::InProcess {
            shell: None,
            tasks: Default::default(),
        },
        idle: Duration::from_secs(60),
    }));
    for _ in 0..100 {
        if client::connect(&places).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let result = connect_as(&user, &places.supervisor());
    // EACCES: the private runtime directory stops it before the socket.
    assert_eq!(result, "refused 13", "{user} reached the supervisor");
    supervisor.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn another_account_that_reaches_a_socket_is_dropped() {
    let Some(user) = other_user() else {
        eprintln!("skipped: set BERTH_TEST_OTHER_USER");
        return;
    };
    let shared = Shared::new("peer");
    let socket = shared.0.join("open.sock");
    let mut listener = Listener::bind(&socket).await.unwrap();
    // Deliberately reachable, so only the peer check stands in the way.
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o777)).unwrap();
    let accepted = tokio::spawn(async move { listener.accept().await.is_ok() });

    let result = tokio::task::spawn_blocking(move || connect_as(&user, &socket))
        .await
        .unwrap();
    assert_eq!(result, "closed 0", "the other account got an answer");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !accepted.is_finished(),
        "the listener handed the connection on"
    );
    accepted.abort();
}
