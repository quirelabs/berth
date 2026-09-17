//! A/B diagnostics: what survives tmux and what does not.
//!
//! Berth sessions are tmux-backed, so anything the client relies on has to be
//! checked on both sides of tmux, not just against a raw shell.

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

/// Pump for a while, feeding everything into a terminal.
async fn pump(terminal: &mut Terminal, rx: &mut mpsc::Receiver<PtyEvent>, ms: u64) {
    let deadline = std::time::Instant::now() + Duration::from_millis(ms);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return;
        }
        match timeout(left, rx.recv()).await {
            Ok(Some(PtyEvent::Output(bytes))) => terminal.advance(&bytes),
            Ok(Some(_)) | Ok(None) => return,
            Err(_) => return,
        }
    }
}

/// Does shell integration survive tmux, and does the alternate screen read
/// correctly through it?
#[tokio::test]
async fn what_survives_tmux() {
    let host = host_or_skip!();
    let conn = Connection::connect(&host, TrustPolicy::KnownHostsOnly)
        .await
        .unwrap();

    // Put the snippet on the host.
    let push = format!(
        "cat > /tmp/berth_si.sh <<'BERTH_EOF'\n{}\nBERTH_EOF",
        berth_probe::SHELL_INTEGRATION
            .replace("\r\n", "\n")
            .trim_end()
    );
    assert!(conn.exec(&push).await.unwrap().succeeded());

    // --- A: a plain shell, no tmux ---
    let (pty, mut rx) = conn.open_shell(100, 30).await.unwrap();
    let mut plain = Terminal::new(TermSize::new(100, 30), CellSize::new(8.0, 16.0));
    pty.write("bash --norc --noprofile -i\n").await;
    pump(&mut plain, &mut rx, 1200).await;
    pty.write("source /tmp/berth_si.sh\n").await;
    pump(&mut plain, &mut rx, 1200).await;
    plain.drain_shell_events();
    let alt_before_plain = plain.alternate_screen();
    pty.write("(exit 7)\n").await;
    pump(&mut plain, &mut rx, 1500).await;
    let plain_events = plain.drain_shell_events();
    pty.write("vim -u NONE\n").await;
    pump(&mut plain, &mut rx, 2500).await;
    let alt_in_vim_plain = plain.alternate_screen();
    pty.write(":q!\n").await;
    pump(&mut plain, &mut rx, 1500).await;
    let alt_after_vim_plain = plain.alternate_screen();
    pty.close().await;

    // --- B: the same through tmux, which is how Berth actually runs ---
    let session = Session::new("abdiag", 100, 30);
    let (pty2, mut rx2) = conn.open_session(&session).await.unwrap();
    let mut through = Terminal::new(TermSize::new(100, 30), CellSize::new(8.0, 16.0));
    pump(&mut through, &mut rx2, 1500).await;
    let alt_after_attach = through.alternate_screen();
    pty2.write("bash --norc --noprofile -i\n").await;
    pump(&mut through, &mut rx2, 1200).await;
    pty2.write("source /tmp/berth_si.sh\n").await;
    pump(&mut through, &mut rx2, 1200).await;
    through.drain_shell_events();
    pty2.write("(exit 7)\n").await;
    pump(&mut through, &mut rx2, 1500).await;
    let tmux_events = through.drain_shell_events();
    pty2.write("vim -u NONE\n").await;
    pump(&mut through, &mut rx2, 2500).await;
    let alt_in_vim_tmux = through.alternate_screen();
    pty2.write(":q!\n").await;
    pump(&mut through, &mut rx2, 1500).await;
    let alt_after_vim_tmux = through.alternate_screen();
    pty2.write("exit\n").await;
    pty2.close().await;

    println!("\n================ A/B: what survives tmux ================");
    println!("plain shell  shell events: {plain_events:?}");
    println!("through tmux shell events: {tmux_events:?}");
    println!(
        "plain shell  alternate screen: before={alt_before_plain} in_vim={alt_in_vim_plain} after={alt_after_vim_plain}"
    );
    println!(
        "through tmux alternate screen: after_attach={alt_after_attach} in_vim={alt_in_vim_tmux} after={alt_after_vim_tmux}"
    );
    println!("=========================================================\n");

    let _ = conn.exec("tmux kill-session -t 'berth/abdiag'").await;
    let _ = conn.exec("rm -f /tmp/berth_si.sh").await;
    conn.disconnect().await.unwrap();

    // The plain case is the control: if this fails, the snippet or the parser
    // is broken and the tmux comparison means nothing.
    assert!(
        !plain_events.is_empty(),
        "the control case must produce markers"
    );
}
