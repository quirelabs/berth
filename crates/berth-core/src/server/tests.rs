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

/// The next message, confirming output and screens as an app does: a worker
/// that is waiting for a screen to be confirmed sends nothing more (D49).
async fn next(client: &mut Client) -> ToApp {
    let message = timeout(Duration::from_secs(10), client.from_server.recv())
        .await
        .expect("the server answered in time")
        .expect("the connection stayed open");
    if let ToApp::Output { session, seq, .. } | ToApp::Screen { session, seq, .. } = &message {
        let _ = client
            .to_server
            .send(ToServer::Confirmed {
                session: *session,
                seq: *seq,
            })
            .await;
    }
    message
}

async fn create(client: &mut Client) -> SessionId {
    client
        .to_server
        .send(ToServer::Create {
            name: Some("t".into()),
            cwd: None,
            cols: 40,
            rows: 10,
            pane: None,
            resume: None,
        })
        .await
        .unwrap();
    loop {
        if let ToApp::Created { session } = next(client).await {
            return session;
        }
    }
}

/// Keep only the tail of a stream being searched, so a flood does not make
/// every check slower than the last.
fn keep_tail(text: &mut Vec<u8>) {
    const TAIL: usize = 4096;
    if text.len() > 2 * TAIL {
        text.drain(..text.len() - TAIL);
    }
}

/// Everything that arrives until `needle` shows in the session's output, or
/// on a screen: after a flood the line can come inside the screen that ends
/// the fallback rather than as output of its own.
async fn until_output(client: &mut Client, needle: &str) {
    let mut text = Vec::new();
    loop {
        match next(client).await {
            ToApp::Output { bytes, .. } => {
                text.extend_from_slice(&bytes);
                if String::from_utf8_lossy(&text).contains(needle) {
                    return;
                }
                keep_tail(&mut text);
            }
            ToApp::Screen { screen, .. } => {
                let screen: v1::Screen = screen.unpack().unwrap();
                let shown: String = screen
                    .lines
                    .iter()
                    .flat_map(|r| r.cells.iter().map(|c| c.text.as_str()))
                    .collect();
                if shown.contains(needle) {
                    return;
                }
            }
            _ => {}
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

/// A client that stops reading must not hold up anyone else: here it is
/// attached to a session that floods, and another client in another session
/// is still answered at once (D50).
#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_stops_reading_holds_up_no_one() {
    let server = server().await;
    let mut stuck = client::connect(&server.places).await.unwrap();
    let flooding = create(&mut stuck).await;
    attach(&mut stuck, flooding).await;

    let mut app = client::connect(&server.places).await.unwrap();
    let quiet = create(&mut app).await;
    attach(&mut app, quiet).await;

    // `stuck` never reads again from here, while its session prints far more
    // than every queue and socket buffer between them holds.
    type_line(&stuck, flooding, "yes berth_flood_line | head -n 6000000").await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let started = std::time::Instant::now();
    type_line(&app, quiet, "echo still_$((8*8))").await;
    until_output(&mut app, "still_64").await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the other session waited {:?}",
        started.elapsed()
    );
    app.to_server.send(ToServer::ListSessions).await.unwrap();
    loop {
        if let ToApp::Sessions(list) = next(&mut app).await {
            assert_eq!(list.len(), 2);
            break;
        }
    }
    drop(stuck);
}

/// A client that fell behind is not behind for good: once it reads again it
/// gets a fresh screen before any more output, and its session carries on.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_fell_behind_catches_up() {
    let server = server().await;
    let mut slow = client::connect(&server.places).await.unwrap();
    let session = create(&mut slow).await;
    attach(&mut slow, session).await;
    type_line(&slow, session, "yes berth_flood_line | head -n 4000000").await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Reading again: a screen arrives before long, then the session answers.
    let screen = timeout(Duration::from_secs(20), async {
        loop {
            if let ToApp::Screen { .. } = next(&mut slow).await {
                return;
            }
        }
    })
    .await;
    assert!(screen.is_ok(), "no screen after catching up");
    type_line(&slow, session, "echo back_$((2*3))").await;
    until_output(&mut slow, "back_6").await;
}

