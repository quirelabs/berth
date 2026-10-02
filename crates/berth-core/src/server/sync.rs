//! When a worker sends raw output and when it sends screens (D49, D50).
//!
//! Raw output while the app keeps up: every byte, so both ends agree and
//! nothing on screen waits for the server. When more than [`BEHIND`] bytes of
//! output are unconfirmed, the app is behind: raw output stops, and a screen
//! goes instead, at most [`SCREENS_PER_SECOND`] a second and only once the app
//! has confirmed the one before, so screens never pile up either. When the
//! flood eases, one last screen brings the history the app missed, and raw
//! output resumes from it.
//!
//! While the app's window is hidden nothing is sent; shown again, it gets one
//! screen with its history. A program leaving the alternate screen gets one
//! too, so the screen underneath is right after vim even for an app that
//! attached while vim was up.
//!
//! Pure: what was sent and confirmed goes in, what to do comes out, so every
//! rule here is tested without a pty or a socket.

use std::time::{Duration, Instant};

/// Unconfirmed output past which the app is behind (D50).
pub const BEHIND: u64 = 256 * 1024;

/// The most screens sent a second while an app is behind (D50). Local only:
/// step 3 makes this and [`BEHIND`] follow the link's bandwidth and round
/// trip.
pub const SCREENS_PER_SECOND: u32 = 30;

const SCREEN_EVERY: Duration = Duration::from_millis(1000 / SCREENS_PER_SECOND as u64);

/// What to do with a chunk of output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Send,
    Drop,
}

/// A screen to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Just the screen: the app is still behind.
    Plain,
    /// The screen and every history line above it: the app's scrollback is
    /// rebuilt from them.
    WithHistory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Raw,
    Screens,
}

#[derive(Debug)]
pub struct Flow {
    mode: Mode,
    visible: bool,
    /// The output number the app has: the last raw chunk or screen sent.
    sent: u64,
    /// How far the app has confirmed.
    confirmed: u64,
    /// The last screen's number and when it went.
    screen_seq: u64,
    screen_at: Option<Instant>,
    /// Output dropped since the last screen, in bytes.
    dropped: u64,
    /// History lines went unsent: the next screen that can carry history
    /// must.
    gap: bool,
    /// A program left the alternate screen.
    alt_left: bool,
}

impl Default for Flow {
    fn default() -> Self {
        Self::new()
    }
}

impl Flow {
    pub fn new() -> Self {
        Self {
            mode: Mode::Raw,
            visible: true,
            sent: 0,
            confirmed: 0,
            screen_seq: 0,
            screen_at: None,
            dropped: 0,
            gap: false,
            alt_left: false,
        }
    }

    /// A chunk of output ending at `seq`, `len` bytes long.
    pub fn output(&mut self, seq: u64, len: u64) -> Output {
        if seq <= self.sent {
            // Already on a screen the app has.
            return Output::Drop;
        }
        if !self.visible || self.mode == Mode::Screens {
            self.drop_chunk(len);
            return Output::Drop;
        }
        if self.sent.saturating_sub(self.confirmed) > BEHIND {
            self.mode = Mode::Screens;
            self.drop_chunk(len);
            return Output::Drop;
        }
        self.sent = seq;
        Output::Send
    }

    fn drop_chunk(&mut self, len: u64) {
        self.dropped += len;
        self.gap = true;
    }

    pub fn confirmed(&mut self, seq: u64) {
        self.confirmed = self.confirmed.max(seq);
    }

    /// The app's window was hidden or shown. Shown again, it needs a screen
    /// with its history now.
    pub fn visible(&mut self, visible: bool) -> Option<Screen> {
        let was = self.visible;
        self.visible = visible;
        (visible && !was).then_some(Screen::WithHistory)
    }

    /// An app attached. It gets a screen with its history now, and raw
    /// output after it.
    pub fn attached(&mut self) -> Screen {
        self.mode = Mode::Raw;
        self.visible = true;
        Screen::WithHistory
    }

    /// A program left the alternate screen.
    pub fn left_alternate_screen(&mut self) {
        self.alt_left = true;
    }

    /// Called often (every few milliseconds): a screen to send now, if any.
    pub fn tick(&mut self, now: Instant) -> Option<Screen> {
        if !self.visible {
            return None;
        }
        let due = self
            .screen_at
            .is_none_or(|at| now.duration_since(at) >= SCREEN_EVERY);
        match self.mode {
            Mode::Raw => (self.alt_left && due).then_some(Screen::WithHistory),
            Mode::Screens => {
                // One screen in flight at a time, and no faster than the cap.
                if !due || self.confirmed < self.screen_seq {
                    return None;
                }
                if self.dropped >= BEHIND {
                    Some(Screen::Plain)
                } else {
                    // The flood has eased: one screen with the history it
                    // skipped, and raw output from there.
                    Some(Screen::WithHistory)
                }
            }
        }
    }

    /// A screen numbered `seq` went to the app.
    pub fn screen_sent(&mut self, seq: u64, kind: Screen, now: Instant) {
        self.sent = self.sent.max(seq);
        self.screen_seq = seq;
        self.screen_at = Some(now);
        self.dropped = 0;
        if kind == Screen::WithHistory {
            self.gap = false;
            self.alt_left = false;
            self.mode = Mode::Raw;
            // The app is taken to have what it was just sent; confirmations
            // from before it say nothing about the gap it closed.
            self.confirmed = self.confirmed.max(seq);
        }
    }

