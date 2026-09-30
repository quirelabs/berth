//! The supervisor and workers together, in-process but over real sockets:
//! the same endpoints, handshakes and frames as separate processes use.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use berth_proto::v1::{self, SessionId, ToApp, ToServer};
use tokio::time::timeout;

use super::client::{self, Client};
use super::paths::Places;
use super::supervisor::{self, Config, Launcher};
use crate::worker::Shell;

type Tasks = Arc<Mutex<HashMap<SessionId, tokio::task::AbortHandle>>>;

struct Server {
    places: Places,
    tasks: Tasks,
    supervisor: tokio::task::JoinHandle<crate::error::Result<()>>,
    _dir: tempfile::TempDir,
}

fn places(dir: &std::path::Path) -> Places {
    Places {
        run: dir.join("run"),
        bin: dir.join("bin"),
    }
}

fn launcher(tasks: &Tasks) -> Launcher {
    Launcher::InProcess {
        shell: Some(Shell {
            program: "/bin/sh".into(),
            login: false,
        }),
        tasks: tasks.clone(),
    }
}

async fn supervise(
    places: Places,
    tasks: &Tasks,
    idle: Duration,
) -> tokio::task::JoinHandle<crate::error::Result<()>> {
    let config = Config {
        places: places.clone(),
        launcher: launcher(tasks),
        idle,
    };
    let task = tokio::spawn(supervisor::run(config));
    for _ in 0..100 {
        if places.supervisor().exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    task
}

async fn server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let places = places(dir.path());
    let tasks = Tasks::default();
    let supervisor = supervise(places.clone(), &tasks, Duration::from_secs(60)).await;
    Server {
        places,
        tasks,
        supervisor,
        _dir: dir,
    }
}

async fn next(client: &mut Client) -> ToApp {
    timeout(Duration::from_secs(10), client.from_server.recv())
        .await
        .expect("the server answered in time")
        .expect("the connection stayed open")
}

async fn create(client: &mut Client) -> SessionId {
    client
        .to_server
        .send(ToServer::Create {
            name: Some("t".into()),
            cwd: None,
            cols: 40,
            rows: 10,
        })
        .await
        .unwrap();
    loop {
        if let ToApp::Created { session } = next(client).await {
            return session;
        }
    }
}

/// Everything that arrives until `needle` shows in the session's output.
async fn until_output(client: &mut Client, needle: &str) {
    let mut text = Vec::new();
    loop {
        if let ToApp::Output { bytes, .. } = next(client).await {
            text.extend_from_slice(&bytes);
            if String::from_utf8_lossy(&text).contains(needle) {
                return;
            }
        }
    }
}

async fn type_line(client: &Client, session: SessionId, line: &str) {
    client
        .to_server
        .send(ToServer::Input {
            session,
            bytes: format!("{line}\n").into_bytes(),
        })
        .await
        .unwrap();
}