/// A program's status report (OSC 7501, D53) reaches the attached app as an
/// event and the session list as the record the session is shown by, is kept
/// while no app is attached, and reaches an app that attaches later.
#[tokio::test(flavor = "multi_thread")]
async fn program_status_reaches_the_app_and_outlives_it() {
    let server = server().await;
    let mut app = client::connect(&server.places).await.unwrap();
    let session = create(&mut app).await;
    attach(&mut app, session).await;
    // "Building" in base64.
    type_line(
        &app,
        session,
        "printf '\\033]7501;state=working:app=make:msg=QnVpbGRpbmc=\\033\\\\'",
    )
    .await;
    let mut event = None;
    let mut listed = None;
    timeout(Duration::from_secs(10), async {
        while event.is_none() || listed.is_none() {
            match next(&mut app).await {
                ToApp::Event {
                    event: v1::Event::Status(status),
                    ..
                } => event = Some(status),
                ToApp::Sessions(list) => {
                    listed = list
                        .into_iter()
                        .find(|s| s.id == session)
                        .and_then(|s| s.status)
                        .or(listed.take());
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the status never reached the app");
    let status = event.unwrap();
    assert_eq!(status.state, v1::StatusState::Working);
    assert_eq!(status.app.as_deref(), Some("make"));
    assert_eq!(status.msg.as_deref(), Some("Building"));
    assert_eq!(listed, Some(status.clone()));

    // A report made while nobody is attached: finished, a second from now.
    type_line(
        &app,
        session,
        "(sleep 1; printf '\\033]7501;state=done:app=make\\033\\\\') &",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(app);
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut later = client::connect(&server.places).await.unwrap();
    later.to_server.send(ToServer::ListSessions).await.unwrap();
    let listed = timeout(Duration::from_secs(10), async {
        loop {
            if let ToApp::Sessions(list) = next(&mut later).await
                && let Some(info) = list.into_iter().find(|s| s.id == session)
            {
                return info.status;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(listed.map(|s| s.state), Some(v1::StatusState::Done));
    later
        .to_server
        .send(ToServer::Attach {
            session,
            cols: 40,
            rows: 10,
        })
        .await
        .unwrap();
    let again = timeout(Duration::from_secs(10), async {
        loop {
            if let ToApp::Event {
                event: v1::Event::Status(status),
                ..
            } = next(&mut later).await
            {
                return status;
            }
        }
    })
    .await
    .expect("an app attaching later was not told the status");
    assert_eq!(again.state, v1::StatusState::Done);
    let _ = later.to_server.send(ToServer::Kill { session }).await;
}

/// A session two apps have open ends for both when one ends it, also right
/// after a flood, with the second app attaching at its end and reading
/// nothing: the worker must go, not linger with its shell dead.
#[tokio::test(flavor = "multi_thread")]
async fn ending_a_session_two_apps_have_open_ends_it() {
    let server = server().await;
    let mut first = client::connect(&server.places).await.unwrap();
    let session = create(&mut first).await;
    attach(&mut first, session).await;
    type_line(
        &first,
        session,
        "yes berth_flood | head -n 800000; echo flood_do\"\"ne",
    )
    .await;
    until_output(&mut first, "flood_done").await;

    let second = client::connect(&server.places).await.unwrap();
    second
        .to_server
        .send(ToServer::Attach {
            session,
            cols: 40,
            rows: 10,
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    first
        .to_server
        .send(ToServer::Kill { session })
        .await
        .unwrap();

    // Neither app reads from here on, as an app that is busy would not.
    let gone = timeout(Duration::from_secs(5), async {
        loop {
            if !server
                .tasks
                .lock()
                .unwrap()
                .get(&session)
                .is_some_and(|task| !task.is_finished())
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(gone.is_ok(), "the worker outlived its session");
    drop(second);
}

/// A client that keeps up at the rates of normal work, a fast build log at a
/// few MB/s in bursts, stays on the raw stream: no screen at all (D50).
#[tokio::test(flavor = "multi_thread")]
async fn a_build_log_stays_raw() {
    let server = server().await;
    let mut app = client::connect(&server.places).await.unwrap();
    let session = create(&mut app).await;
    attach(&mut app, session).await;
    type_line(
        &app,
        session,
        "i=0; while [ $i -lt 60 ]; do seq 1 12000; sleep 0.01; i=$((i+1)); done; echo log_do\"\"ne",
    )
    .await;
    let mut screens = 0;
    let mut text = Vec::new();
    timeout(Duration::from_secs(30), async {
        loop {
            match next(&mut app).await {
                ToApp::Screen { .. } => screens += 1,
                ToApp::Output { bytes, .. } => {
                    text.extend_from_slice(&bytes);
                    if String::from_utf8_lossy(&text).contains("log_done") {
                        return;
                    }
                    keep_tail(&mut text);
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the log never finished");
    assert_eq!(screens, 0, "fell back to screens on a build log");
    let _ = app.to_server.send(ToServer::Kill { session }).await;
}

/// A client that fell behind is never left frozen while the flood goes on:
/// screens, or raw output once it keeps up again, arrive every quarter of a
/// second, not only once the flood ends (D50). The flood here never ends, so
/// everything counted came during it.
#[tokio::test(flavor = "multi_thread")]
async fn a_pane_that_fell_behind_never_freezes_during_a_flood() {
    let server = server().await;
    let mut slow = client::connect(&server.places).await.unwrap();
    let session = create(&mut slow).await;
    attach(&mut slow, session).await;
    type_line(&slow, session, "yes berth_flood_line").await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let mut quiet = Vec::new();
    let started = std::time::Instant::now();
    let mut last = started;
    let mut screens = 0;
    let _ = timeout(Duration::from_secs(3), async {
        loop {
            let message = next(&mut slow).await;
            if matches!(message, ToApp::Screen { .. } | ToApp::Output { .. }) {
                if matches!(message, ToApp::Screen { .. }) {
                    screens += 1;
                }
                let now = std::time::Instant::now();
                if now.duration_since(last) > Duration::from_millis(250) {
                    quiet.push(now.duration_since(last));
                }
                last = now;
            }
        }
    })
    .await;
    assert!(screens >= 1, "it never fell behind: no screen at all");
    assert!(quiet.is_empty(), "frozen for {quiet:?} during the flood");
    let _ = slow.to_server.send(ToServer::Kill { session }).await;
}

mod through_the_app {
    //! The app's side: sessions seen as ptys through `LocalServer`.
    use super::*;
    use crate::pty::PtyEvent;
    use crate::server::local::{LocalServer, Start};
    use tokio::sync::mpsc::Receiver;

    /// Pty output until `needle` appears, as the app's pump would see it.
    async fn until(events: &mut Receiver<PtyEvent>, needle: &str) -> String {
        let mut text = Vec::new();
        timeout(Duration::from_secs(15), async {
            while let Some(event) = events.recv().await {
                if let PtyEvent::Output(bytes) = event {
                    text.extend_from_slice(&bytes);
                    if String::from_utf8_lossy(&text).contains(needle) {
                        return;
                    }
                    keep_tail(&mut text);
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{needle:?} never came: {:?}",
                String::from_utf8_lossy(&text)
            )
        });
        String::from_utf8_lossy(&text).to_string()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_is_a_pty_and_outlives_the_app() {
        let server = server().await;
        let app = LocalServer::start(server.places.clone(), Start::External)
            .await
            .unwrap();
        let session = app.create(None, 40, 10, None, None).await.unwrap();
        let (pty, mut events) = app.attach(session, 40, 10, None).await.unwrap();
        pty.write(b"echo seen_$((9*9))\n".to_vec()).await;
        until(&mut events, "seen_81").await;

        // Closing the pane detaches: the session stays.
        pty.close().await;
        drop(pty);
        drop(app);
        tokio::time::sleep(Duration::from_millis(100)).await;

        // A new app finds it, and its first output is the screen as it was.
        let app = LocalServer::start(server.places.clone(), Start::External)
            .await
            .unwrap();
        let list = app.list().await.unwrap();
        assert_eq!(list.iter().map(|s| s.id).collect::<Vec<_>>(), vec![session]);
        assert!(list[0].pid.is_some(), "the shell's pid is listed");
        let (_pty, mut events) = app.attach(session, 40, 10, None).await.unwrap();
        until(&mut events, "seen_81").await;
    }

    /// The supervisor goes away and comes back: the tab reattaches by itself
    /// and keeps working.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_restarted_supervisor_costs_the_tab_nothing() {
        let server = server().await;
        let app = LocalServer::start(server.places.clone(), Start::External)
            .await
            .unwrap();
        let session = app.create(None, 40, 10, None, None).await.unwrap();
        let (pty, mut events) = app.attach(session, 40, 10, None).await.unwrap();
        pty.write(b"echo one_$((1+0))\n".to_vec()).await;
        until(&mut events, "one_1").await;

        server.supervisor.abort();
        let _ = server.supervisor.await;
        let _again = supervise(
            server.places.clone(),
            &server.tasks,
            Duration::from_secs(60),
        )
        .await;

        // The screen comes back first, then the session answers again.
        until(&mut events, "one_1").await;
        let mut answered = false;
        for _ in 0..40 {
            pty.write(b"echo two_$((1+1))\n".to_vec()).await;
            let got = timeout(Duration::from_millis(500), until(&mut events, "two_2")).await;
            if got.is_ok() {
                answered = true;
                break;
            }
        }
        assert!(answered, "the session never answered after the restart");
    }

    /// A pane that stops taking output while its session floods falls
    /// behind, is caught up with a screen, and holds up no other tab.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pane_that_falls_behind_catches_up_with_a_screen() {
        let server = server().await;
        let app = LocalServer::start(server.places.clone(), Start::External)
            .await
            .unwrap();
        let flooding = app.create(None, 40, 10, None, None).await.unwrap();
        let (flood_pty, mut flood_events) = app.attach(flooding, 40, 10, None).await.unwrap();
        let quiet = app.create(None, 40, 10, None, None).await.unwrap();
        let (quiet_pty, mut quiet_events) = app.attach(quiet, 40, 10, None).await.unwrap();

        flood_pty
            .write(b"yes berth_flood | head -n 3000000; echo flood_do\"\"ne\n".to_vec())
            .await;
        // Nobody reads the flooding pane for a while.
        tokio::time::sleep(Duration::from_secs(2)).await;

        let started = std::time::Instant::now();
        quiet_pty.write(b"echo quiet_$((6+6))\n".to_vec()).await;
        until(&mut quiet_events, "quiet_12").await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );

        // Reading again, the flooding pane reaches the end.
        until(&mut flood_events, "flood_done").await;
    }

    /// Whether a rebuild of the pane's scrollback completes within `within`,
    /// reading everything else on the way.
    async fn rebuilt_within(events: &mut Receiver<PtyEvent>, within: Duration) -> bool {
        timeout(within, async {
            while let Some(event) = events.recv().await {
                if event == PtyEvent::Rebuild(berth_term::Shadow::Commit) {
                    return;
                }
            }
        })
        .await
        .is_ok()
    }

    /// After falling behind, a pane gets the history it missed only when
    /// output calms down or when it scrolls up and asks: never while output
    /// keeps coming unasked, and at once when asked (D50).
    #[tokio::test(flavor = "multi_thread")]
    async fn missed_history_comes_when_the_pane_scrolls_up_for_it() {
        let server = server().await;
        let app = LocalServer::start(server.places.clone(), Start::External)
            .await
            .unwrap();
        let session = app.create(None, 40, 10, None, None).await.unwrap();
        let (pty, mut events) = app.attach(session, 40, 10, None).await.unwrap();
        // The attach's own rebuild first, and whatever else it brings.
        assert!(rebuilt_within(&mut events, Duration::from_secs(10)).await);
        let _ = timeout(Duration::from_millis(500), async {
            while events.recv().await.is_some() {}
        })
        .await;

        pty.write(
            b"yes berth_flood | head -n 3000000; while :; do seq 1 3000; sleep 0.02; done\n"
                .to_vec(),
        )
        .await;
        // Nobody reads the pane for a while: it falls behind.
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Reading again while output keeps coming, faster than calm: the
        // pane catches up, and no history comes unasked.
        assert!(!rebuilt_within(&mut events, Duration::from_secs(3)).await);
        // Scrolled up into it: the history comes.
        pty.want_history().await;
        assert!(rebuilt_within(&mut events, Duration::from_secs(5)).await);
        let _ = app.kill(session).await;
    }
}

mod sync_under_load {
    //! Piece 5's gate (D51): after a flood that prunes the history several
    //! times, with the app falling behind on the way, the app's scrollback
    //! equals the worker's line for line.
    use super::*;
    use crate::pty::PtyEvent;
    use crate::server::local::{LocalServer, Start};
    use berth_term::size::{CellSize, TermSize};
    use berth_term::terminal::Terminal;

    fn text(row: &v1::Row) -> String {
        row.cells
            .iter()
            .map(|c| c.text.as_str())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// The worker's own history and screen, as a fresh attach receives them.
    async fn worker_view(places: &Places, session: SessionId) -> (Vec<String>, Vec<String>) {
        let mut raw = client::connect(places).await.unwrap();
        raw.to_server
            .send(ToServer::Attach {
                session,
                cols: 80,
                rows: 24,
            })
            .await
            .unwrap();
        let mut screen = None;
        let mut history: Vec<String> = Vec::new();
        loop {
            match next(&mut raw).await {
                ToApp::Screen {
                    screen: packed,
                    history_follows: true,
                    ..
                } => {
                    let s: v1::Screen = packed.unpack().unwrap();
                    screen = Some(s.lines.iter().map(text).collect());
                }
                ToApp::History {
                    lines, complete, ..
                } if screen.is_some() => {
                    let rows: Vec<v1::Row> = lines.unpack().unwrap();
                    history.extend(rows.iter().map(text));
                    if complete {
                        break;
                    }
                }
                _ => {}
            }
        }
        history.reverse();
        (history, screen.unwrap_or_default())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn after_a_pruning_flood_the_app_holds_the_workers_history_line_for_line() {
        let server = server().await;
        let app = LocalServer::start(server.places.clone(), Start::External)
            .await
            .unwrap();
        let session = app.create(None, 80, 24, None, None).await.unwrap();
        let (pty, mut events) = app.attach(session, 80, 24, None).await.unwrap();

        // The app's terminal, fed as the app's pump feeds it.
        let terminal = std::sync::Arc::new(Terminal::new(
            TermSize::new(80, 24),
            CellSize::new(8.0, 16.0),
        ));
        let pause = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (done_tx, done) = tokio::sync::oneshot::channel::<()>();
        let (rebuilt_tx, rebuilt) = tokio::sync::oneshot::channel::<()>();
        // Keeps feeding to the end: the screen and history that close the
        // flood come after its last line is first seen, once output has been
        // calm for a second (D50).
        let _feeding = {
            let terminal = terminal.clone();
            let pause = pause.clone();
            let mut done_tx = Some(done_tx);
            let mut rebuilt_tx = Some(rebuilt_tx);
            tokio::spawn(async move {
                let mut tail = Vec::new();
                loop {
                    while pause.load(std::sync::atomic::Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    match events.recv().await {
                        Some(PtyEvent::Output(bytes)) => {
                            terminal.advance(&bytes);
                            tail.extend_from_slice(&bytes);
                            if String::from_utf8_lossy(&tail).contains("flood_done")
                                && let Some(tx) = done_tx.take()
                            {
                                let _ = tx.send(());
                            }
                            if tail.len() > 8192 {
                                tail.drain(..tail.len() - 4096);
                            }
                        }
                        // As the app's pump does: in order with the output.
                        Some(PtyEvent::Rebuild(step)) => {
                            let commit = step == berth_term::Shadow::Commit;
                            terminal.shadow(step);
                            if commit
                                && done_tx.is_none()
                                && let Some(tx) = rebuilt_tx.take()
                            {
                                let _ = tx.send(());
                            }
                        }
                        Some(_) => {}
                        None => return,
                    }
                }
            })
        };

        // Past the window's ceiling (8 MB), so the pause below makes the
        // app fall behind however wide the window has grown.
        pty.write(b"seq 1 2000000; echo flood_do\"\"ne\n".to_vec())
            .await;
        // Fall behind for a while in the middle of it.
        tokio::time::sleep(Duration::from_millis(300)).await;
        pause.store(true, std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(Duration::from_secs(2)).await;
        pause.store(false, std::sync::atomic::Ordering::Relaxed);
        timeout(Duration::from_secs(60), done)
            .await
            .expect("the flood reached the app")
            .unwrap();
        // The history the app missed, once output is calm, and the terminal
        // caught up with it.
        timeout(Duration::from_secs(20), rebuilt)
            .await
            .expect("the missed history never came")
            .unwrap();
        let _ = terminal.snapshot();

        let (worker_history, worker_screen) = worker_view(&server.places, session).await;
        let app_screen = terminal.wire_screen().unwrap();
        let mut app_history: Vec<String> = terminal
            .wire_history(Some(app_screen.first_line), u32::MAX)
            .lines
            .iter()
            .map(text)
            .collect();
        app_history.reverse();
        let app_screen: Vec<String> = app_screen.lines.iter().map(text).collect();

        assert!(
            worker_history.len() > 9_000,
            "the worker kept {}",
            worker_history.len()
        );
        assert_eq!(app_screen, worker_screen, "the screens differ");
        // Line for line, newest backwards, for as far as both hold.
        let both = app_history.len().min(worker_history.len());
        let app_tail = &app_history[app_history.len() - both..];
        let worker_tail = &worker_history[worker_history.len() - both..];
        if let Some(at) = (0..both).find(|&i| app_tail[i] != worker_tail[i]) {
            panic!(
                "line {at} of the last {both} differs: app {:?}, worker {:?}",
                app_tail[at], worker_tail[at]
            );
        }
        // And as many: the last thing the app did was rebuild from the
        // worker's history, and nothing since has pruned it.
        assert_eq!(
            app_history.len(),
            worker_history.len(),
            "app {} lines, worker {}",
            app_history.len(),
            worker_history.len()
        );
    }

    /// Attach a raw client and take the screen and history it is sent first.
    async fn attached(server: &Server) -> (Client, SessionId) {
        let mut app = client::connect(&server.places).await.unwrap();
        let session = create(&mut app).await;
        app.to_server
            .send(ToServer::Attach {
                session,
                cols: 40,
                rows: 10,
            })
            .await
            .unwrap();
        loop {
            if let ToApp::History { complete: true, .. } = next(&mut app).await {
                break;
            }
        }
        (app, session)
    }

    /// A program leaving the alternate screen is followed by a screen with
    /// its history, so what lies underneath is right even for an app that
    /// only ever saw the program (D49).
    #[tokio::test(flavor = "multi_thread")]
    async fn leaving_the_alternate_screen_sends_a_screen_with_history() {
        let server = server().await;
        let (mut app, session) = attached(&server).await;
        type_line(
            &app,
            session,
            "printf '\\033[?1049h'; sleep 0.3; printf 'in_alt'; sleep 0.3; printf '\\033[?1049l'; sleep 0.3; echo back_ou\"\"t",
        )
        .await;
        let got = timeout(Duration::from_secs(10), async {
            loop {
                if let ToApp::Screen {
                    history_follows: true,
                    screen,
                    ..
                } = next(&mut app).await
                {
                    let screen: v1::Screen = screen.unpack().unwrap();
                    if !screen.modes.alternate_screen {
                        return;
                    }
                }
            }
        })
        .await;
        assert!(got.is_ok(), "no screen after leaving the alternate screen");
    }

    /// While the window is hidden nothing is sent; shown again, a screen
    /// with what happened meanwhile (D50).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hidden_window_is_sent_nothing_until_shown() {
        let server = server().await;
        let (mut app, session) = attached(&server).await;
        app.to_server
            .send(ToServer::Visible { visible: false })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        type_line(&app, session, "echo while_$((4*4))").await;
        let quiet = timeout(Duration::from_millis(800), async {
            loop {
                if let ToApp::Output { bytes, .. } = next(&mut app).await
                    && String::from_utf8_lossy(&bytes).contains("while_16")
                {
                    return;
                }
            }
        })
        .await;
        assert!(quiet.is_err(), "output reached a hidden window");

        app.to_server
            .send(ToServer::Visible { visible: true })
            .await
            .unwrap();
        loop {
            if let ToApp::Screen {
                history_follows: true,
                screen,
                ..
            } = next(&mut app).await
            {
                let screen: v1::Screen = screen.unpack().unwrap();
                let text: String = screen
                    .lines
                    .iter()
                    .flat_map(|r| r.cells.iter().map(|c| c.text.as_str()))
                    .collect();
                assert!(text.contains("while_16"), "{text}");
                break;
            }
        }
    }
}

mod claude_through_the_worker {
    //! Piece 6 (D50): Claude's hook reports to the session's worker, which
    //! keeps the state and tells the app.
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    const UUID: &str = "0b9c5f5e-7b39-4a55-8a53-2b8f0d1c9e11";

    async fn session_with_token(app: &mut Client) -> SessionId {
        app.to_server
            .send(ToServer::Create {
                name: None,
                cwd: None,
                cols: 40,
                rows: 10,
                pane: Some(TOKEN.into()),
                resume: None,
            })
            .await
            .unwrap();
        loop {
            if let ToApp::Created { session } = next(app).await {
                return session;
            }
        }
    }

    async fn claude_event(app: &mut Client) -> Option<v1::Claude> {
        timeout(Duration::from_secs(5), async {
            loop {
                if let ToApp::Event {
                    event: v1::Event::Claude(claude),
                    ..
                } = next(app).await
                {
                    return claude;
                }
            }
        })
        .await
        .ok()
    }

    fn hook(server: &Server, session: SessionId, token: &str, event: &str) {
        let line = format!("7717;berth;1;{token};{event};{UUID};-;L3RtcA");
        super::super::hook::send(&server.places.hook(session), &line).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hook_report_reaches_the_app_and_the_session_list() {
        let server = server().await;
        let mut app = client::connect(&server.places).await.unwrap();
        let session = session_with_token(&mut app).await;
        attach(&mut app, session).await;

        hook(&server, session, TOKEN, "prompt");
        let claude = claude_event(&mut app).await.expect("Claude's state");
        assert_eq!(claude.state, v1::ClaudeState::Working);

        hook(&server, session, TOKEN, "stop");
        let claude = claude_event(&mut app).await.expect("the turn ended");
        assert_eq!(claude.state, v1::ClaudeState::YourMove);

        let listed = list(&mut app).await;
        assert_eq!(
            listed[0].claude.as_ref().map(|c| c.state),
            Some(v1::ClaudeState::YourMove)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_report_without_the_token_changes_nothing() {
        let server = server().await;
        let mut app = client::connect(&server.places).await.unwrap();
        let session = session_with_token(&mut app).await;
        attach(&mut app, session).await;
        hook(
            &server,
            session,
            "ffffffffffffffffffffffffffffffff",
            "prompt",
        );
        assert!(claude_event(&mut app).await.is_none());
    }

    /// The worker keeps the state while no app is attached, and tells the
    /// next one as it attaches.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_app_that_attaches_later_is_told_what_claude_is_doing() {
        let server = server().await;
        let session = {
            let mut app = client::connect(&server.places).await.unwrap();
            session_with_token(&mut app).await
        };
        hook(&server, session, TOKEN, "prompt");
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut app = client::connect(&server.places).await.unwrap();
        attach(&mut app, session).await;
        let claude = claude_event(&mut app).await.expect("told on attach");
        assert_eq!(claude.state, v1::ClaudeState::Working);
    }
}

/// The session list is pushed as it changes: a client that never asks still
/// hears of a session starting and ending (D50).
#[tokio::test(flavor = "multi_thread")]
async fn the_session_list_is_pushed_as_it_changes() {
    let server = server().await;
    let mut watching = client::connect(&server.places).await.unwrap();
    let mut other = client::connect(&server.places).await.unwrap();

    let session = create(&mut other).await;
    let pushed = timeout(Duration::from_secs(5), async {
        loop {
            if let ToApp::Sessions(list) = next(&mut watching).await
                && list.iter().any(|s| s.id == session)
            {
                return;
            }
        }
    })
    .await;
    assert!(pushed.is_ok(), "the new session was not pushed");

    other
        .to_server
        .send(ToServer::Kill { session })
        .await
        .unwrap();
    let ended = timeout(Duration::from_secs(5), async {
        loop {
            if let ToApp::Sessions(list) = next(&mut watching).await
                && list.iter().all(|s| s.id != session)
            {
                return;
            }
        }
    })
    .await;
    assert!(ended.is_ok(), "the ended session was not pushed");
}
