//! `berth-server --connect` as a host runs it (D55): the real binary, as a
//! process, with its data directory in a temporary one. Unix only, as hosts
//! are.
#![cfg(unix)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use berth_core::server::paths::Places;

const SERVER: &str = env!("CARGO_BIN_EXE_berth-server");

/// Our greeting, framed, as the app sends it first.
fn greeting() -> Vec<u8> {
    let hello = berth_core::server::link::hello().encode().unwrap();
    berth_proto::frame(&hello).unwrap()
}

/// Start a relay with `data` as its data directory.
fn relay(data: &std::path::Path) -> std::process::Child {
    Command::new(SERVER)
        .arg("--connect")
        .env(berth_core::env::DATA, data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Greet through a relay and read the supervisor's greeting back: its first
/// frame, whole.
fn greeted(child: &mut std::process::Child) -> berth_proto::Hello {
    let stdin = child.stdin.as_mut().unwrap();
    stdin.write_all(&greeting()).unwrap();
    stdin.flush().unwrap();
    let stdout = child.stdout.as_mut().unwrap();
    let mut len = [0u8; 4];
    stdout.read_exact(&mut len).unwrap();
    let mut payload = vec![0u8; u32::from_le_bytes(len) as usize];
    stdout.read_exact(&mut payload).unwrap();
    berth_proto::Hello::decode(&payload).unwrap()
}

fn supervisor_pid(places: &Places) -> Option<u32> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(pid) = std::fs::read_to_string(places.supervisor_pid())
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            return Some(pid);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}

/// Stops the supervisor when the test ends, passing or not.
struct Stopped(u32);

impl Drop for Stopped {
    fn drop(&mut self) {
        stop(self.0);
    }
}

fn stop(pid: u32) {
    let _ = Command::new("kill").arg(pid.to_string()).status();
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Two relays arriving together both reach the supervisor, one supervisor
/// between them; when the app's side closes, the relay ends and its output
/// closes, while the supervisor, detached in a session of its own, holding
/// none of the relay's descriptors, runs on.
#[test]
fn two_relays_at_once_share_one_detached_supervisor() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let places = Places::under(&data);
    let mut first = relay(&data);
    let mut second = relay(&data);
    let a = greeted(&mut first);
    let b = greeted(&mut second);
    let version = Command::new(SERVER).arg("--version").output().unwrap();
    assert_eq!(a.build, String::from_utf8_lossy(&version.stdout).trim());
    assert_eq!(b.build, a.build);
    let pid = supervisor_pid(&places).expect("a supervisor started");
    let _stopped = Stopped(pid);

    // Its own process group and session: setsid made it their leader.
    let group = Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&group.stdout).trim(),
        pid.to_string(),
        "the supervisor leads a group of its own"
    );

    // The app's side closes: the relay's output reaches its end at once,
    // which it would not if the supervisor held the relay's stdout.
    drop(first.stdin.take());
    let mut rest = Vec::new();
    let started = Instant::now();
    first
        .stdout
        .as_mut()
        .unwrap()
        .read_to_end(&mut rest)
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(first.wait().unwrap().success());
    assert!(alive(pid), "the supervisor outlives the relay");

    // The second relay still talks to the same supervisor.
    drop(second.stdin.take());
    let _ = second.wait();
    assert_eq!(supervisor_pid(&places), Some(pid));
}

/// Started together any number of times, a supervisor is started once:
/// whoever takes the start lock first starts it, and the rest find it
/// answering.
#[tokio::test(flavor = "multi_thread")]
async fn a_supervisor_is_started_once_however_many_ask_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let places = Places::under(&data);
    // A stand-in that counts its starts, then becomes the real supervisor.
    let counter = dir.path().join("starts");
    let exe = dir.path().join("supervisor.sh");
    std::fs::write(
        &exe,
        format!(
            "#!/bin/sh\necho started >> '{}'\nBERTH_DATA='{}' exec '{SERVER}' \"$@\"\n",
            counter.display(),
            data.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    let asking: Vec<_> = (0..4)
        .map(|_| {
            let places = places.clone();
            let exe = exe.clone();
            tokio::spawn(
                async move { berth_core::server::start::ensure_running(&places, &exe).await },
            )
        })
        .collect();
    for asked in asking {
        asked.await.unwrap().unwrap();
    }
    let _stopped = supervisor_pid(&places).map(Stopped);
    let starts = std::fs::read_to_string(&counter).unwrap();
    assert_eq!(starts.lines().count(), 1, "{starts:?}");
}
