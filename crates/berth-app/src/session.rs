//! Connecting a host to a terminal view, and pumping output between them.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use berth_core::Host;
use berth_core::known_hosts::TrustPolicy;
use berth_core::pty::{Pty, PtyEvent};
use berth_core::session::Session;
use berth_core::transport::Connection;
use berth_term::Terminal;
use berth_term::size::{CellSize, TermSize};

use crate::runtime;
use crate::terminal_view::{Intake, SharedTerminal, TerminalView};

/// Never gather more than this many events into one write, so a truly
/// unbounded firehose still reaches the parser in steps. Zed uses the same cap.
pub const COALESCE_MAX_EVENTS: usize = 100;

/// The most output handed to the parser and not yet parsed before the pump
/// stops reading. A producer faster than the parser is then held back by the
/// pty or the ssh channel, as a terminal should hold it back, rather than
/// queueing without bound. Under 10 ms of parsing on the slowest measured
/// stream on macOS.
pub const MAX_BACKLOG: usize = 512 * 1024;

pub struct Connected {
    pub connection: Arc<Connection>,
    pub pty: Pty,
    pub terminal: SharedTerminal,
    pub events: mpsc::Receiver<PtyEvent>,
}

/// Just the transport: a connection and a tmux session attached to it.
///
/// Separate from [`connect`] because a reconnect must not build a terminal.
/// The grid on screen is the one Berth already holds, and it stays there while
/// the transport is rebuilt underneath it.
pub struct Attached {
    pub connection: Arc<Connection>,
    pub pty: Pty,
    pub events: mpsc::Receiver<PtyEvent>,
}

/// A shell on this machine.
///
/// Deliberately not a `Connected`: there is no connection, and a field holding
/// a fake one would be a lie the rest of the app would have to keep believing.
pub struct Local {
    pub pty: Pty,
    pub terminal: SharedTerminal,
    pub events: mpsc::Receiver<PtyEvent>,
}

/// Open a shell on this machine, with the same terminal a remote one gets.
pub async fn open_local(
    cols: u16,
    rows: u16,
    palette: berth_term::terminal::Palette,
    pane: berth_core::claude::PaneEnv,
) -> berth_core::Result<Local> {
    let (pty, events) = berth_core::local::open(cols, rows, Some(&pane)).await?;
    Ok(Local {
        pty,
        terminal: terminal_for(cols, rows, palette),
        events,
    })
}

/// What a host offered when asked for its key, without being trusted.
pub enum Offer {
    /// Nothing is recorded for this host. The key to put in front of the user,
    /// and the one written if they accept it.
    Unknown {
        key: Box<berth_core::known_hosts::PublicKey>,
        fingerprint: String,
    },
    /// Recorded and matching after all, so there is nothing to ask.
    Trusted,
    /// A different key is recorded. Never a question and never offered as one.
    Changed { line: usize, fingerprint: String },
}

/// Ask a host what key it presents, so the user can be shown a fingerprint.
///
/// A host already in known_hosts is answered from the file without touching the
/// network. Only an unrecorded one is probed, and that probe refuses whatever it
/// is shown, so nothing is authenticated and no session is left open behind a
/// prompt the user has not answered yet.
pub async fn scout(host: Host) -> berth_core::Result<Offer> {
    if berth_core::known_hosts::is_known(&host.hostname, host.port) {
        return Ok(Offer::Trusted);
    }
    match Connection::offered_key(&host).await? {
        (berth_core::known_hosts::Verdict::Trusted, _) => Ok(Offer::Trusted),
        (berth_core::known_hosts::Verdict::Unknown { fingerprint }, key) => Ok(Offer::Unknown {
            key: Box::new(key),
            fingerprint,
        }),
        (berth_core::known_hosts::Verdict::Changed { line, fingerprint }, _) => {
            Ok(Offer::Changed { line, fingerprint })
        }
    }
}

/// Record the key the user just accepted.
///
/// Written straight to known_hosts, in OpenSSH's own format and to the file
/// `ssh` reads. The key recorded is the one whose fingerprint was displayed,
/// not one fetched again afterwards, so what the user approved and what is
/// stored cannot differ. If the server presents something else on the next
/// connect, that is a changed key and fails loudly.
pub fn trust(host: &Host, key: &berth_core::known_hosts::PublicKey) -> berth_core::Result<()> {
    berth_core::known_hosts::learn(&host.hostname, host.port, key)
        .map_err(|e| berth_core::Error::KnownHosts(e.to_string()))
}

