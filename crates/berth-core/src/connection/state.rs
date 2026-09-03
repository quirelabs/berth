//! The reconnect state machine, as a pure function of events and a clock.
//!
//! ```text
//! Disconnected -> Connecting -> Connected -> Degraded -> Reconnecting -> Connected
//!                                        \-> Disconnected (user closed)
//! ```
//!
//! Time is passed in as milliseconds so the machine is deterministic under
//! test. Nothing here performs I/O; the driver turns [`Action`]s into work.

/// Keepalives go out on this interval while a connection is live.
pub const KEEPALIVE_INTERVAL_MS: u64 = 15_000;
/// Two unacknowledged keepalives move a connection to `Degraded`.
pub const MISSED_KEEPALIVES_TO_DEGRADE: u32 = 2;
/// How long `Degraded` is tolerated before a full reconnect.
pub const DEGRADED_GRACE_MS: u64 = 10_000;
/// Reconnect backoff; the last entry repeats forever.
pub const BACKOFF_MS: [u64; 6] = [1_000, 2_000, 4_000, 8_000, 15_000, 30_000];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Degraded,
    Reconnecting,
}

impl ConnectionState {
    /// True while the UI should keep the terminal grid on screen.
    pub fn holds_ui(self) -> bool {
        matches!(self, Self::Connected | Self::Degraded | Self::Reconnecting)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The user opened the host, or asked for a retry now.
    Open,
    /// The user closed the tab. Terminal for the machine.
    Close,
    ConnectSucceeded,
    ConnectFailed,
    KeepaliveAck,
    /// Transport-level failure that cannot be recovered by waiting.
    HardError,
    /// Advance the clock.
    Tick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Begin a connection attempt.
    Connect,
    SendKeepalive,
    /// Re-attach tmux sessions and re-establish port forwards.
    Reattach,
    /// Drop the transport and release resources.
    Teardown,
}

#[derive(Debug, Clone)]
pub struct Machine {
    state: ConnectionState,
    missed_keepalives: u32,
    last_keepalive_ms: u64,
    degraded_since_ms: Option<u64>,
    retry_at_ms: Option<u64>,
    backoff_step: usize,
    /// Set once a connection has been established, so the machine knows a
    /// later success is a re-attach rather than a first attach.
    was_connected: bool,
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

impl Machine {
    pub fn new() -> Self {
        Self {
            state: ConnectionState::Disconnected,
            missed_keepalives: 0,
            last_keepalive_ms: 0,
            degraded_since_ms: None,
            retry_at_ms: None,
            backoff_step: 0,
            was_connected: false,
        }
    }

    pub fn state(&self) -> ConnectionState {
        self.state
    }

    pub fn missed_keepalives(&self) -> u32 {
        self.missed_keepalives
    }

    /// The next instant at which [`Event::Tick`] can change anything, so the
    /// driver can sleep instead of polling.
    pub fn next_deadline_ms(&self) -> Option<u64> {
        match self.state {
            ConnectionState::Connected => Some(self.last_keepalive_ms + KEEPALIVE_INTERVAL_MS),
            ConnectionState::Degraded => {
                let keepalive = self.last_keepalive_ms + KEEPALIVE_INTERVAL_MS;
                let grace = self.degraded_since_ms.map(|t| t + DEGRADED_GRACE_MS);
                Some(grace.map_or(keepalive, |g| g.min(keepalive)))
            }
            ConnectionState::Reconnecting => self.retry_at_ms,
            ConnectionState::Disconnected | ConnectionState::Connecting => None,
        }
    }

