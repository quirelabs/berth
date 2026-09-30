//! Claude Code, live: the shim, the hook and the parser, end to end (D48).
//!
//! Skipped unless `BERTH_TEST_CLAUDE=1`, because it runs the real `claude` and
//! spends a few tokens on the account it is logged into:
//!
//!     BERTH_TEST_CLAUDE=1 cargo test -p berth-core --test claude -- --nocapture
//!
//! Nothing is installed into Claude's config: the plugin and the shim go in a
//! temp directory, and the shim loads the plugin with `--plugin-dir`.
//! `--no-session-persistence` keeps the run out of Claude's own history.

#![cfg(unix)]

use std::time::Duration;

use berth_core::claude::{self, Event, PaneEnv, PaneToken, State, Tracker};
use berth_core::pty::PtyEvent;
use berth_term::size::{CellSize, TermSize};
use berth_term::terminal::Terminal;
use std::path::Path;
use tokio::time::timeout;

fn enabled() -> bool {
    std::env::var(berth_core::env::TEST_CLAUDE).ok().as_deref() == Some("1")
}

/// Run one print-mode prompt in a Berth pane and return everything the pane
/// received, as a terminal would.
async fn run_in_pane(pane: &PaneEnv, dir: &std::path::Path) -> Terminal {
    let (pty, mut events) =
        berth_core::local::open_with("/bin/sh", false, Some(dir), Some(pane), 120, 40)
            .await
            .expect("a local shell should open");
    // The marker is computed by the shell, so the echo of this line cannot be
    // mistaken for it.
    assert!(
        pty.write(
            "claude -p 'Reply with just the word ok.' --no-session-persistence; \
             echo berth-claude-$((20+22))\n"
        )
        .await
    );

    let term = Terminal::new(TermSize::new(120, 40), CellSize::new(8.0, 16.0));
    let mut seen = String::new();
    let deadline = Duration::from_secs(120);
    while let Ok(Some(event)) = timeout(deadline, events.recv()).await {
        if let PtyEvent::Output(bytes) = event {
            term.advance(&bytes);
            seen.push_str(&String::from_utf8_lossy(&bytes));
            if seen.contains("berth-claude-42") {
                break;
            }
        }
    }
    pty.close().await;
    assert!(
        seen.contains("berth-claude-42"),
        "claude never finished: {seen:?}"
    );
    term
}

#[tokio::test]
async fn claude_reports_through_the_shim_and_the_hook() {
    if !enabled() {
        eprintln!("skipped: set BERTH_TEST_CLAUDE=1 to run");
        return;
    }
    let base = tempfile::tempdir().expect("a temp dir");
    claude::install(base.path()).expect("plugin and shim in place");
    let work = tempfile::tempdir().expect("a work dir");
    let pane = PaneEnv::new(Some(base.path()));

    let term = run_in_pane(&pane, work.path()).await;
    let payloads = term.drain_agent_signals();
    let signals: Vec<_> = payloads
        .iter()
        .filter_map(|p| claude::parse(p, &pane.token))
        .collect();
    assert_eq!(
        signals.len(),
        payloads.len(),
        "every signal from the real hook parses: {payloads:?}"
    );

    let events: Vec<Event> = signals.iter().map(|s| s.event).collect();
    assert_eq!(
        events,
        [Event::Start, Event::Prompt, Event::Stop, Event::End],
        "the order seen in the live checks"
    );

    let mut tracker = Tracker::default();
    for signal in &signals {
        tracker.apply(signal);
    }
    assert_eq!(tracker.state, Some(State::Finished));
    assert!(claude::is_session_id(
        tracker.session_id.as_deref().unwrap_or("")
    ));
    assert_eq!(
        tracker.cwd.as_ref().and_then(|d| d.canonicalize().ok()),
        work.path().canonicalize().ok(),
        "the directory resume is scoped to"
    );
}

