//! When a worker sends raw output and when it sends screens (D49, D50).
//!
//! Raw output while the app keeps up: every byte, so both ends agree and
//! nothing on screen waits for the server. The app is behind when more output
//! is unconfirmed than the window, which is sized from what is measured:
//! twice the recent output rate times the recent round trip of a
//! confirmation, never under [`MIN_WINDOW`] nor over [`MAX_WINDOW`]. A client
//! that keeps up, on a fast link or a slow one, so stays on the raw stream.
//!
//! Behind, raw output stops and a screen goes instead, at most
//! [`SCREENS_PER_SECOND`] a second and only once the app has confirmed the
//! one before, so screens never pile up either. When less than the window was
//! dropped in [`CALM_INTERVALS`] screen intervals in a row, a last screen
//! resumes the raw stream. The history the app missed is not sent then: it
//! goes once output has been calm for [`CALM_FOR`], or at once when the app
//! asks for it (its user scrolled up into lines it does not have), so a flood
//! never sets off one history rebuild after another.
//!
//! While the app's window is hidden nothing is sent; shown again, it gets one
//! screen with its history. A program leaving the alternate screen gets one
//! too, so the screen underneath is right after vim even for an app that
//! attached while vim was up.
//!
//! Pure: what was sent and confirmed, and when, goes in, what to do comes
//! out, so every rule here is tested without a pty or a socket.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The smallest window of unconfirmed output (D50): room for the app's
/// confirmation spacing and a little more, whatever was measured.
pub const MIN_WINDOW: u64 = 256 * 1024;

/// The largest window: past this an app is behind however fast the link.
pub const MAX_WINDOW: u64 = 8 * 1024 * 1024;

/// The most messages unconfirmed before the app is behind, whatever their
/// bytes. Half the smallest queue on the way (the app's, 256 messages per
/// session): the window must never fill a queue, or that queue's own
/// recovery, a full re-attach with all the history, fires instead of this
/// flow.
pub const MAX_IN_FLIGHT: usize = 128;

/// The most screens sent a second while an app is behind (D50).
pub const SCREENS_PER_SECOND: u32 = 30;

const SCREEN_EVERY: Duration = Duration::from_millis(1000 / SCREENS_PER_SECOND as u64);

/// Screen intervals in a row with less than the window dropped before raw
/// output resumes.
pub const CALM_INTERVALS: u32 = 3;

/// How long output must stay calm before the history an app missed is sent.
pub const CALM_FOR: Duration = Duration::from_secs(1);

/// Output faster than this, in bytes a second, is not calm.
const CALM_RATE: f64 = 64.0 * 1024.0;

/// How long a screen waits for its confirmation before the next goes anyway:
/// a confirmation lost on the way must not silence a session.
const CONFIRM_WITHIN: Duration = Duration::from_secs(1);

/// The span over which the output rate is measured.
const RATE_SPAN: Duration = Duration::from_millis(50);

