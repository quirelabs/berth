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

/// Connect, attach to a tmux session, and prepare a terminal for it.
pub async fn connect(
    host: Host,
    slug: String,
    cols: u16,
    rows: u16,
) -> berth_core::Result<Connected> {
    let connection = Connection::connect(&host, TrustPolicy::KnownHostsOnly).await?;
    let session = Session::new(slug, cols, rows);
    let (pty, events) = connection.open_session(&session).await?;
    let terminal: SharedTerminal = Arc::new(Mutex::new(Terminal::new(
        TermSize::new(cols, rows),
        CellSize::new(8.0, 16.0),
    )));
    Ok(Connected {
        connection: Arc::new(connection),
        pty,
        terminal,
        events,
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

        let mut batch = vec![first];
        let mut closed = matches!(batch[0], PtyEvent::Closed);

        // Take whatever is *already* queued, but never wait for more.
        //
        // Waiting out a fixed window here would add its full length to every
        // keystroke echo, which is a lone small event with nothing to batch it
        // with. Draining only what is pending keeps bursts batched, because a
        // firehose refills the queue faster than this loop empties it, while a
        // single echo goes through immediately. `cx.notify()` already collapses
        // repaints to one per frame, so painting more often than the coalescing
        // window costs no extra frames.
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
}
