//! Berth's server as real processes (D50): `berth --supervisor` from the built
//! binary, which starts each worker as a detached process of its own. The
//! in-process tests in berth-core cover the logic; this covers the processes,
//! their arguments, and a supervisor that is killed outright.
//!
//! Unix only for now: the Windows pipe transport is compile-checked in CI and
//! verified by hand.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use berth_core::server::client::{self, Client};
use berth_core::server::paths::Places;
use berth_proto::v1::{self, SessionId, ToApp, ToServer};
use tokio::time::timeout;

fn places(data: &Path) -> Places {
    Places {
        run: data.join("run"),
        bin: data.join("bin"),
    }
}

/// The supervisor, killed when the test ends however it ends.
struct Supervisor(Child);

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn start(data: &Path) -> Supervisor {
    let child = Command::new(env!("CARGO_BIN_EXE_berth"))
        .arg("--supervisor")
        .env("BERTH_DATA", data)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the supervisor starts");
    let endpoint = places(data).supervisor();
    for _ in 0..200 {
        if client::connect(&places(data)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(endpoint.exists(), "the supervisor listens");
    Supervisor(child)
}

async fn next(app: &mut Client) -> ToApp {
    timeout(Duration::from_secs(15), app.from_server.recv())
        .await
        .expect("the server answered in time")
        .expect("the connection stayed open")
}

async fn until_output(app: &mut Client, needle: &str) {
    let mut text = Vec::new();
    loop {
        if let ToApp::Output { bytes, .. } = next(app).await {
            text.extend_from_slice(&bytes);
            if String::from_utf8_lossy(&text).contains(needle) {
                return;
            }
        }
    }
}

fn worker_pid(data: &Path, session: SessionId) -> Option<u32> {
    std::fs::read_to_string(places(data).worker_pid(session))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_run_in_worker_processes_that_outlive_the_supervisor() {
    let dir = tempfile::tempdir().unwrap();
    let data: PathBuf = dir.path().to_path_buf();
    let supervisor = start(&data).await;

    let mut app = client::connect(&places(&data)).await.unwrap();
    app.to_server
        .send(ToServer::Create {
            name: Some("proc".into()),
            cwd: Some(data.to_string_lossy().to_string()),
            cols: 80,
            rows: 24,
        })
        .await
        .unwrap();
    let session = loop {
        match next(&mut app).await {
            ToApp::Created { session } => break session,
            ToApp::Refused { why, .. } => panic!("refused: {why}"),
            _ => {}
        }
    };
    app.to_server
        .send(ToServer::Attach {
            session,
            cols: 80,
            rows: 24,
        })
        .await
        .unwrap();
    app.to_server
        .send(ToServer::Input {
            session,
            bytes: b"echo proc_$((20+22))\n".to_vec(),
        })
        .await
        .unwrap();
    until_output(&mut app, "proc_42").await;

    let pid = worker_pid(&data, session).expect("the worker wrote its pid");
    assert_ne!(pid, supervisor.0.id(), "the worker is a process of its own");

    // Kill the supervisor outright. The worker must not notice.
    drop(app);
    drop(supervisor);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(alive(pid), "the worker outlived its supervisor");

    let _again = start(&data).await;
    let mut app = client::connect(&places(&data)).await.unwrap();
    let mut found = Vec::new();
    for _ in 0..100 {
        app.to_server.send(ToServer::ListSessions).await.unwrap();
        if let ToApp::Sessions(list) = next(&mut app).await {
            found = list;
            if !found.is_empty() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        found.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![session]
    );

    app.to_server
        .send(ToServer::Attach {
            session,
            cols: 80,
            rows: 24,
        })
        .await
        .unwrap();
    loop {
        if let ToApp::Screen { screen, .. } = next(&mut app).await {
            let screen: v1::Screen = screen.unpack().unwrap();
            let text: String = screen
                .lines
                .iter()
                .flat_map(|r| r.cells.iter().map(|c| c.text.clone()))
                .collect();
            assert!(text.contains("proc_42"), "the screen came back");
            break;
        }
    }

    // Ending the session ends its process and clears its files.
    app.to_server
        .send(ToServer::Kill { session })
        .await
        .unwrap();
    for _ in 0..100 {
        if !alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!alive(pid), "the worker ended");
    assert!(
        worker_pid(&data, session).is_none(),
        "its pid file went with it"
    );
}