    /// Whether [`Flow::tick`] has anything to decide: an idle session never
    /// wakes the worker for it.
    pub fn wants_tick(&self) -> bool {
        self.visible && (self.mode == Mode::Screens || self.alt_left)
    }

    pub fn is_behind(&self) -> bool {
        self.mode == Mode::Screens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KB: u64 = 1024;

    /// Feed `n` chunks of `size` after `from`, confirming none.
    fn flood(flow: &mut Flow, from: u64, n: u64, size: u64) -> (u64, Vec<Output>) {
        let mut seq = from;
        let mut out = Vec::new();
        for _ in 0..n {
            seq += size;
            out.push(flow.output(seq, size));
        }
        (seq, out)
    }

    #[test]
    fn output_flows_raw_while_the_app_keeps_up() {
        let mut flow = Flow::new();
        let mut seq = 0;
        for _ in 0..1000 {
            seq += 8 * KB;
            assert_eq!(flow.output(seq, 8 * KB), Output::Send);
            flow.confirmed(seq);
        }
        assert!(!flow.is_behind());
    }

    #[test]
    fn past_the_threshold_raw_output_stops() {
        let mut flow = Flow::new();
        let (_, out) = flood(&mut flow, 0, 40, 8 * KB);
        let sent = out.iter().filter(|o| **o == Output::Send).count() as u64;
        assert!(
            sent * 8 * KB > BEHIND && sent * 8 * KB <= BEHIND + 8 * KB,
            "{sent}"
        );
        assert_eq!(out.last(), Some(&Output::Drop));
        assert!(flow.is_behind());
    }

    #[test]
    fn behind_screens_wait_for_the_last_to_be_confirmed_and_the_cap() {
        let start = Instant::now();
        let mut flow = Flow::new();
        let (seq, _) = flood(&mut flow, 0, 200, 8 * KB);
        assert_eq!(flow.tick(start), Some(Screen::Plain));
        flow.screen_sent(seq, Screen::Plain, start);

        // More floods in. The app has not confirmed that screen: nothing.
        let (seq, _) = flood(&mut flow, seq, 100, 8 * KB);
        assert_eq!(flow.tick(start + SCREEN_EVERY), None);
        // Confirmed, but too soon after the last: nothing.
        flow.confirmed(seq - 100 * 8 * KB);
        assert_eq!(flow.tick(start + Duration::from_millis(5)), None);
        // Confirmed, and due: the next screen.
        assert_eq!(flow.tick(start + SCREEN_EVERY), Some(Screen::Plain));
    }

    #[test]
    fn when_the_flood_eases_one_screen_brings_the_history_and_raw_resumes() {
        let start = Instant::now();
        let mut flow = Flow::new();
        let (seq, _) = flood(&mut flow, 0, 200, 8 * KB);
        flow.screen_sent(seq, Screen::Plain, start);
        flow.confirmed(seq);
        // A trickle, under the threshold, then a tick.
        let (seq, _) = flood(&mut flow, seq, 2, KB);
        let at = start + SCREEN_EVERY;
        assert_eq!(flow.tick(at), Some(Screen::WithHistory));
        flow.screen_sent(seq, Screen::WithHistory, at);
        assert!(!flow.is_behind());
        assert_eq!(flow.output(seq + 10, 10), Output::Send);
    }

    #[test]
    fn output_a_screen_already_covers_is_not_sent() {
        let mut flow = Flow::new();
        flow.screen_sent(1000, Screen::WithHistory, Instant::now());
        assert_eq!(flow.output(900, 100), Output::Drop);
        assert_eq!(flow.output(1000, 100), Output::Drop);
        assert_eq!(flow.output(1100, 100), Output::Send);
    }

    #[test]
    fn hidden_sends_nothing_and_shown_again_gets_a_screen() {
        let start = Instant::now();
        let mut flow = Flow::new();
        assert_eq!(flow.visible(false), None);
        assert_eq!(flow.output(100, 100), Output::Drop);
        assert_eq!(flow.tick(start), None, "nothing at all while hidden");
        assert_eq!(flow.visible(true), Some(Screen::WithHistory));
        assert_eq!(flow.visible(true), None, "shown twice is one screen");
    }

    #[test]
    fn leaving_the_alternate_screen_sends_one_screen_with_history() {
        let start = Instant::now();
        let mut flow = Flow::new();
        assert_eq!(flow.tick(start), None);
        flow.left_alternate_screen();
        assert_eq!(flow.tick(start), Some(Screen::WithHistory));
        flow.screen_sent(0, Screen::WithHistory, start);
        assert_eq!(flow.tick(start + SCREEN_EVERY), None);
    }

    #[test]
    fn attaching_resets_to_raw_with_a_full_screen() {
        let mut flow = Flow::new();
        flood(&mut flow, 0, 200, 8 * KB);
        assert!(flow.is_behind());
        assert_eq!(flow.attached(), Screen::WithHistory);
        flow.screen_sent(2_000_000, Screen::WithHistory, Instant::now());
        assert!(!flow.is_behind());
        assert_eq!(flow.output(2_000_100, 100), Output::Send);
    }
}