    pub fn handle(&mut self, event: Event, now_ms: u64) -> Vec<Action> {
        use ConnectionState::*;

        match (self.state, event) {
            (_, Event::Close) => {
                self.state = Disconnected;
                self.reset_timers();
                vec![Action::Teardown]
            }

            (Disconnected, Event::Open) => {
                self.state = Connecting;
                vec![Action::Connect]
            }
            // An explicit Open while reconnecting means "retry now".
            (Reconnecting, Event::Open) => {
                self.retry_at_ms = None;
                vec![Action::Connect]
            }

            (Connecting | Reconnecting, Event::ConnectSucceeded) => {
                let reattaching = self.was_connected;
                self.state = Connected;
                self.was_connected = true;
                self.missed_keepalives = 0;
                self.last_keepalive_ms = now_ms;
                self.degraded_since_ms = None;
                self.retry_at_ms = None;
                self.backoff_step = 0;
                if reattaching {
                    vec![Action::Reattach]
                } else {
                    Vec::new()
                }
            }

            (Connecting | Reconnecting, Event::ConnectFailed) => {
                self.state = Reconnecting;
                self.schedule_retry(now_ms);
                Vec::new()
            }

            (Connected | Degraded, Event::KeepaliveAck) => {
                self.state = Connected;
                self.missed_keepalives = 0;
                self.degraded_since_ms = None;
                Vec::new()
            }

            (Connecting | Connected | Degraded, Event::HardError) => {
                self.state = Reconnecting;
                self.missed_keepalives = 0;
                self.degraded_since_ms = None;
                self.schedule_retry(now_ms);
                Vec::new()
            }

            (Connected, Event::Tick) => {
                let mut actions = Vec::new();
                if now_ms.saturating_sub(self.last_keepalive_ms) >= KEEPALIVE_INTERVAL_MS {
                    self.last_keepalive_ms = now_ms;
                    self.missed_keepalives += 1;
                    actions.push(Action::SendKeepalive);
                }
                if self.missed_keepalives >= MISSED_KEEPALIVES_TO_DEGRADE {
                    self.state = Degraded;
                    self.degraded_since_ms = Some(now_ms);
                }
                actions
            }

            (Degraded, Event::Tick) => {
                let mut actions = Vec::new();
                if now_ms.saturating_sub(self.last_keepalive_ms) >= KEEPALIVE_INTERVAL_MS {
                    self.last_keepalive_ms = now_ms;
                    self.missed_keepalives += 1;
                    actions.push(Action::SendKeepalive);
                }
                let expired = self
                    .degraded_since_ms
                    .is_some_and(|t| now_ms.saturating_sub(t) >= DEGRADED_GRACE_MS);
                if expired {
                    self.state = Reconnecting;
                    self.degraded_since_ms = None;
                    self.missed_keepalives = 0;
                    self.schedule_retry(now_ms);
                    actions.clear();
                }
                actions
            }

            (Reconnecting, Event::Tick) => {
                if self.retry_at_ms.is_some_and(|t| now_ms >= t) {
                    self.retry_at_ms = None;
                    vec![Action::Connect]
                } else {
                    Vec::new()
                }
            }

            // Everything else is a no-op: late acks, duplicate opens, ticks
            // while disconnected.
            _ => Vec::new(),
        }
    }

    /// Backoff advances 1, 2, 4, 8, 15, 30 s and then repeats 30 s forever.
    fn schedule_retry(&mut self, now_ms: u64) {
        let delay = BACKOFF_MS[self.backoff_step.min(BACKOFF_MS.len() - 1)];
        self.retry_at_ms = Some(now_ms + delay);
        self.backoff_step = (self.backoff_step + 1).min(BACKOFF_MS.len() - 1);
    }