async fn attach(client: &mut Client, session: SessionId) -> String {
    client
        .to_server
        .send(ToServer::Attach {
            session,
            cols: 40,
            rows: 10,
        })
        .await
        .unwrap();
    loop {
        if let ToApp::Screen { screen, .. } = next(client).await {
            let screen: v1::Screen = screen.unpack().unwrap();
            return screen
                .lines
                .iter()
                .map(|r| r.cells.iter().map(|c| c.text.as_str()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
}

async fn list(client: &mut Client) -> Vec<v1::SessionInfo> {
    client.to_server.send(ToServer::ListSessions).await.unwrap();
    loop {
        if let ToApp::Sessions(list) = next(client).await {
            return list;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_is_created_attached_and_typed_into() {
    let server = server().await;
    let mut app = client::connect(&server.places).await.unwrap();
    let session = create(&mut app).await;
    attach(&mut app, session).await;
    type_line(&app, session, "echo hi_$((6*7))").await;
    until_output(&mut app, "hi_42").await;
    let sessions = list(&mut app).await;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, session);
}

/// Quitting the app detaches: the shell keeps running, and the next
/// connection finds it with its screen.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_outlives_its_client() {
    let server = server().await;
    let session = {
        let mut app = client::connect(&server.places).await.unwrap();
        let session = create(&mut app).await;
        attach(&mut app, session).await;
        type_line(&app, session, "echo before_$((1+1))").await;
        until_output(&mut app, "before_2").await;
        session
    };
    let mut app = client::connect(&server.places).await.unwrap();
    assert_eq!(list(&mut app).await[0].id, session);
    let screen = attach(&mut app, session).await;
    assert!(screen.contains("before_2"), "{screen}");
    type_line(&app, session, "echo after_$((2+1))").await;
    until_output(&mut app, "after_3").await;
}

/// A crashed worker takes down its own session, and only that.
#[tokio::test(flavor = "multi_thread")]
async fn a_crashed_worker_loses_one_session() {
    let server = server().await;
    let mut app = client::connect(&server.places).await.unwrap();
    let doomed = create(&mut app).await;
    let kept = create(&mut app).await;
    attach(&mut app, doomed).await;
    attach(&mut app, kept).await;

    let task = server.tasks.lock().unwrap().get(&doomed).cloned().unwrap();
    task.abort();
    loop {
        if let ToApp::Event {
            session,
            event: v1::Event::Exited { .. },
        } = next(&mut app).await
        {
            assert_eq!(session, doomed);
            break;
        }
    }
    let left: Vec<SessionId> = list(&mut app).await.iter().map(|s| s.id).collect();
    assert_eq!(left, vec![kept]);
    type_line(&app, kept, "echo alive_$((3+4))").await;
    until_output(&mut app, "alive_7").await;
}

/// The supervisor holds no terminal: killing it loses nothing, and the next
/// one finds every worker again from the runtime directory.
#[tokio::test(flavor = "multi_thread")]
async fn a_crashed_supervisor_loses_nothing() {
    let server = server().await;
    let session = {
        let mut app = client::connect(&server.places).await.unwrap();
        let session = create(&mut app).await;
        attach(&mut app, session).await;
        type_line(&app, session, "echo kept_$((5*5))").await;
        until_output(&mut app, "kept_25").await;
        session
    };
    server.supervisor.abort();
    let _ = server.supervisor.await;

    let _again = supervise(
        server.places.clone(),
        &server.tasks,
        Duration::from_secs(60),
    )
    .await;
    let mut app = client::connect(&server.places).await.unwrap();
    let mut found = Vec::new();
    for _ in 0..50 {
        found = list(&mut app).await;
        if !found.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        found.iter().map(|s| s.id).collect::<Vec<_>>(),
        vec![session]
    );
    let screen = attach(&mut app, session).await;
    assert!(screen.contains("kept_25"), "{screen}");
    // New sessions do not reuse the number of one still running.
    assert!(create(&mut app).await > session);
}

/// Something that is not Berth is dropped, and the server carries on.
#[tokio::test(flavor = "multi_thread")]
async fn a_peer_that_does_not_speak_berth_is_dropped() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let server = server().await;
    let mut stranger = super::transport::connect(&server.places.supervisor())
        .await
        .unwrap();
    stranger.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    let mut buf = vec![0u8; 4096];
    let closed = timeout(Duration::from_secs(10), async {
        loop {
            match stranger.read(&mut buf).await {
                Ok(0) | Err(_) => return true,
                Ok(_) => continue,
            }
        }
    })
    .await;
    assert_eq!(closed, Ok(true));
    let mut app = client::connect(&server.places).await.unwrap();
    assert!(list(&mut app).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_supervisor_exits() {
    let dir = tempfile::tempdir().unwrap();
    let places = places(dir.path());
    let tasks = Tasks::default();
    let supervisor = supervise(places.clone(), &tasks, Duration::from_millis(300)).await;
    let ended = timeout(Duration::from_secs(5), supervisor).await;
    assert!(matches!(ended, Ok(Ok(Ok(())))), "{ended:?}");
    assert!(!places.supervisor().exists(), "its endpoint is gone");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_supervisor_does_not_start() {
    let server = server().await;
    let config = Config {
        places: server.places.clone(),
        launcher: launcher(&server.tasks),
        idle: Duration::from_secs(60),
    };
    assert!(supervisor::run(config).await.is_err());
    let mut app = client::connect(&server.places).await.unwrap();
    assert!(list(&mut app).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_does_not_exist_is_refused_by_name() {
    let server = server().await;
    let mut app = client::connect(&server.places).await.unwrap();
    app.to_server
        .send(ToServer::Attach {
            session: 99,
            cols: 80,
            rows: 24,
        })
        .await
        .unwrap();
    let ToApp::Refused { what, .. } = next(&mut app).await else {
        panic!("not refused");
    };
    assert_eq!(what, "attach 99");
}
