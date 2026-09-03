//! Driving the reconnect state machine against a real connection.
//!
//! The machine in `berth-core` is pure: events and a clock in, actions out. This
//! is the half that does I/O, kept separate so the machine stays testable and
//! this file stays small enough to read.
//!
//! The one rule that shapes the UI: a connection that is Degraded or
//! Reconnecting keeps its grid on screen. Berth already holds the last rendered
//! screen and it is correct until the remote says otherwise, so there is never a
//! reason to blank the terminal while waiting. tmux's repaint on re-attach
//! overwrites it in place.

use std::sync::Arc;
use std::time::{Duration, Instant};

use berth_core::connection::state::{Action, ConnectionState, Event, Machine};
use berth_core::transport::Connection;

/// A monotonic millisecond clock for the machine, which takes time as a number
/// so it can be tested without one.
#[derive(Debug, Clone)]
pub struct Clock {
    origin: Instant,
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }

    pub fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    /// How long to wait before the next `Tick` can change anything.
    ///
    /// `None` means the machine is resting and only an external event will move
    /// it, so the driver should wait on the event channel alone rather than
    /// spinning on a timer.
    pub fn until(&self, deadline_ms: Option<u64>) -> Option<Duration> {
        deadline_ms.map(|d| Duration::from_millis(d.saturating_sub(self.now_ms())))
    }
}

/// What the driver decided to do, so a caller can act without knowing the
/// machine's internals. Kept separate from `Action` because the UI cares about
/// the state transition and not about the work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub state: ConnectionState,
    pub actions: Vec<Action>,
    /// True when this step changed the state, so the UI repaints only then.
    pub changed: bool,
}

/// The machine plus a clock, with the I/O left to the caller.
pub struct Driver {
    machine: Machine,
    clock: Clock,
}

impl Default for Driver {
    fn default() -> Self {
        Self::new()
    }
}

impl Driver {
    pub fn new() -> Self {
        Self {
            machine: Machine::new(),
            clock: Clock::new(),
        }
    }

    pub fn state(&self) -> ConnectionState {
        self.machine.state()
    }

    /// True while the terminal grid stays on screen, whatever the transport is
    /// doing. This is instant reattach: the grid is never taken away.
    pub fn holds_grid(&self) -> bool {
        self.machine.state().holds_ui()
    }

    pub fn handle(&mut self, event: Event) -> Step {
        self.handle_at(event, self.clock.now_ms())
    }

    /// The machine takes time as a number so it is deterministic under test,
    /// and wrapping it in a real clock would throw that away. This is the seam
    /// that keeps it.
    pub fn handle_at(&mut self, event: Event, now_ms: u64) -> Step {
        let before = self.machine.state();
        let actions = self.machine.handle(event, now_ms);
        let state = self.machine.state();
        Step {
            state,
            actions,
            changed: state != before,
        }
    }

    /// How long until a `Tick` is worth sending.
    pub fn next_wait(&self) -> Option<Duration> {
        self.clock.until(self.machine.next_deadline_ms())
    }
}

/// One keepalive: a trivial exec, which proves the server is still answering
/// rather than merely that the socket is open.
///
/// A TCP-level probe would survive a wedged sshd, and a session that looks alive
/// but answers nothing is precisely the case Degraded exists for.
pub async fn keepalive(connection: Arc<Connection>) -> bool {
    matches!(connection.exec("true").await, Ok(out) if out.succeeded())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_driver_is_disconnected_and_holds_nothing() {
        let driver = Driver::new();
        assert_eq!(driver.state(), ConnectionState::Disconnected);
        assert!(!driver.holds_grid());
    }

    #[test]
    fn opening_asks_for_a_connection_and_reports_the_change() {
        let mut driver = Driver::new();
        let step = driver.handle(Event::Open);
        assert!(step.changed);
        assert_eq!(step.state, ConnectionState::Connecting);
        assert!(step.actions.contains(&Action::Connect));
    }

    /// The property the UI depends on: once connected, the grid stays up
    /// through every state the transport can reach short of the user closing
    /// the tab.
    #[test]
    fn the_grid_survives_degrading_and_reconnecting() {
        let mut driver = Driver::new();
        driver.handle_at(Event::Open, 0);
        driver.handle_at(Event::ConnectSucceeded, 0);
        assert!(driver.holds_grid());

        // Walk a minute of silence past the keepalive interval and the
        // degraded grace period, answering nothing.
        let mut seen = Vec::new();
        for second in 1..=60u64 {
            driver.handle_at(Event::Tick, second * 1_000);
            assert!(
                driver.holds_grid(),
                "the grid was dropped at {second}s in {:?}",
                driver.state()
            );
            if !seen.contains(&driver.state()) {
                seen.push(driver.state());
            }
        }
        assert!(
            seen.contains(&ConnectionState::Degraded),
            "nothing degraded, so the test proved nothing: {seen:?}"
        );
        assert!(
            seen.contains(&ConnectionState::Reconnecting),
            "nothing reconnected, so the test proved nothing: {seen:?}"
        );
    }

    #[test]
    fn closing_tears_down_and_releases_the_grid() {
        let mut driver = Driver::new();
        driver.handle_at(Event::Open, 0);
        driver.handle_at(Event::ConnectSucceeded, 0);
        let step = driver.handle(Event::Close);
        assert!(step.actions.contains(&Action::Teardown));
        assert!(!driver.holds_grid());
    }

    #[test]
    fn a_resting_machine_asks_for_no_timer() {
        let driver = Driver::new();
        assert!(
            driver.next_wait().is_none(),
            "Disconnected has no deadline, so the driver must not spin"
        );
    }

    #[test]
    fn a_connected_machine_asks_for_a_keepalive_timer() {
        let mut driver = Driver::new();
        driver.handle_at(Event::Open, 0);
        driver.handle_at(Event::ConnectSucceeded, 0);
        assert!(driver.next_wait().is_some());
    }

    /// The reconnect never gives up and never spins: the backoff grows and the
    /// last step repeats, so a laptop shut overnight retries every 30 s rather
    /// than every millisecond.
    #[test]
    fn reconnecting_keeps_asking_for_a_later_retry() {
        let mut driver = Driver::new();
        driver.handle_at(Event::Open, 0);
        driver.handle_at(Event::ConnectSucceeded, 0);
        let mut connects = 0;
        // Fail only an attempt that was actually made. Answering a failure to a
        // connect that never happened just pushes the backoff out and proves
        // nothing.
        for second in 1..=600u64 {
            let now = second * 1_000;
            let step = driver.handle_at(Event::Tick, now);
            if step.actions.contains(&Action::Connect) {
                connects += 1;
                driver.handle_at(Event::ConnectFailed, now);
            }
        }
        assert!(connects > 1, "it stopped retrying after {connects}");
        assert!(
            connects < 600,
            "it retried {connects} times in ten minutes, which is a spin"
        );
    }

    #[test]
    fn a_deadline_in_the_past_waits_for_no_time_rather_than_underflowing() {
        let clock = Clock::new();
        let wait = clock.until(Some(0)).expect("a deadline was given");
        assert_eq!(wait, Duration::ZERO);
    }
}
