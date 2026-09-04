//! Connecting a host to a terminal view, and pumping output between them.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{AsyncApp, Context, Entity, WeakEntity};
use tokio::sync::mpsc;

use berth_core::Host;
use berth_core::known_hosts::TrustPolicy;
use berth_core::pty::{Pty, PtyEvent};
use berth_core::session::Session;
use berth_core::transport::Connection;
use berth_term::Terminal;
use berth_term::size::{CellSize, TermSize};

use crate::terminal_view::{SharedTerminal, TerminalView};

/// Never gather more than this many events before painting, so a truly
/// unbounded firehose still yields to the UI. Zed uses the same cap.
pub const COALESCE_MAX_EVENTS: usize = 100;

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

/// Connect, attach to a tmux session, and prepare a terminal for it.
pub async fn connect(
    host: Host,
    slug: String,
    cols: u16,
    rows: u16,
    palette: berth_term::terminal::Palette,
) -> berth_core::Result<Connected> {
    let attached = attach(host, slug, cols, rows).await?;
    // The palette comes from TerminalStyle, so every visual constant stays in
    // one struct rather than half of them living in berth-term's defaults.
    let mut term = Terminal::new(TermSize::new(cols, rows), CellSize::new(8.0, 16.0));
    term.set_palette(palette);
    let terminal: SharedTerminal = Arc::new(Mutex::new(term));
    Ok(Connected {
        connection: attached.connection,
        pty: attached.pty,
        terminal,
        events: attached.events,
    })
}

/// Drain PTY output into the view, coalescing bursts into one repaint.
///
/// Runs on GPUI's foreground executor. `tokio::sync::mpsc` is executor
/// agnostic, so awaiting it here needs no tokio reactor.
pub async fn pump(
    view: WeakEntity<TerminalView>,
    mut events: mpsc::Receiver<PtyEvent>,
    cx: &mut AsyncApp,
) {
    loop {
        let Some(first) = events.recv().await else {
            break;
        };

        let (batch, closed) = take_batch(first, &mut events);

        let updated = view.update(cx, |view, cx| {
            for event in batch {
                view.ingest(event);
            }
            cx.notify();
        });
        if updated.is_err() || closed {
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
pub fn start_pump(
    view: &Entity<TerminalView>,
    events: mpsc::Receiver<PtyEvent>,
    cx: &mut Context<TerminalView>,
) {
    let weak = view.downgrade();
    cx.spawn(async move |_, cx| {
        pump(weak, events, cx).await;
    })
    .detach();
}

/// Measure the round trip by timing a trivial exec, for the status bar and for
/// the predictive-echo threshold in M4.
pub async fn round_trip(connection: Arc<Connection>) -> Option<Duration> {
    let start = std::time::Instant::now();
    let out = connection.exec("true").await.ok()?;
    out.succeeded().then(|| start.elapsed())
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