/// Open a connection and attach to the session named by `slug`.
///
/// `Session::new` uses `new-session -A`, so the same slug lands in the same
/// tmux session whether this is the first attach or the fifth. That is what
/// makes a reconnect resume the same shell rather than start another one.
pub async fn attach(
    host: Host,
    slug: String,
    cols: u16,
    rows: u16,
) -> berth_core::Result<Attached> {
    let connection = Connection::connect(&host, TrustPolicy::KnownHostsOnly).await?;
    let session = Session::new(slug, cols, rows);
    let (pty, events) = connection.open_session(&session).await?;
    Ok(Attached {
        connection: Arc::new(connection),
        pty,
        events,
    })
}

/// Attach another tmux session over a connection that is already open.
///
/// Splitting a host, or reattaching several of its sessions at once, must not
/// present sshd with a handshake per session: `MaxStartups` is 10 unauthenticated
/// connections by default, and a workspace of panes would trip it. One
/// connection per host with a channel per session is the shape Berth has always
/// claimed, and this is what makes it true.
pub async fn rejoin(
    connection: Arc<Connection>,
    slug: String,
    cols: u16,
    rows: u16,
) -> berth_core::Result<Attached> {
    let session = Session::new(slug, cols, rows);
    let (pty, events) = connection.open_session(&session).await?;
    Ok(Attached {
        connection,
        pty,
        events,
    })
}

/// As [`connect`], over a connection that is already open.
///
/// What splitting a host uses: a new channel and a new terminal, no handshake.
pub async fn open_on(
    connection: Arc<Connection>,
    slug: String,
    cols: u16,
    rows: u16,
    palette: berth_term::terminal::Palette,
) -> berth_core::Result<Connected> {
    let attached = rejoin(connection, slug, cols, rows).await?;
    Ok(Connected {
        connection: attached.connection,
        pty: attached.pty,
        terminal: terminal_for(cols, rows, palette),
        events: attached.events,
    })
}

/// Connect, attach to a tmux session, and prepare a terminal for it.
pub async fn connect(
    host: Host,
    slug: String,
    cols: u16,
    rows: u16,
    palette: berth_term::terminal::Palette,
) -> berth_core::Result<Connected> {
    let attached = attach(host, slug, cols, rows).await?;
    Ok(Connected {
        connection: attached.connection,
        pty: attached.pty,
        terminal: terminal_for(cols, rows, palette),
        events: attached.events,
    })
}

/// The palette comes from TerminalStyle, so every visual constant stays in one
/// struct rather than half of them living in berth-term's defaults.
pub(crate) fn terminal_for(
    cols: u16,
    rows: u16,
    palette: berth_term::terminal::Palette,
) -> SharedTerminal {
    let term = Terminal::new(TermSize::new(cols, rows), CellSize::new(8.0, 16.0));
    term.set_palette(palette);
    Arc::new(term)
}

/// Hand PTY output to the parser as it arrives, on a thread of its own.
///
/// Nothing here touches the UI thread: the terminal parses on its own thread
/// and publishes screens, and the view repaints when it does. So heavy output
/// never keeps the window from drawing (D49), and an echo reaches the screen
/// without waiting for the UI thread to get round to it.
fn pump(terminal: SharedTerminal, intake: Arc<Intake>, mut events: mpsc::Receiver<PtyEvent>) {
    while let Some(first) = events.blocking_recv() {
        let (batch, closed) = take_batch(first, &mut events);
        let mut bytes = Vec::new();
        for event in batch {
            if let PtyEvent::Output(chunk) = event {
                bytes.extend_from_slice(&chunk);
            }
        }
        if !bytes.is_empty() {
            let seq = terminal.advance(&bytes);
            intake.arrived(bytes.len(), seq);
            terminal.wait_for_room(MAX_BACKLOG);
        }
        // The last holder: the view is gone and nobody will see this.
        if closed || Arc::strong_count(&terminal) == 1 {
            break;
        }
    }
}

/// Take `first` plus whatever is *already* queued, and never wait for more.
///
/// This is the whole batching policy, extracted so it can be tested without a
/// window. Two properties matter and they pull in opposite directions:
///
/// - A lone event must go through immediately. Waiting out a fixed window, as
///   an earlier version did, adds the window's full length to every keystroke
///   echo, which has nothing to batch against.
/// - A burst must still batch. It does, because a firehose refills the queue
///   faster than this drains it, so `try_recv` keeps returning until the cap.
///
/// Painting more often than a coalescing window costs nothing, because
/// `cx.notify()` already collapses repaints to one per frame.
fn take_batch(first: PtyEvent, events: &mut mpsc::Receiver<PtyEvent>) -> (Vec<PtyEvent>, bool) {
    let mut closed = matches!(first, PtyEvent::Closed);
    let mut batch = vec![first];

    while !closed && batch.len() < COALESCE_MAX_EVENTS {
        match events.try_recv() {
            Ok(event) => {
                closed = matches!(event, PtyEvent::Closed);
                batch.push(event);
            }
            Err(mpsc::error::TryRecvError::Empty) => break,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                closed = true;
                break;
            }
        }
    }
    (batch, closed)
}