/// What has been sent and not yet confirmed, for timing round trips and
/// counting messages in flight. Bounded: past this the oldest are forgotten.
const TIMED_AT_MOST: usize = 4096;

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
    /// Just the screen, and raw output from there: the app keeps up again.
    /// The history it missed comes later.
    Resume,
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
    /// History lines went unsent: the app's scrollback has a gap.
    gap: bool,
    /// The app asked for the history it is missing.
    history_wanted: bool,
    /// A program left the alternate screen.
    alt_left: bool,
    /// Screen intervals in a row with less than the window dropped.
    calm_intervals: u32,
    /// The output rate, in bytes a second, smoothed over recent spans.
    rate: f64,
    /// The span being measured: when it began and the output in it.
    span_start: Option<Instant>,
    span_bytes: u64,
    /// The last time output ran faster than calm.
    busy_at: Option<Instant>,
    /// When the last chunk of output came.
    last_output: Option<Instant>,
    /// A confirmation's round trip, smoothed. `None` until one is measured.
    round_trip: Option<Duration>,
    /// What went out and when, oldest first, until confirmed.
    timed: VecDeque<(u64, Instant)>,
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
            history_wanted: false,
            alt_left: false,
            calm_intervals: 0,
            rate: 0.0,
            span_start: None,
            span_bytes: 0,
            busy_at: None,
            last_output: None,
            round_trip: None,
            timed: VecDeque::new(),
        }
    }

    /// A chunk of output ending at `seq`, `len` bytes long.
    pub fn output(&mut self, seq: u64, len: u64, now: Instant) -> Output {
        if seq <= self.sent {
            // Already on a screen the app has.
            return Output::Drop;
        }
        self.measure(now);
        self.span_bytes += len;
        self.last_output = Some(now);
        if !self.visible || self.mode == Mode::Screens {
            self.drop_chunk(len);
            return Output::Drop;
        }
        let unconfirmed = self.sent.saturating_sub(self.confirmed);
        if unconfirmed > self.window(now) || self.timed.len() >= MAX_IN_FLIGHT {
            self.mode = Mode::Screens;
            self.calm_intervals = 0;
            self.drop_chunk(len);
            return Output::Drop;
        }
        self.sent = seq;
        self.timed_out(seq, now);
        Output::Send
    }

    fn drop_chunk(&mut self, len: u64) {
        self.dropped += len;
        self.gap = true;
    }

    /// The app has everything up to `seq`.
    pub fn confirmed(&mut self, seq: u64, now: Instant) {
        self.confirmed = self.confirmed.max(seq);
        let mut newest = None;
        while let Some(&(sent, at)) = self.timed.front() {
            if sent > seq {
                break;
            }
            newest = Some(at);
            self.timed.pop_front();
        }
        if let Some(at) = newest {
            let sample = now.saturating_duration_since(at);
            // Up at once, down slowly: a window too small for the moment
            // costs a switch to screens, one too large only a little memory.
            self.round_trip = Some(match self.round_trip {
                Some(before) if sample < before => (before * 7 + sample) / 8,
                _ => sample,
            });
        }
    }

    /// How much output may be unconfirmed before the app is behind: twice
    /// what one round trip carries at the recent rate, within the bounds.
    pub fn window(&self, now: Instant) -> u64 {
        // The span under way counts as soon as it says anything, so a flood
        // that starts now is not judged by the quiet before it.
        let partial = self.span_start.map_or(0.0, |start| {
            let elapsed = now
                .saturating_duration_since(start)
                .max(Duration::from_millis(10));
            self.span_bytes as f64 / elapsed.as_secs_f64()
        });
        let rate = self.rate.max(partial);
        let round_trip = self.round_trip.unwrap_or_default().as_secs_f64();
        ((2.0 * rate * round_trip) as u64).clamp(MIN_WINDOW, MAX_WINDOW)
    }

    /// The smoothed round trip, if one has been measured.
    pub fn round_trip(&self) -> Option<Duration> {
        self.round_trip
    }

    /// Close the span being measured if it has run its length.
    fn measure(&mut self, now: Instant) {
        let Some(start) = self.span_start else {
            self.span_start = Some(now);
            return;
        };
        let elapsed = now.saturating_duration_since(start);
        if elapsed < RATE_SPAN {
            return;
        }
        let rate = self.span_bytes as f64 / elapsed.as_secs_f64();
        self.rate = if self.rate == 0.0 {
            rate
        } else {
            self.rate * 0.75 + rate * 0.25
        };
        if rate > CALM_RATE {
            // Busy until its last output, not until the span was closed: a
            // span closed late, after a pause, says nothing about the pause.
            self.busy_at = Some(self.last_output.unwrap_or(now));
        }
        self.span_start = Some(now);
        self.span_bytes = 0;
    }

    fn timed_out(&mut self, seq: u64, now: Instant) {
        self.timed.push_back((seq, now));
        if self.timed.len() > TIMED_AT_MOST {
            self.timed.pop_front();
        }
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
        self.calm_intervals = 0;
        Screen::WithHistory
    }

    /// A program left the alternate screen.
    pub fn left_alternate_screen(&mut self) {
        self.alt_left = true;
    }

    /// The app wants the history it is missing: its user scrolled up into
    /// it. Sent as soon as the app keeps up, without waiting for calm.
    pub fn history_wanted(&mut self) {
        if self.gap {
            self.history_wanted = true;
        }
    }

    /// Called often (every few milliseconds): a screen to send now, if any.
    pub fn tick(&mut self, now: Instant) -> Option<Screen> {
        if !self.visible {
            return None;
        }
        self.measure(now);
        let due = self
            .screen_at
            .is_none_or(|at| now.duration_since(at) >= SCREEN_EVERY);
        if !due {
            return None;
        }
        match self.mode {
            Mode::Raw => {
                let calm = self
                    .busy_at
                    .is_none_or(|at| now.duration_since(at) >= CALM_FOR);
                let history = self.gap && (self.history_wanted || calm);
                (self.alt_left || history).then_some(Screen::WithHistory)
            }
            Mode::Screens => {
                // One screen in flight at a time, and no faster than the cap,
                // unless its confirmation is long overdue.
                let overdue = self
                    .screen_at
                    .is_some_and(|at| now.duration_since(at) >= CONFIRM_WITHIN);
                if self.confirmed < self.screen_seq && !overdue {
                    return None;
                }
                if self.dropped < self.window(now) {
                    self.calm_intervals += 1;
                } else {
                    self.calm_intervals = 0;
                }
                if self.calm_intervals >= CALM_INTERVALS {
                    Some(Screen::Resume)
                } else {
                    Some(Screen::Plain)
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
        self.timed_out(seq, now);
        if kind != Screen::Plain {
            self.mode = Mode::Raw;
            self.calm_intervals = 0;
            // The app is taken to have what it was just sent; confirmations
            // from before it say nothing about the gap it closed.
            self.confirmed = self.confirmed.max(seq);
        }
        if kind == Screen::WithHistory {
            self.gap = false;
            self.history_wanted = false;
            self.alt_left = false;
        }
    }

    /// Whether [`Flow::tick`] has anything to decide: an idle session with
    /// nothing owed never wakes the worker for it.
    pub fn wants_tick(&self) -> bool {
        self.visible && (self.mode == Mode::Screens || self.alt_left || self.gap)
    }

    pub fn is_behind(&self) -> bool {
        self.mode == Mode::Screens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;

    /// Feed `n` chunks of `size` after `from`, one every `every`, confirming
    /// none. Returns the last number, the time after, and what was decided.
    fn flood(
        flow: &mut Flow,
        from: u64,
        n: u64,
        size: u64,
        at: Instant,
        every: Duration,
    ) -> (u64, Instant, Vec<Output>) {
        let mut seq = from;
        let mut now = at;
        let mut out = Vec::new();
        for _ in 0..n {
            seq += size;
            now += every;
            out.push(flow.output(seq, size, now));
        }
        (seq, now, out)
    }

    /// Fall behind: a fast flood with nothing confirmed.
    fn behind(flow: &mut Flow, at: Instant) -> (u64, Instant) {
        let (seq, now, _) = flood(flow, 0, 400, 8 * KB, at, Duration::from_micros(100));
        assert!(flow.is_behind());
        (seq, now)
    }

    #[test]
    fn output_flows_raw_while_the_app_keeps_up() {
        let start = Instant::now();
        let mut flow = Flow::new();
        let mut seq = 0;
        for i in 0..1000 {
            seq += 8 * KB;
            let now = start + Duration::from_millis(i);
            assert_eq!(flow.output(seq, 8 * KB, now), Output::Send);
            flow.confirmed(seq, now + Duration::from_millis(1));
        }
        assert!(!flow.is_behind());
    }

    #[test]
    fn with_nothing_measured_the_window_is_the_floor() {
        let start = Instant::now();
        let mut flow = Flow::new();
        let (_, _, out) = flood(&mut flow, 0, 40, 8 * KB, start, Duration::ZERO);
        let sent = out.iter().filter(|o| **o == Output::Send).count() as u64;
        assert!(
            sent * 8 * KB > MIN_WINDOW && sent * 8 * KB <= MIN_WINDOW + 8 * KB,
            "{sent}"
        );
        assert_eq!(out.last(), Some(&Output::Drop));
        assert!(flow.is_behind());
    }

    /// The case the fixed window failed (D50): 28 MB/s with a 10 ms round
    /// trip keeps 280 KB in flight, over 256 KB, while the app keeps up.
    #[test]
    fn a_client_that_keeps_up_on_a_slow_round_trip_stays_raw() {
        let start = Instant::now();
        let mut flow = Flow::new();
        let round_trip = Duration::from_millis(10);
        let every = Duration::from_micros(8 * 1000 / 28); // 8 KB at 28 MB/s
        // Attaching: a screen, confirmed one round trip later. The worker
        // knows the round trip from it before any output.
        flow.attached();
        flow.screen_sent(1, Screen::WithHistory, start);
        flow.confirmed(1, start + round_trip);
        let mut seq = 1;
        let mut now = start + round_trip;
        let mut sent = VecDeque::new();
        for _ in 0..20_000 {
            seq += 8 * KB;
            now += every;
            assert_eq!(flow.output(seq, 8 * KB, now), Output::Send, "at {seq}");
            sent.push_back((seq, now));
            // The app confirms each chunk one round trip after it went.
            while let Some(&(s, at)) = sent.front() {
                if now.duration_since(at) < round_trip {
                    break;
                }
                flow.confirmed(s, at + round_trip);
                sent.pop_front();
            }
        }
        assert!(!flow.is_behind());
        let window = flow.window(now);
        assert!(window > 400 * KB && window < 800 * KB, "{window}");
    }

    /// Many small chunks fill the queues on the way long before the bytes
    /// fill the window: the count of messages decides then.
    #[test]
    fn small_chunks_are_bounded_by_count() {
        let start = Instant::now();
        let mut flow = Flow::new();
        let (_, _, out) = flood(&mut flow, 0, 1000, 100, start, Duration::from_micros(10));
        let sent = out.iter().filter(|o| **o == Output::Send).count();
        assert_eq!(sent, MAX_IN_FLIGHT);
        assert!(flow.is_behind());
    }

    #[test]
    fn the_window_has_a_ceiling() {
        let start = Instant::now();
        let mut flow = Flow::new();
        flow.output(8 * KB, 8 * KB, start);
        flow.confirmed(8 * KB, start + Duration::from_secs(5));
        flood(
            &mut flow,
            8 * KB,
            2000,
            64 * KB,
            start,
            Duration::from_micros(10),
        );
        assert_eq!(flow.window(start + Duration::from_millis(20)), MAX_WINDOW);
    }

    #[test]
    fn behind_screens_wait_for_the_last_to_be_confirmed_and_the_cap() {
        let mut flow = Flow::new();
        let (seq, now) = behind(&mut flow, Instant::now());
        assert_eq!(flow.tick(now), Some(Screen::Plain));
        flow.screen_sent(seq, Screen::Plain, now);

        // More floods in. The app has not confirmed that screen: nothing.
        let (seq, _, _) = flood(&mut flow, seq, 100, 8 * KB, now, Duration::from_micros(100));
        assert_eq!(flow.tick(now + SCREEN_EVERY), None);
        // Confirmed, but too soon after the last: nothing.
        flow.confirmed(seq - 100 * 8 * KB, now + Duration::from_millis(5));
        assert_eq!(flow.tick(now + Duration::from_millis(5)), None);
        // Confirmed, and due: the next screen.
        assert_eq!(flow.tick(now + SCREEN_EVERY), Some(Screen::Plain));
    }

    /// A lost confirmation delays the next screen, it does not silence the
    /// session.
    #[test]
    fn an_unconfirmed_screen_is_followed_by_another_in_time() {
        let mut flow = Flow::new();
        let (seq, now) = behind(&mut flow, Instant::now());
        flow.screen_sent(seq, Screen::Plain, now);
        flood(&mut flow, seq, 100, 8 * KB, now, Duration::from_micros(100));
        assert_eq!(flow.tick(now + SCREEN_EVERY), None);
        assert_eq!(flow.tick(now + CONFIRM_WITHIN), Some(Screen::Plain));
    }

    /// One quiet interval is not the end of a flood: raw output resumes only
    /// after three in a row, and with a plain screen, not the history.
    #[test]
    fn raw_resumes_after_three_calm_intervals_without_the_history() {
        let mut flow = Flow::new();
        let (mut seq, mut now) = behind(&mut flow, Instant::now());
        let mut screens = Vec::new();
        for interval in 0..7 {
            now += SCREEN_EVERY;
            // A flood in the first and fourth intervals, a trickle otherwise.
            let size = if interval == 0 || interval == 3 {
                MB
            } else {
                KB
            };
            seq += size;
            flow.output(seq, size, now);
            let screen = flow.tick(now);
            if let Some(kind) = screen {
                flow.screen_sent(seq, kind, now);
                flow.confirmed(seq, now);
            }
            screens.push(screen);
        }
        // Calm, calm, then a flood: the count starts again.
        let mut expected = vec![Some(Screen::Plain); 6];
        expected.push(Some(Screen::Resume));
        assert_eq!(screens, expected);
        assert!(!flow.is_behind());
        assert_eq!(flow.output(seq + 10, 10, now), Output::Send);
    }

    /// After a flood the missed history waits for a second of calm, and a
    /// flood that keeps coming never sends it.
    #[test]
    fn missed_history_waits_for_calm() {
        let mut flow = Flow::new();
        let (seq, now) = behind(&mut flow, Instant::now());
        flow.screen_sent(seq, Screen::Resume, now);
        assert!(flow.wants_tick(), "history is owed");
        // Raw output at a flood's rate, confirmed as it goes: no history.
        let mut seq = seq;
        let mut now = now;
        for _ in 0..100 {
            now += Duration::from_millis(10);
            seq += 64 * KB;
            assert_eq!(flow.output(seq, 64 * KB, now), Output::Send);
            flow.confirmed(seq, now);
            assert_eq!(flow.tick(now), None);
        }
        // Quiet now: nothing until a second has passed, then the history.
        assert_eq!(flow.tick(now + Duration::from_millis(500)), None);
        let later = now + CALM_FOR + RATE_SPAN;
        assert_eq!(flow.tick(later), Some(Screen::WithHistory));
        flow.screen_sent(seq, Screen::WithHistory, later);
        assert!(!flow.wants_tick());
    }

    #[test]
    fn asked_for_the_history_goes_at_once() {
        let mut flow = Flow::new();
        let (seq, now) = behind(&mut flow, Instant::now());
        flow.screen_sent(seq, Screen::Resume, now);
        let mut seq = seq;
        let mut now = now;
        for _ in 0..10 {
            now += Duration::from_millis(10);
            seq += 64 * KB;
            flow.output(seq, 64 * KB, now);
            flow.confirmed(seq, now);
        }
        assert_eq!(flow.tick(now), None);
        flow.history_wanted();
        assert_eq!(flow.tick(now), Some(Screen::WithHistory));
    }

    #[test]
    fn asking_with_nothing_missing_sends_nothing() {
        let mut flow = Flow::new();
        flow.history_wanted();
        assert_eq!(flow.tick(Instant::now()), None);
        assert!(!flow.wants_tick());
    }

    #[test]
    fn output_a_screen_already_covers_is_not_sent() {
        let now = Instant::now();
        let mut flow = Flow::new();
        flow.screen_sent(1000, Screen::WithHistory, now);
        assert_eq!(flow.output(900, 100, now), Output::Drop);
        assert_eq!(flow.output(1000, 100, now), Output::Drop);
        assert_eq!(flow.output(1100, 100, now), Output::Send);
    }

    #[test]
    fn hidden_sends_nothing_and_shown_again_gets_a_screen() {
        let start = Instant::now();
        let mut flow = Flow::new();
        assert_eq!(flow.visible(false), None);
        assert_eq!(flow.output(100, 100, start), Output::Drop);
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
        let (_, now) = behind(&mut flow, Instant::now());
        assert_eq!(flow.attached(), Screen::WithHistory);
        flow.screen_sent(20 * MB, Screen::WithHistory, now);
        assert!(!flow.is_behind());
        assert_eq!(flow.output(20 * MB + 100, 100, now), Output::Send);
    }
}