/// The control: the same run with another pane's token is believed not at
/// all, which is what keeps a cat'ed log from moving any state.
#[tokio::test]
async fn another_panes_signals_are_not_believed() {
    if !enabled() {
        eprintln!("skipped: set BERTH_TEST_CLAUDE=1 to run");
        return;
    }
    let base = tempfile::tempdir().expect("a temp dir");
    claude::install(base.path()).expect("plugin and shim in place");
    let work = tempfile::tempdir().expect("a work dir");
    let pane = PaneEnv::new(Some(base.path()));

    let term = run_in_pane(&pane, work.path()).await;
    let payloads = term.drain_agent_signals();
    assert!(!payloads.is_empty(), "the hook did write");
    let stranger = PaneToken::new();
    assert!(
        payloads
            .iter()
            .all(|p| claude::parse(p, &stranger).is_none())
    );
}

/// With no `BERTH_PANE`, the hook writes nothing at all.
#[tokio::test]
async fn outside_a_berth_pane_the_hook_is_silent() {
    if !enabled() {
        eprintln!("skipped: set BERTH_TEST_CLAUDE=1 to run");
        return;
    }
    let base = tempfile::tempdir().expect("a temp dir");
    claude::install(base.path()).expect("plugin and shim in place");
    let work = tempfile::tempdir().expect("a work dir");
    let (pty, mut events) =
        berth_core::local::open_with("/bin/sh", false, Some(work.path()), None, 120, 40)
            .await
            .expect("a local shell should open");
    let plugin = base.path().join("claude-plugin");
    let line = format!(
        "claude -p 'Reply with just the word ok.' --no-session-persistence --plugin-dir '{}'; echo berth-claude-$((20+22))\n",
        plugin.display()
    );
    assert!(pty.write(line).await);
    let term = Terminal::new(TermSize::new(120, 40), CellSize::new(8.0, 16.0));
    let mut seen = String::new();
    while let Ok(Some(event)) = timeout(Duration::from_secs(120), events.recv()).await {
        if let PtyEvent::Output(bytes) = event {
            term.advance(&bytes);
            seen.push_str(&String::from_utf8_lossy(&bytes));
            if seen.contains("berth-claude-42") {
                break;
            }
        }
    }
    pty.close().await;
    assert!(
        seen.contains("berth-claude-42"),
        "claude never finished: {seen:?}"
    );
    assert!(term.drain_agent_signals().is_empty());
}

/// Ask a shell started the way Berth starts it where `claude` resolves, after
/// all of its startup files have run.
async fn resolve_claude(shell: &str, login: bool, pane: &PaneEnv) -> String {
    let (pty, mut events) = berth_core::local::open_with(shell, login, None, Some(pane), 120, 40)
        .await
        .expect("a shell should open");
    assert!(
        pty.write(
            "printf 'resolved=%s;zdotdir=%s;\\n' \"$(command -v claude)\" \"${ZDOTDIR-unset}\"\n"
        )
        .await
    );
    let mut seen = String::new();
    while let Ok(Some(event)) = timeout(Duration::from_secs(30), events.recv()).await {
        if let PtyEvent::Output(bytes) = event {
            seen.push_str(&String::from_utf8_lossy(&bytes));
            if seen.contains("resolved=/") && seen.contains(";\r") {
                break;
            }
        }
    }
    pty.close().await;
    seen
}

fn resolved(seen: &str) -> Option<&str> {
    let start = seen.rfind("resolved=")? + "resolved=".len();
    seen[start..].split(';').next()
}

fn zdotdir(seen: &str) -> Option<&str> {
    let start = seen.rfind("zdotdir=")? + "zdotdir=".len();
    seen[start..].split(';').next()
}