/// Start the pump for a freshly connected session.
pub fn start_pump(view: &TerminalView, events: mpsc::Receiver<PtyEvent>) {
    let terminal = view.terminal();
    let intake = view.intake.clone();
    let spawned = std::thread::Builder::new()
        .name("berth-pump".into())
        .spawn(move || pump(terminal, intake, events));
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not start the output pump");
    }
}

/// Measure the round trip by timing a trivial exec, for the status bar and for
/// the predictive-echo threshold in M4.
pub async fn round_trip(connection: Arc<Connection>) -> Option<Duration> {
    let start = std::time::Instant::now();
    let out = connection.exec("true").await.ok()?;
    out.succeeded().then(|| start.elapsed())
}

/// Bring up every forward a host declares, on a fresh connection.
///
/// One that cannot bind is logged and skipped rather than failing the whole
/// reconnect: a port taken by something else on this machine is not a reason to
/// refuse the session, and the shell matters more than the tunnel.
pub async fn start_forwards(
    connection: Arc<berth_core::transport::Connection>,
    forwards: &[berth_core::forward::Forward],
) -> Vec<berth_core::forward::Forwarder> {
    let mut started = Vec::new();
    for forward in forwards {
        match runtime::spawn(berth_core::forward::Forwarder::start(
            connection.clone(),
            forward.clone(),
        ))
        .await
        {
            Ok(Ok(forwarder)) => started.push(forwarder),
            Ok(Err(error)) => tracing::warn!(%forward, %error, "forward could not be restored"),
            Err(_) => tracing::warn!(%forward, "the runtime dropped the forward task"),
        }
    }
    started
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_event_cap_matches_zeds() {
        assert_eq!(COALESCE_MAX_EVENTS, 100);
    }

    fn output(n: u8) -> PtyEvent {
        PtyEvent::Output(vec![n])
    }

    /// Regression guard for the keystroke-latency bug: a lone echo must not be
    /// held back waiting for company that is not coming.
    #[tokio::test]
    async fn a_lone_event_is_taken_on_its_own() {
        let (tx, mut rx) = mpsc::channel(8);
        tx.send(output(1)).await.unwrap();
        let first = rx.recv().await.unwrap();

        let started = std::time::Instant::now();
        let (batch, closed) = take_batch(first, &mut rx);

        assert_eq!(batch.len(), 1);
        assert!(!closed);
        assert!(
            started.elapsed() < Duration::from_millis(1),
            "took {:?}; a lone event must not wait on a timer",
            started.elapsed()
        );
    }

    /// Regression guard for the other direction: removing the timer must not
    /// cost batching, or the 50 MB cat frame budget goes with it.
    #[tokio::test]
    async fn a_burst_is_batched_in_one_take() {
        let (tx, mut rx) = mpsc::channel(64);
        for i in 0..50 {
            tx.send(output(i)).await.unwrap();
        }
        let first = rx.recv().await.unwrap();
        let (batch, _) = take_batch(first, &mut rx);
        assert_eq!(batch.len(), 50, "a queued burst must come out in one batch");
    }

    #[tokio::test]
    async fn a_batch_never_exceeds_the_cap() {
        let (tx, mut rx) = mpsc::channel(COALESCE_MAX_EVENTS * 2);
        for i in 0..(COALESCE_MAX_EVENTS + 40) {
            tx.send(output(i as u8)).await.unwrap();
        }
        let first = rx.recv().await.unwrap();
        let (batch, _) = take_batch(first, &mut rx);
        assert_eq!(batch.len(), COALESCE_MAX_EVENTS);
    }

    #[tokio::test]
    async fn a_close_ends_the_batch_and_is_reported() {
        let (tx, mut rx) = mpsc::channel(8);
        tx.send(output(1)).await.unwrap();
        tx.send(PtyEvent::Closed).await.unwrap();
        tx.send(output(2)).await.unwrap();
        let first = rx.recv().await.unwrap();
        let (batch, closed) = take_batch(first, &mut rx);
        assert!(closed);
        assert_eq!(batch.len(), 2, "nothing is taken past the close");
    }

    #[tokio::test]
    async fn a_dropped_sender_reports_closed() {
        let (tx, mut rx) = mpsc::channel(8);
        tx.send(output(1)).await.unwrap();
        let first = rx.recv().await.unwrap();
        drop(tx);
        let (_, closed) = take_batch(first, &mut rx);
        assert!(closed);
    }
}