    fn reset_timers(&mut self) {
        self.missed_keepalives = 0;
        self.degraded_since_ms = None;
        self.retry_at_ms = None;
        self.backoff_step = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive to `Connected` and return the machine plus the current clock.
    fn connected() -> (Machine, u64) {
        let mut m = Machine::new();
        assert_eq!(m.handle(Event::Open, 0), vec![Action::Connect]);
        assert_eq!(m.state(), ConnectionState::Connecting);
        assert!(m.handle(Event::ConnectSucceeded, 0).is_empty());
        assert_eq!(m.state(), ConnectionState::Connected);
        (m, 0)
    }

    #[test]
    fn first_connect_does_not_reattach() {
        let (m, _) = connected();
        assert_eq!(m.state(), ConnectionState::Connected);
    }

    #[test]
    fn two_missed_keepalives_degrade() {
        let (mut m, _) = connected();
        assert_eq!(m.handle(Event::Tick, 15_000), vec![Action::SendKeepalive]);
        assert_eq!(m.state(), ConnectionState::Connected);
        assert_eq!(m.handle(Event::Tick, 30_000), vec![Action::SendKeepalive]);
        assert_eq!(m.state(), ConnectionState::Degraded);
    }

    #[test]
    fn an_ack_recovers_from_degraded() {
        let (mut m, _) = connected();
        m.handle(Event::Tick, 15_000);
        m.handle(Event::Tick, 30_000);
        assert_eq!(m.state(), ConnectionState::Degraded);
        m.handle(Event::KeepaliveAck, 31_000);
        assert_eq!(m.state(), ConnectionState::Connected);
        assert_eq!(m.missed_keepalives(), 0);
    }

    #[test]
    fn degraded_expires_into_reconnecting() {
        let (mut m, _) = connected();
        m.handle(Event::Tick, 15_000);
        m.handle(Event::Tick, 30_000);
        assert_eq!(m.state(), ConnectionState::Degraded);
        m.handle(Event::Tick, 39_999);
        assert_eq!(m.state(), ConnectionState::Degraded);
        m.handle(Event::Tick, 40_000);
        assert_eq!(m.state(), ConnectionState::Reconnecting);
    }

    #[test]
    fn hard_error_skips_degraded() {
        let (mut m, _) = connected();
        m.handle(Event::HardError, 1_000);
        assert_eq!(m.state(), ConnectionState::Reconnecting);
    }

    #[test]
    fn reconnect_success_reattaches() {
        let (mut m, _) = connected();
        m.handle(Event::HardError, 1_000);
        assert_eq!(m.handle(Event::Tick, 2_000), vec![Action::Connect]);
        assert_eq!(
            m.handle(Event::ConnectSucceeded, 2_100),
            vec![Action::Reattach]
        );
        assert_eq!(m.state(), ConnectionState::Connected);
    }

    #[test]
    fn backoff_follows_the_documented_schedule() {
        let mut m = Machine::new();
        m.handle(Event::Open, 0);
        let mut now = 0;
        let mut delays = Vec::new();
        for _ in 0..8 {
            m.handle(Event::ConnectFailed, now);
            let at = m.next_deadline_ms().unwrap();
            delays.push(at - now);
            now = at;
            assert_eq!(m.handle(Event::Tick, now), vec![Action::Connect]);
        }
        assert_eq!(
            delays,
            vec![1_000, 2_000, 4_000, 8_000, 15_000, 30_000, 30_000, 30_000]
        );
    }

    #[test]
    fn backoff_resets_after_a_success() {
        let mut m = Machine::new();
        m.handle(Event::Open, 0);
        m.handle(Event::ConnectFailed, 0);
        m.handle(Event::ConnectFailed, 1_000);
        m.handle(Event::Tick, 10_000);
        m.handle(Event::ConnectSucceeded, 10_000);
        m.handle(Event::HardError, 11_000);
        assert_eq!(
            m.next_deadline_ms(),
            Some(12_000),
            "backoff should restart at 1 s"
        );
    }

    #[test]
    fn close_is_terminal_from_every_state() {
        for setup in [
            Event::Open,
            Event::ConnectSucceeded,
            Event::HardError,
            Event::Tick,
        ] {
            let mut m = Machine::new();
            m.handle(Event::Open, 0);
            m.handle(setup, 0);
            assert_eq!(m.handle(Event::Close, 0), vec![Action::Teardown]);
            assert_eq!(m.state(), ConnectionState::Disconnected);
        }
    }

    #[test]
    fn ui_is_held_through_degraded_and_reconnecting() {
        assert!(!ConnectionState::Disconnected.holds_ui());
        assert!(!ConnectionState::Connecting.holds_ui());
        assert!(ConnectionState::Connected.holds_ui());
        assert!(ConnectionState::Degraded.holds_ui());
        assert!(ConnectionState::Reconnecting.holds_ui());
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        fn any_event() -> impl Strategy<Value = Event> {
            prop_oneof![
                Just(Event::Open),
                Just(Event::ConnectSucceeded),
                Just(Event::ConnectFailed),
                Just(Event::KeepaliveAck),
                Just(Event::HardError),
                Just(Event::Tick),
            ]
        }

        proptest! {
            /// Without a Close, the machine never parks in Disconnected: some
            /// path back to a connection always remains.
            #[test]
            fn never_gives_up_without_close(
                events in prop::collection::vec((any_event(), 0u64..5_000), 1..200)
            ) {
                let mut m = Machine::new();
                m.handle(Event::Open, 0);
                let mut now = 0u64;
                for (event, step) in events {
                    now += step;
                    m.handle(event, now);
                    prop_assert_ne!(m.state(), ConnectionState::Disconnected);
                }
            }

            /// Close always wins, whatever came before it.
            #[test]
            fn close_always_disconnects(
                events in prop::collection::vec((any_event(), 0u64..5_000), 0..100)
            ) {
                let mut m = Machine::new();
                let mut now = 0u64;
                for (event, step) in events {
                    now += step;
                    m.handle(event, now);
                }
                m.handle(Event::Close, now);
                prop_assert_eq!(m.state(), ConnectionState::Disconnected);
                prop_assert_eq!(m.next_deadline_ms(), None);
            }

            /// A scheduled retry is never further out than the 30 s ceiling.
            #[test]
            fn retry_delay_is_bounded(
                events in prop::collection::vec((any_event(), 0u64..1_000), 1..200)
            ) {
                let mut m = Machine::new();
                m.handle(Event::Open, 0);
                let mut now = 0u64;
                for (event, step) in events {
                    now += step;
                    m.handle(event, now);
                    if m.state() == ConnectionState::Reconnecting
                        && let Some(at) = m.next_deadline_ms() {
                        prop_assert!(at.saturating_sub(now) <= 30_000);
                    }
                }
            }
        }
    }
}