/// The user's own login zsh, with their real startup files: after all of them
/// have run, `claude` must still be the shim. This is the check that failed
/// before the wrapper existed, on a Mac whose `.zshrc` puts `~/.local/bin`
/// first and whose `/etc/zprofile` runs `path_helper`.
#[tokio::test]
async fn the_users_login_zsh_keeps_the_shim_first() {
    if !enabled() {
        eprintln!("skipped: set BERTH_TEST_CLAUDE=1 to run");
        return;
    }
    let Some(zsh) = ["/bin/zsh", "/usr/bin/zsh"]
        .into_iter()
        .find(|p| Path::new(p).exists())
    else {
        eprintln!("skipped: no zsh");
        return;
    };
    let base = tempfile::tempdir().expect("a temp dir");
    claude::install(base.path()).expect("support files in place");
    let pane = PaneEnv::new(Some(base.path()));
    let seen = resolve_claude(zsh, true, &pane).await;
    let shim = claude::shim_dir(base.path()).join("claude");
    assert_eq!(
        resolved(&seen).map(|p| Path::new(p).canonicalize().ok()),
        Some(shim.canonicalize().ok()),
        "claude resolves to the shim: {seen:?}"
    );
    // ZDOTDIR is handed back, so a zsh started inside is an ordinary one.
    let expected = if pane.user_zdotdir_was_set {
        pane.user_zdotdir.as_ref().map(|d| d.display().to_string())
    } else {
        Some("unset".to_string())
    };
    assert_eq!(zdotdir(&seen).map(str::to_string), expected);
}

/// The same for bash, which Berth starts on its own init file.
#[tokio::test]
async fn bash_keeps_the_shim_first() {
    if !enabled() {
        eprintln!("skipped: set BERTH_TEST_CLAUDE=1 to run");
        return;
    }
    let base = tempfile::tempdir().expect("a temp dir");
    claude::install(base.path()).expect("support files in place");
    let pane = PaneEnv::new(Some(base.path()));
    let seen = resolve_claude("/bin/bash", true, &pane).await;
    let shim = claude::shim_dir(base.path()).join("claude");
    assert_eq!(
        resolved(&seen).map(|p| Path::new(p).canonicalize().ok()),
        Some(shim.canonicalize().ok()),
        "claude resolves to the shim: {seen:?}"
    );
}

/// The control for the two above: the same user zsh without Berth's wrapper.
/// On a machine whose startup files reorder PATH, as the author's do, the
/// shim loses, which is what shows the check can fail at all. Where nothing
/// reorders PATH the shim wins either way and this says so rather than fail.
#[tokio::test]
async fn without_the_wrapper_the_users_zsh_can_lose_the_shim() {
    if !enabled() {
        eprintln!("skipped: set BERTH_TEST_CLAUDE=1 to run");
        return;
    }
    let Some(zsh) = ["/bin/zsh", "/usr/bin/zsh"]
        .into_iter()
        .find(|p| Path::new(p).exists())
    else {
        eprintln!("skipped: no zsh");
        return;
    };
    let base = tempfile::tempdir().expect("a temp dir");
    claude::install(base.path()).expect("support files in place");
    let pane = PaneEnv {
        shell_dir: None,
        ..PaneEnv::new(Some(base.path()))
    };
    let seen = resolve_claude(zsh, true, &pane).await;
    let shim = claude::shim_dir(base.path()).join("claude");
    let won =
        resolved(&seen).map(|p| Path::new(p).canonicalize().ok()) == Some(shim.canonicalize().ok());
    eprintln!(
        "without the wrapper the shim {}",
        if won {
            "still won: nothing here reorders PATH"
        } else {
            "lost, as expected here"
        }
    );
}

/// A shell Berth starts never sees a parent Claude session's markers. When
/// this suite runs from inside Claude Code, the test process has them, so this
/// checks the removal for real; elsewhere it passes trivially.
#[tokio::test]
async fn a_local_shell_never_sees_a_parent_claude_session() {
    if !enabled() {
        eprintln!("skipped: set BERTH_TEST_CLAUDE=1 to run");
        return;
    }
    let inside = std::env::var_os("CLAUDE_CODE_CHILD_SESSION").is_some();
    let (pty, mut events) = berth_core::local::open_with("/bin/sh", false, None, None, 120, 40)
        .await
        .expect("a shell should open");
    assert!(
        pty.write(
            "printf 'marker=%s;\\n' \"${CLAUDE_CODE_CHILD_SESSION-unset}${CLAUDECODE-unset}\"\n"
        )
        .await
    );
    let mut seen = String::new();
    while let Ok(Some(event)) = timeout(Duration::from_secs(10), events.recv()).await {
        if let PtyEvent::Output(bytes) = event {
            seen.push_str(&String::from_utf8_lossy(&bytes));
            if seen.contains("marker=u") || seen.contains("marker=1") {
                break;
            }
        }
    }
    pty.close().await;
    eprintln!("parent has the markers: {inside}");
    assert!(seen.contains("marker=unsetunset;"), "{seen:?}");
}

/// Resume, end to end: a conversation saved to disk, then Berth's own resume
/// line typed into the user's real login zsh the moment it starts, the way a
/// restored tab does it. Claude must come back with that conversation.
///
/// The line is typed with `-p` added so the check can read an answer without
/// driving the interactive screen; everything that is Berth's (typeahead into
/// a shell still running its startup files, the shim, the id, the directory)
/// is the same. Uses a fixed work directory so repeated runs keep to one
/// project folder in Claude's own history.
#[tokio::test]
async fn a_saved_conversation_resumes_in_a_fresh_login_shell() {
    if !enabled() {
        eprintln!("skipped: set BERTH_TEST_CLAUDE=1 to run");
        return;
    }
    let Some(zsh) = ["/bin/zsh", "/usr/bin/zsh"]
        .into_iter()
        .find(|p| Path::new(p).exists())
    else {
        eprintln!("skipped: no zsh");
        return;
    };
    let base = tempfile::tempdir().expect("a temp dir");
    claude::install(base.path()).expect("support files in place");
    let work = std::env::temp_dir().join("berth-live-claude-resume");
    std::fs::create_dir_all(&work).expect("work dir");

    // A conversation that is saved, in the work directory.
    let pane = PaneEnv::new(Some(base.path()));
    let (pty, mut events) =
        berth_core::local::open_with("/bin/sh", false, Some(&work), Some(&pane), 120, 40)
            .await
            .expect("a shell");
    assert!(
        pty.write(
            "claude -p 'Remember the word marzipan. Reply with just ok.'; echo berth-claude-$((20+22))\n"
        )
        .await
    );
    let term = Terminal::new(TermSize::new(120, 40), CellSize::new(8.0, 16.0));
    let mut seen = String::new();
    while let Ok(Some(event)) = timeout(Duration::from_secs(120), events.recv()).await {
        if let PtyEvent::Output(bytes) = event {
            term.advance(&bytes);
            seen.push_str(&String::from_utf8_lossy(&bytes));
            if seen.contains("berth-claude-42") {
                break;
            }
        }
    }
    pty.close().await;
    let session = term
        .drain_agent_signals()
        .iter()
        .find_map(|p| claude::parse(p, &pane.token))
        .map(|s| s.session_id)
        .expect("the first run reported its session");

    // A fresh login zsh in that directory, with the resume line typed before
    // its startup files have finished, exactly as a restored tab does.
    let pane = PaneEnv::new(Some(base.path()));
    let (pty, mut events) =
        berth_core::local::open_with(zsh, true, Some(&work), Some(&pane), 120, 40)
            .await
            .expect("a login zsh");
    let line = claude::resume_line(&session).expect("a uuid");
    assert!(
        pty.write(format!(
            "{line} -p 'What word did I ask you to remember? One word.'; echo berth-claude-$((20+22))\r"
        ))
        .await
    );
    let term = Terminal::new(TermSize::new(120, 40), CellSize::new(8.0, 16.0));
    let mut seen = String::new();
    while let Ok(Some(event)) = timeout(Duration::from_secs(120), events.recv()).await {
        if let PtyEvent::Output(bytes) = event {
            term.advance(&bytes);
            seen.push_str(&String::from_utf8_lossy(&bytes));
            if seen.contains("berth-claude-42") {
                break;
            }
        }
    }
    pty.close().await;
    let signals: Vec<_> = term
        .drain_agent_signals()
        .iter()
        .filter_map(|p| claude::parse(p, &pane.token))
        .collect();
    assert!(
        signals
            .iter()
            .any(|s| s.event == Event::Start && s.session_id == session),
        "the resumed session reported under its own id: {signals:?}"
    );
    assert!(
        seen.to_lowercase().contains("marzipan"),
        "the conversation came back: {seen:?}"
    );
}

/// The way zsh and bash resume: the id goes in with the shell, and Berth's
/// startup wrapper runs `claude --resume` before the first prompt. Nothing is
/// typed, so the typed line must never appear on screen, and the process under
/// the shell must be Claude resuming that exact session through the shim.
///
/// Checked on the process rather than on Claude's own report, because the work
/// directory was never trusted interactively and Claude stops at its trust
/// question before starting a session; trusting it would mean writing to the
/// user's Claude config.
#[tokio::test]
async fn zsh_resumes_by_itself_before_its_first_prompt() {
    if !enabled() {
        eprintln!("skipped: set BERTH_TEST_CLAUDE=1 to run");
        return;
    }
    let Some(zsh) = ["/bin/zsh", "/usr/bin/zsh"]
        .into_iter()
        .find(|p| Path::new(p).exists())
    else {
        eprintln!("skipped: no zsh");
        return;
    };
    let base = tempfile::tempdir().expect("a temp dir");
    claude::install(base.path()).expect("support files in place");
    let work = std::env::temp_dir().join("berth-live-claude-resume");
    std::fs::create_dir_all(&work).expect("work dir");

    // A saved conversation to come back to.
    let pane = PaneEnv::new(Some(base.path()));
    let (pty, mut events) =
        berth_core::local::open_with("/bin/sh", false, Some(&work), Some(&pane), 120, 40)
            .await
            .expect("a shell");
    assert!(
        pty.write("claude -p 'Reply with just ok.'; echo berth-claude-$((20+22))\n")
            .await
    );
    let term = Terminal::new(TermSize::new(120, 40), CellSize::new(8.0, 16.0));
    let mut seen = String::new();
    while let Ok(Some(event)) = timeout(Duration::from_secs(120), events.recv()).await {
        if let PtyEvent::Output(bytes) = event {
            term.advance(&bytes);
            seen.push_str(&String::from_utf8_lossy(&bytes));
            if seen.contains("berth-claude-42") {
                break;
            }
        }
    }
    pty.close().await;
    let session = term
        .drain_agent_signals()
        .iter()
        .find_map(|p| claude::parse(p, &pane.token))
        .map(|s| s.session_id)
        .expect("the first run reported its session");

    let pane = PaneEnv::new(Some(base.path())).resuming(&session);
    assert!(berth_core::local::resumes_itself(&pane) || !login_shell_is_zsh_or_bash());
    let (pty, mut events) =
        berth_core::local::open_with(zsh, true, Some(&work), Some(&pane), 120, 40)
            .await
            .expect("a login zsh");
    let shell_pid = pty.local_pid().expect("a local shell has a pid");
    let mut seen = String::new();
    let mut running = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while running.is_none() && std::time::Instant::now() < deadline {
        if let Ok(Some(PtyEvent::Output(bytes))) =
            timeout(Duration::from_millis(500), events.recv()).await
        {
            seen.push_str(&String::from_utf8_lossy(&bytes));
        }
        running = child_command(shell_pid).filter(|c| c.contains("--resume"));
    }
    pty.close().await;
    let running = running.expect("claude started under the shell");
    assert!(
        running.contains(&format!("--resume {session}")),
        "the saved session, not another: {running}"
    );
    assert!(
        running.contains("--plugin-dir"),
        "through the shim: {running}"
    );
    // What the screen showed is Claude's own start, never a typed line or a
    // prompt before it.
    assert!(
        !seen.contains("claude --resume"),
        "nothing was typed: {seen:?}"
    );
}

/// The command line of a process whose parent is `pid`, if there is one.
fn child_command(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-A", "-o", "ppid=,command="])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| {
            let line = line.trim_start();
            let (ppid, command) = line.split_once(' ')?;
            (ppid.parse::<u32>().ok()? == pid).then(|| command.trim().to_string())
        })
}

fn login_shell_is_zsh_or_bash() -> bool {
    let shell = berth_core::local::login_shell();
    shell.ends_with("/zsh") || shell.ends_with("/bash")
}
