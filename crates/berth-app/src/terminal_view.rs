//! The GPUI element that paints a terminal grid.
//!
//! Written against gpui's primitives rather than lifted from Zed, whose
//! element depends on `editor`, `theme`, `settings`, `ui` and `workspace`
//! (see decisions.md D17). The shape is Zed's: paint from an owned snapshot,
//! batch contiguous same-style cells into one shaped line, and never hold the
//! grid lock while painting.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use gpui::{
    Bounds, Context, Element, ElementId, Entity, FocusHandle, Focusable, FontStyle, FontWeight,
    GlobalElementId, Hsla, InspectorElementId, InteractiveElement, IntoElement, LayoutId,
    ParentElement, Pixels, Point, SharedString, StrikethroughStyle, Style, Styled, Task, TextAlign,
    TextRun, UnderlineStyle, Window, fill, point, px, size,
};

use crate::terminal_style::{CursorShape, TerminalStyle};

/// The longest a frame waits for the engine to reflow a new size.
const RESIZE_WAIT: Duration = Duration::from_millis(3);
use berth_core::pty::Pty;
use berth_term::size::{CellSize, TermSize};
use berth_term::terminal::Selection;
use berth_term::terminal::{Rgb, Snapshot};
use berth_term::{KeyEvent, MouseButton as TermButton, Scroll, Terminal, WheelAction};

use crate::metrics::{Histogram, KeystrokeLatency, LatencyReport};

/// Shared between the output pump and the UI. No lock: the terminal hands
/// everything to its own thread, and a frame takes the latest screen it
/// published.
pub type SharedTerminal = Arc<Terminal>;

/// How long every vsync keeps looking for a newer screen after a keystroke
/// while its echo is awaited: about twice the recent echo time, within these
/// bounds. Past it, a key with no echo (a password, a program not drawing)
/// stops costing anything, and the echo that does come is shown the ordinary
/// way, a frame later at worst.
const ECHO_WAIT_FLOOR: Duration = Duration::from_millis(50);
const ECHO_WAIT_CEILING: Duration = Duration::from_secs(1);

/// How long to look for an echo, given the recent echo time. Until one has
/// been measured, the ceiling.
fn echo_wait(recent: Option<Duration>) -> Duration {
    recent.map_or(ECHO_WAIT_CEILING, |recent| {
        (recent * 2).clamp(ECHO_WAIT_FLOOR, ECHO_WAIT_CEILING)
    })
}

/// Fold one echo time into the recent one, a quarter at a time, so one slow
/// echo moves it but does not decide it. Output later than the ceiling is not
/// taken for an echo: it is a command that took a while to print.
fn fold_echo(recent: Option<Duration>, sample: Duration) -> Option<Duration> {
    if sample >= ECHO_WAIT_CEILING {
        return recent;
    }
    Some(recent.map_or(sample, |recent| (recent * 3 + sample) / 4))
}

/// What the output pump, on its own thread, shares with the view.
#[derive(Default)]
pub struct Intake {
    /// Bytes handed to the parser, so throughput claims can be checked.
    bytes: AtomicU64,
    /// When the most recent keystroke was sent, until its echo arrives.
    keystroke: Mutex<Option<Instant>>,
    /// The echo, until a frame shows it.
    echo: Mutex<Option<Echo>>,
    /// The recent keystroke-to-echo time, which sets how long to look.
    recent_echo: Mutex<Option<Duration>>,
    /// When output last arrived: with no frame since, the window is not being
    /// drawn (D50).
    last_output: Mutex<Option<Instant>>,
    /// When the session last said input was dropped: its program is not
    /// reading (D50).
    input_dropped: Mutex<Option<Instant>>,
}

/// How long a pane says its input was not delivered.
pub const INPUT_NOTICE_FOR: Duration = Duration::from_secs(5);

/// The first output after a keystroke.
#[derive(Debug, Clone, Copy)]
pub struct Echo {
    pub arrived: Instant,
    /// Keystroke sent to echo received: the network half.
    pub round_trip: Duration,
    /// The write it was, so a frame can tell whether it shows it.
    pub seq: u64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Intake {
    /// The session dropped input: its program is not reading.
    pub fn note_input_dropped(&self) {
        *lock(&self.input_dropped) = Some(Instant::now());
    }

    /// Whether to say so still.
    pub fn input_dropped_recently(&self) -> bool {
        lock(&self.input_dropped).is_some_and(|at| at.elapsed() < INPUT_NOTICE_FOR)
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub fn reset_bytes(&self) {
        self.bytes.store(0, Ordering::Relaxed);
    }

    fn sent_key(&self) {
        *lock(&self.keystroke) = Some(Instant::now());
    }

    /// Output handed to the parser as write `seq`. The first after a
    /// keystroke is its echo: the round trip ends here, and whatever follows
    /// is Berth's own render cost.
    pub fn arrived(&self, len: usize, seq: u64) {
        self.bytes.fetch_add(len as u64, Ordering::Relaxed);
        *lock(&self.last_output) = Some(Instant::now());
        if let Some(sent) = lock(&self.keystroke).take() {
            let round_trip = sent.elapsed();
            let mut recent = lock(&self.recent_echo);
            *recent = fold_echo(*recent, round_trip);
            *lock(&self.echo) = Some(Echo {
                arrived: Instant::now(),
                round_trip,
                seq,
            });
        }
    }

    /// When output last arrived.
    pub fn last_output(&self) -> Option<Instant> {
        *lock(&self.last_output)
    }

    /// The echo, if a frame showing write `painted` shows it.
    fn painted(&self, painted: u64) -> Option<Echo> {
        let mut echo = lock(&self.echo);
        if echo.is_some_and(|e| e.seq <= painted) {
            echo.take()
        } else {
            None
        }
    }

    /// Whether a keystroke's echo is still to reach the screen.
    fn awaiting_echo(&self) -> bool {
        lock(&self.echo).is_some()
            || lock(&self.keystroke)
                .is_some_and(|sent| sent.elapsed() < echo_wait(*lock(&self.recent_echo)))
    }
}

/// What the font system resolved, as opposed to what was requested.
///
/// The requested family is often not installed, and both Core Text and
/// DirectWrite substitute silently rather than failing, so the request tells
/// you nothing about what is being measured. gpui cannot name the face it
/// landed on either: `get_font_for_id` returns the `Font` that was asked for,
/// and `all_font_names` returns 11 entries on macOS, mostly gpui's own
/// fallback stack, with none of the installed families in it.
///
/// So the face is identified by its metrics rather than by name. Two runs
/// agreeing on all of these are using the same face whatever it is called, and
/// `m` against `i` is the check that cannot be argued with: equal in every
/// monospace face, unequal in a proportional one, which is the failure mode
/// that makes a terminal grid quietly wrong.
#[derive(Debug, Clone, PartialEq)]
pub struct FontFacts {
    pub advance_m: f32,
    pub advance_i: f32,
    pub units_per_em: u32,
    pub ascent: f32,
    pub descent: f32,
    pub cap_height: f32,
    pub x_height: f32,
}

impl FontFacts {
    /// True when `m` and `i` advance identically, as they must in a grid.
    pub fn is_monospace(&self) -> bool {
        (self.advance_m - self.advance_i).abs() < 0.01
    }
}

/// Agent signal payloads a pane received, raw and untrusted (D48). The
/// workspace holds the pane's token and decides what to believe.
pub struct AgentSignals(pub Vec<String>);

impl gpui::EventEmitter<AgentSignals> for TerminalView {}

pub struct TerminalView {
    terminal: SharedTerminal,
    pty: Option<Pty>,
    focus: FocusHandle,
    style: TerminalStyle,
    /// Frame build times, the basis of the heavy-output acceptance test.
    pub frames: Histogram,
    /// The slowest frames since the histogram was last reset, each with the
    /// part spent taking the screen from the engine, so a long frame can be
    /// put down to the engine or to drawing, and which frame it was, so a
    /// one-off first frame can be told from a steady cost.
    pub slow_frames: Vec<(Duration, Duration, u64)>,
    /// When the last frame finished, and the longest time between two frames
    /// since the histogram was reset. Build time alone cannot see a window that
    /// draws nothing while the main thread is busy; this can.
    pub last_frame_at: Option<Instant>,
    pub longest_gap: Duration,
    /// Size changes told to the pty, and frames drawn at a size other than
    /// the one their bounds asked for, since the last reset: what the resize
    /// benchmark holds to one per frame and to none (D53).
    pub resizes_sent: u64,
    pub stale_frames: u64,
    /// The background's corners: rounded where the pane meets the edge of
    /// the pane area, square where it meets another pane or its header
    /// (D53). A plain rounded fill, never a mask.
    pub corners: gpui::Corners<Pixels>,
    /// Last size announced to the remote, so window_change is sent once.
    last_sent_size: Option<TermSize>,
    /// Shared with the output pump: bytes in, and the keystroke and echo times
    /// that split latency into the network half and the render half.
    pub intake: Arc<Intake>,
    /// Keystroke latency samples, network and local reported separately.
    pub latency: LatencyReport,
    /// Cell metrics as the platform font system resolved them. Reported by the
    /// benchmarks because font metrics are the most likely thing to differ
    /// between platforms, and a metric difference looks like a latency
    /// difference if it is not shown.
    pub resolved_cell: Option<CellSize>,
    /// What the font system actually gave us, as opposed to what was asked
    /// for. Reported by the benchmarks; see `FontFacts`.
    pub resolved_font: Option<FontFacts>,
    /// The current local selection, if the user is dragging or has dragged.
    selection: Option<Selection>,
    /// Where a drag started, kept so the selection can be rebuilt as it moves.
    drag_anchor: Option<(u16, u16)>,
    /// Whether the press that began the current gesture went to the program.
    /// Decided once at the press, so letting go of shift mid-drag cannot send
    /// the program a release it never saw the press for.
    forwarding: bool,
    /// Grid origin and cell size from the last paint, so a mouse position can
    /// be turned into a cell. Set during paint because only the element knows
    /// the laid-out bounds.
    geometry: Option<(Point<Pixels>, CellSize)>,
    /// Wheel movement too small to be a line yet. A trackpad reports pixels,
    /// so without carrying the remainder a slow drag scrolls nothing at all.
    scroll_px: f32,
    /// Repaints, and hands on agent signals, whenever the engine publishes.
    _published: Task<()>,
}

impl TerminalView {
    pub fn new(
        terminal: SharedTerminal,
        pty: Option<Pty>,
        style: TerminalStyle,
        cx: &mut Context<Self>,
    ) -> Self {
        // The engine publishes on its own thread; this carries the news to the
        // UI thread. A Notify keeps one permit, so a flood of publishes between
        // two turns of this loop is one repaint, not a queue of them.
        let published = Arc::new(tokio::sync::Notify::new());
        terminal.on_publish({
            let published = published.clone();
            move || published.notify_one()
        });
        let _published = cx.spawn(async move |this, cx| {
            loop {
                published.notified().await;
                let updated = this.update(cx, |view, cx| {
                    let signals = view.terminal.take_agent_signals();
                    if !signals.is_empty() {
                        cx.emit(AgentSignals(signals));
                    }
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        Self {
            terminal,
            pty,
            focus: cx.focus_handle(),
            style,
            frames: Histogram::new(),
            slow_frames: Vec::new(),
            last_frame_at: None,
            longest_gap: Duration::ZERO,
            resizes_sent: 0,
            stale_frames: 0,
            corners: gpui::Corners::default(),
            last_sent_size: None,
            intake: Arc::new(Intake::default()),
            latency: crate::metrics::LatencyReport::new(),
            resolved_cell: None,
            resolved_font: None,
            selection: None,
            drag_anchor: None,
            forwarding: false,
            geometry: None,
            scroll_px: 0.0,
            _published,
        }
    }

    /// The terminal this view draws, for the output pump.
    pub fn terminal(&self) -> SharedTerminal {
        self.terminal.clone()
    }

    /// Bytes handed to the parser since the last reset.
    pub fn bytes_in(&self) -> u64 {
        self.intake.bytes()
    }

    /// Note a frame's time and the gap since the one before, keeping the five
    /// slowest.
    fn note_frame(&mut self, total: Duration, copy: Duration) {
        let now = Instant::now();
        if let Some(last) = self.last_frame_at {
            self.longest_gap = self.longest_gap.max(now.duration_since(last));
        }
        self.last_frame_at = Some(now);
        self.slow_frames.push((total, copy, self.frames.count()));
        self.slow_frames.sort_by_key(|f| std::cmp::Reverse(f.0));
        self.slow_frames.truncate(5);
    }

    /// The five slowest frames, for a benchmark report.
    pub fn slow_frames_report(&self) -> String {
        let frames: Vec<String> = self
            .slow_frames
            .iter()
            .map(|(total, copy, n)| {
                format!(
                    "{:.2} ms at frame {n} (copy {:.2}, draw {:.2})",
                    total.as_secs_f64() * 1000.0,
                    copy.as_secs_f64() * 1000.0,
                    total.saturating_sub(*copy).as_secs_f64() * 1000.0
                )
            })
            .collect();
        format!("slowest frames: {}\n", frames.join(", "))
    }

    /// The grid's current size, so a reconnect can attach at the same shape.
    pub fn grid_size(&self) -> TermSize {
        self.terminal.size()
    }

    /// Point the view at a new transport without touching the grid.
    ///
    /// This is instant reattach. The terminal, and so everything on screen,
    /// is deliberately left alone: the last rendered screen is still the
    /// truth until the remote contradicts it, and tmux sends a full repaint
    /// on attach which overwrites it in place. Clearing here would produce
    /// exactly the blank flash the reconnect is supposed to avoid.
    pub fn swap_pty(&mut self, pty: Pty) {
        self.pty = Some(pty);
        // The remote is a fresh tmux client that has not been told the size,
        // so forget what the last one was told and let the next paint resize.
        self.last_sent_size = None;
    }

    /// The live screen as bytes that redraw it, and the grid's size, for
    /// saving a local session (D46).
    pub fn saved_screen(&self) -> Option<(String, TermSize)> {
        Some((self.terminal.screen_ansi(), self.terminal.size()))
    }

    /// Start a restored session's new shell in place of its old prompt.
    ///
    /// The saved cursor sits on the line the old shell was waiting on, which
    /// is its prompt. That line is cleared and the new prompt is drawn there,
    /// so the screen reads as if the shell never went away. Everything above it
    /// stays as it was saved.
    pub fn resume(&mut self, pty: Pty) {
        self.terminal.advance(b"\x1b[0m\r\x1b[2K");
        self.swap_pty(pty);
    }

    /// The pty this view is currently writing to.
    ///
    /// Read rather than held by the caller, because a reconnect replaces it and
    /// a benchmark holding the old one would be typing into a dead channel.
    pub fn pty(&self) -> Option<Pty> {
        self.pty.clone()
    }

    /// Translate a key event and send it to the remote.
    ///
    /// Modes come from the screen on display, which is what the user is
    /// typing at, and never wait on the parser.
    pub fn send_key(&mut self, event: KeyEvent, cx: &mut Context<Self>) {
        let application_cursor = self.terminal.latest().modes.application_cursor;
        let mode = berth_term::TermMode { application_cursor };
        let Some(bytes) = berth_term::encode(event, mode) else {
            return;
        };
        // Typing goes to the live grid, so it is never typed into a screen the
        // user cannot see.
        self.scroll_to_bottom();
        // Timed from here so keystroke latency measures the whole journey.
        self.intake.sent_key();
        self.write(bytes);
        // Every vsync looks for the echo until it is on screen: see the paint.
        cx.notify();
    }

    /// Tell the remote the grid changed size. Only sent when it actually
    /// changed, since a window_change per frame would flood the channel.
    fn sync_remote_size(&mut self, size: TermSize) {
        if self.last_sent_size == Some(size) {
            return;
        }
        self.last_sent_size = Some(size);
        self.resizes_sent += 1;
        let Some(pty) = self.pty.clone() else { return };
        crate::runtime::handle().spawn(async move {
            pty.resize(size.cols, size.rows).await;
        });
    }

    pub fn style(&self) -> &TerminalStyle {
        &self.style
    }

    /// Change the text size.
    ///
    /// Nothing else is needed: the cell size is measured from the font during
    /// prepaint, so the grid reshapes itself on the next frame and the remote
    /// is told through the resize that paint already sends.
    pub fn set_font_size(&mut self, size: f32) {
        self.style.font_size = size;
    }

    /// Which cell a window position falls on, clamped to the grid.
    fn cell_at(&self, position: Point<Pixels>) -> Option<(u16, u16)> {
        let (origin, cell) = self.geometry?;
        let column = ((f32::from(position.x) - f32::from(origin.x)) / cell.width).floor();
        let line = ((f32::from(position.y) - f32::from(origin.y)) / cell.height).floor();
        Some((
            line.max(0.0).min(u16::MAX as f32) as u16,
            column.max(0.0).min(u16::MAX as f32) as u16,
        ))
    }

    /// True when the remote asked for mouse events, in which case a click
    /// belongs to the program rather than to local selection.
    fn forwards_mouse(&self) -> bool {
        self.terminal.latest().modes.mouse_reporting
    }

    fn on_mouse_down(&mut self, position: Point<Pixels>, shift: bool) {
        let Some((line, column)) = self.cell_at(position) else {
            return;
        };
        self.forwarding = !click_is_local(self.forwards_mouse(), shift);
        if self.forwarding {
            self.send_mouse(TermButton::Left, column, line, true);
            return;
        }
        self.drag_anchor = Some((line, column));
        self.selection = None;
    }

    fn on_mouse_drag(&mut self, position: Point<Pixels>) {
        if self.forwarding {
            return;
        }
        let (Some((anchor_line, anchor_column)), Some((line, column))) =
            (self.drag_anchor, self.cell_at(position))
        else {
            return;
        };
        self.selection = Some(Selection::new(anchor_line, anchor_column, line, column));
    }

    fn on_mouse_up(&mut self, position: Point<Pixels>) {
        if std::mem::take(&mut self.forwarding) {
            if let Some((line, column)) = self.cell_at(position) {
                self.send_mouse(TermButton::Left, column, line, false);
            }
            return;
        }
        self.drag_anchor = None;
    }

    fn send_mouse(&self, button: TermButton, column: u16, line: u16, pressed: bool) {
        self.write(berth_term::mouse_sgr(button, column, line, pressed));
    }

    /// Move the view through the scrollback.
    ///
    /// A selection is dropped when the view moves, because its coordinates
    /// are rows of the viewport rather than of the history: left alone it
    /// would stay painted over whatever scrolled into those rows, and copy it
    /// too. Going to the bottom when already there is not a move.
    pub fn scroll(&mut self, scroll: Scroll) {
        let shown = self.terminal.latest();
        let moves = match scroll {
            Scroll::Bottom | Scroll::PageDown => shown.display_offset > 0,
            Scroll::Lines(lines) => lines > 0 || shown.display_offset > 0,
            Scroll::PageUp => true,
        };
        if !moves {
            return;
        }
        self.terminal.scroll(scroll);
        self.selection = None;
        self.drag_anchor = None;
        // Up into the history: a session on Berth's own server that fell
        // behind fetches the lines it missed (D50). Asked once per gap.
        if matches!(scroll, Scroll::Lines(lines) if lines > 0) || scroll == Scroll::PageUp {
            self.want_history();
        }
    }

    fn want_history(&self) {
        let Some(pty) = self.pty.clone() else { return };
        crate::runtime::handle().spawn(async move {
            pty.want_history().await;
        });
    }

    /// Put the view back on the live grid.
    ///
    /// Called when a key is sent, because typing into a shell you cannot see
    /// is the one thing worse than not being able to scroll at all. Every
    /// terminal does this and nobody notices it until it is missing.
    fn scroll_to_bottom(&mut self) {
        self.scroll(Scroll::Bottom);
    }

    /// A wheel notch, which is not always ours: see [`WheelAction`].
    fn on_scroll_wheel(&mut self, event: &gpui::ScrollWheelEvent) {
        let Some((_, cell)) = self.geometry else {
            return;
        };
        let lines = self.wheel_lines(event, cell.height);
        if lines == 0 {
            return;
        }

        let modes = self.terminal.latest().modes;
        let application_cursor = modes.application_cursor;
        match modes.wheel_action(event.modifiers.shift) {
            WheelAction::View => self.scroll(Scroll::Lines(lines)),
            WheelAction::Arrows => {
                let mode = berth_term::TermMode { application_cursor };
                self.write(berth_term::alt_scroll(lines, mode));
            }
            WheelAction::Report => {
                let Some((line, column)) = self.cell_at(event.position) else {
                    return;
                };
                let button = if lines > 0 {
                    TermButton::ScrollUp
                } else {
                    TermButton::ScrollDown
                };
                // One report per line, as xterm does. A program counting
                // notches would otherwise see a fast flick as a single one.
                for _ in 0..lines.unsigned_abs() {
                    self.send_mouse(button, column, line, true);
                }
            }
        }
    }

    /// How many whole lines a wheel event is worth.
    fn wheel_lines(&mut self, event: &gpui::ScrollWheelEvent, line_height: f32) -> i32 {
        match event.touch_phase {
            // A new gesture starts from nothing, so a flick left over from the
            // last one cannot leak into this one.
            gpui::TouchPhase::Started => {
                self.scroll_px = 0.0;
                0
            }
            gpui::TouchPhase::Moved => whole_lines(
                f32::from(event.delta.pixel_delta(px(line_height)).y),
                line_height,
                &mut self.scroll_px,
            ),
            // Neither an end nor a cancel commits anything of its own.
            _ => 0,
        }
    }

    /// Send bytes to the remote, if there is one.
    fn write(&self, bytes: Vec<u8>) {
        let Some(pty) = self.pty.clone() else { return };
        crate::runtime::handle().spawn(async move {
            pty.write(bytes).await;
        });
    }

    /// The selected text, for copying.
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let text = self.terminal.selection_text(selection);
        (!text.is_empty()).then_some(text)
    }

    /// Paste, honouring the remote's bracketed paste mode.
    pub fn paste(&self, text: &str) {
        let bracketed = self.terminal.latest().modes.bracketed_paste;
        self.write(berth_term::paste_bytes(text, bracketed));
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl gpui::Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let element = TerminalElement {
            view: cx.entity(),
            style: self.style.clone(),
        };
        gpui::div()
            .track_focus(&self.focus)
            .key_context("Terminal")
            .size_full()
            .on_key_down(
                cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
                    // Copy and paste are chords the terminal never sees; a
                    // bare Ctrl+C must still reach the remote as an interrupt,
                    // which is why copy is Ctrl+Shift+C.
                    match crate::input::clipboard_action(&event.keystroke) {
                        Some(crate::input::Clipboard::Copy) => {
                            if let Some(text) = this.selected_text() {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                            }
                        }
                        Some(crate::input::Clipboard::Paste) => {
                            if let Some(text) =
                                cx.read_from_clipboard().and_then(|item| item.text())
                            {
                                this.paste(&text);
                            }
                        }
                        None => match crate::input::scroll_action(&event.keystroke) {
                            Some(crate::input::ScrollKey::PageUp) => {
                                this.scroll(Scroll::PageUp);
                                cx.notify();
                            }
                            Some(crate::input::ScrollKey::PageDown) => {
                                this.scroll(Scroll::PageDown);
                                cx.notify();
                            }
                            None => {
                                if let Some(key_event) = crate::input::translate(&event.keystroke) {
                                    this.send_key(key_event, cx);
                                }
                            }
                        },
                    }
                }),
            )
            .on_scroll_wheel(
                cx.listener(|this, event: &gpui::ScrollWheelEvent, _window, cx| {
                    this.on_scroll_wheel(event);
                    cx.notify();
                }),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _window, cx| {
                    this.on_mouse_down(event.position, event.modifiers.shift);
                    cx.notify();
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, _window, cx| {
                    if event.dragging() {
                        this.on_mouse_drag(event.position);
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseUpEvent, _window, cx| {
                    this.on_mouse_up(event.position);
                    cx.notify();
                }),
            )
            .child(element)
    }
}

/// Whether a click belongs to Berth's own selection rather than the program.
///
/// A program that asked for mouse events gets them, which is what makes
/// clicking work in vim or htop. Shift takes the mouse back, as it takes the
/// wheel back (D42), because otherwise a program like Claude Code that asks for
/// the mouse leaves no way to select and copy anything on its screen.
fn click_is_local(program_wants_mouse: bool, shift: bool) -> bool {
    !program_wants_mouse || shift
}

pub struct TerminalElement {
    view: Entity<TerminalView>,
    style: TerminalStyle,
}

/// Everything painting needs, computed during prepaint.
pub struct PrepaintState {
    snapshot: Arc<Snapshot>,
    selection: Option<Selection>,
    runs: Vec<CellRun>,
    background: Hsla,
    cell: CellSize,
    font_facts: FontFacts,
    started: Instant,
    /// How long taking the screen from the engine took, of the frame.
    copy: Duration,
}

/// A horizontal stretch of cells sharing one style, shaped as a single line.
struct CellRun {
    text: String,
    line: u16,
    column: u16,
    fg: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
}

fn hsla(c: Rgb) -> Hsla {
    gpui::Rgba {
        r: c.r as f32 / 255.0,
        g: c.g as f32 / 255.0,
        b: c.b as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = gpui::relative(1.0).into();
        style.size.height = gpui::relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> Self::PrepaintState {
        let started = Instant::now();

        // Cell geometry comes from the font, exactly as Zed derives it.
        let font = self.style.font();
        let font_id = window.text_system().resolve_font(&font);
        let size = self.style.font_size_px();
        let advance_of = |ch: char| {
            window
                .text_system()
                .advance(font_id, size, ch)
                .map(|s| f32::from(s.width))
                .unwrap_or(self.style.fallback_advance)
        };
        let advance = advance_of('m');

        // A fingerprint of the face the platform actually chose, so two runs
        // can be compared without either of them being able to name it.
        let text = window.text_system();
        let font_facts = FontFacts {
            advance_m: advance,
            advance_i: advance_of('i'),
            units_per_em: text.units_per_em(font_id),
            ascent: f32::from(text.ascent(font_id, size)),
            descent: f32::from(text.descent(font_id, size)),
            cap_height: f32::from(text.cap_height(font_id, size)),
            x_height: f32::from(text.x_height(font_id, size)),
        };
        let cell = CellSize::new(
            self.style.cell_rounding.apply(advance),
            self.style
                .cell_rounding
                .apply(f32::from(self.style.line_height_px())),
        );

        // Padding is inside the pane, so the grid is measured against what is
        // left after it.
        let size = TermSize::from_pixels(
            f32::from(bounds.size.width) - self.style.padding_x * 2.0,
            f32::from(bounds.size.height) - self.style.padding_y * 2.0,
            cell,
        );

        // The latest screen the engine published, which never waits on the
        // parser. A resize reaches it for a later frame.
        let copy_started = Instant::now();
        // A new size waits a few milliseconds for the engine to reflow, so
        // this frame shows the content at its new size (D53): a divider or a
        // window edge being dragged redraws every frame, never stretched and
        // never blank. A busy engine gets no longer than that.
        let snapshot = {
            let terminal = &self.view.read(cx).terminal;
            terminal.resize_within(size, cell, RESIZE_WAIT)
        };
        if snapshot.size != size {
            self.view.update(cx, |view, _| view.stale_frames += 1);
        }
        let copy = copy_started.elapsed();

        let selection = self.view.read(cx).selection;
        let runs = batch_runs(&snapshot);
        let background = self.style.colour(self.style.background);

        PrepaintState {
            snapshot,
            selection,
            runs,
            background,
            cell,
            font_facts,
            started,
            copy,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request: &mut Self::RequestLayoutState,
        state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        let cell_w = px(state.cell.width);
        let cell_h = px(state.cell.height);
        let origin = point(
            bounds.origin.x + px(self.style.padding_x),
            bounds.origin.y + px(self.style.padding_y),
        );

        let corners = self.view.read(cx).corners;
        window.paint_quad(fill(bounds, state.background).corner_radii(corners));

        // Backgrounds first, as a separate pass, so glyphs are never clipped by
        // a neighbouring cell's fill.
        for cell in state.snapshot.cells() {
            let colour = hsla(cell.bg);
            if colour == state.background {
                continue;
            }
            let at = point(
                origin.x + cell_w * cell.column as f32,
                origin.y + cell_h * cell.line as f32,
            );
            window.paint_quad(fill(Bounds::new(at, size(cell_w, cell_h)), colour));
        }

        // Selection sits above the cell backgrounds and below the glyphs, so
        // the text under it stays readable.
        if let Some(selection) = state.selection.map(Selection::normalised)
            && !selection.is_empty()
        {
            let colour = self.style.selection_colour();
            for cell in state.snapshot.cells() {
                if !selection_covers(selection, cell.line, cell.column) {
                    continue;
                }
                let at = point(
                    origin.x + cell_w * cell.column as f32,
                    origin.y + cell_h * cell.line as f32,
                );
                window.paint_quad(fill(Bounds::new(at, size(cell_w, cell_h)), colour));
            }
        }

        for run in &state.runs {
            let text: SharedString = run.text.clone().into();
            let mut font = self.style.font();
            if run.bold {
                font.weight = FontWeight::BOLD;
            }
            if run.italic {
                font.style = FontStyle::Italic;
            }
            let text_run = TextRun {
                len: text.len(),
                font,
                color: run.fg,
                background_color: None,
                underline: run.underline.then(|| UnderlineStyle {
                    thickness: px(self.style.underline_thickness),
                    color: Some(run.fg),
                    wavy: false,
                }),
                strikethrough: run.strikethrough.then(|| StrikethroughStyle {
                    thickness: px(self.style.strikethrough_thickness),
                    color: Some(run.fg),
                }),
            };
            let at = point(
                origin.x + cell_w * run.column as f32,
                origin.y + cell_h * run.line as f32,
            );
            // force_width pins every glyph to the cell advance, which is what
            // makes a proportional fallback font still line up in the grid.
            let shaped = window.text_system().shape_line(
                text,
                self.style.font_size_px(),
                std::slice::from_ref(&text_run),
                Some(cell_w),
            );
            let _ = shaped.paint(at, cell_h, TextAlign::Left, None, window, cx);
        }

        if state.snapshot.cursor.visible && !state.snapshot.is_scrolled_back() {
            let cursor = &self.style.cursor;
            let at = point(
                origin.x + cell_w * state.snapshot.cursor.column as f32,
                origin.y + cell_h * state.snapshot.cursor.line as f32,
            );
            let thickness = px(cursor.thickness);
            let (at, cursor_size) = match cursor.shape {
                CursorShape::Block => (at, size(cell_w, cell_h)),
                CursorShape::Bar => (at, size(thickness, cell_h)),
                CursorShape::Underline => (
                    point(at.x, at.y + cell_h - thickness),
                    size(cell_w, thickness),
                ),
            };
            window.paint_quad(fill(
                Bounds::new(at, cursor_size),
                self.style.colour(cursor.colour),
            ));
        }

        let elapsed = state.started.elapsed();
        let copy = state.copy;
        let frame_started = state.started;
        let painted_size = state.snapshot.size;
        let painted_seq = state.snapshot.seq;
        let geometry = (origin, state.cell);
        let painted_cell = state.cell;
        let painted_font = state.font_facts.clone();
        let awaiting_echo = self.view.update(cx, |view, _| {
            view.frames.record(elapsed);
            view.note_frame(elapsed, copy);
            view.sync_remote_size(painted_size);
            view.geometry = Some(geometry);
            view.resolved_cell = Some(painted_cell);
            view.resolved_font = Some(painted_font);
            // This frame carried the echoed glyph, so the local half of the
            // keystroke's journey ends now.
            if let Some(echo) = view.intake.painted(painted_seq) {
                view.latency.record(KeystrokeLatency {
                    round_trip: echo.round_trip,
                    local_render: echo.arrived.elapsed(),
                    // Split at the moment this frame started being built, so
                    // the wait for vsync and Berth's own work are separable.
                    wait: frame_started.saturating_duration_since(echo.arrived),
                    paint: elapsed,
                });
            }
            view.intake.awaiting_echo()
        });
        if awaiting_echo {
            let (terminal, intake) = {
                let view = self.view.read(cx);
                (view.terminal.clone(), view.intake.clone())
            };
            watch_for_echo(window, self.view.entity_id(), terminal, intake, painted_seq);
        }
    }
}

/// While an echo is on its way, look at every vsync for a newer screen, and
/// draw only when there is one.
///
/// The engine publishes on its own thread. Waiting instead for that news to
/// reach the UI thread and ask for a frame loses the race with a vsync often
/// enough to miss a frame (D49's latency bug). Drawing every frame outright
/// fixes that too, but costs a full frame per vsync for up to a second after
/// every key, which typing into a password prompt measured at eight times the
/// energy of before (2026-09-30). A look costs a lock and a comparison.
fn watch_for_echo(
    window: &mut Window,
    view: gpui::EntityId,
    terminal: SharedTerminal,
    intake: Arc<Intake>,
    painted: u64,
) {
    window.on_next_frame(move |window, cx| {
        if terminal.latest().seq > painted {
            cx.notify(view);
        } else if intake.awaiting_echo() {
            watch_for_echo(window, view, terminal, intake, painted);
        }
    });
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// How many whole lines a pixel delta is worth, carrying the remainder.
///
/// Pulled out of the view so it can be tested: this is the arithmetic that
/// decides whether a trackpad feels right, and it has two ways to be wrong
/// that a compile never catches. Truncating each event on its own means a slow
/// drag of sub-line deltas scrolls nothing at all, forever. Carrying without
/// subtracting what was already spent means one flick scrolls twice.
///
/// Positive is back into the history, matching the direction a wheel is pushed.
fn whole_lines(delta_y: f32, line_height: f32, carried: &mut f32) -> i32 {
    if line_height <= 0.0 || !delta_y.is_finite() {
        return 0;
    }
    let before = (*carried / line_height) as i32;
    *carried += delta_y;
    (*carried / line_height) as i32 - before
}

/// Whether a selection covers a cell. Mirrors the rule in berth-term, which
/// owns the text extraction; this is the painting half of the same shape.
fn selection_covers(selection: Selection, line: u16, column: u16) -> bool {
    if line < selection.start_line || line > selection.end_line {
        return false;
    }
    if line == selection.start_line && column < selection.start_column {
        return false;
    }
    if line == selection.end_line && column > selection.end_column {
        return false;
    }
    true
}

/// Group contiguous cells on a row that share a style into single runs.
///
/// One `shape_line` call per run rather than per glyph is the difference
/// between a comfortable frame and a stalled one when output is streaming.
fn batch_runs(snapshot: &Snapshot) -> Vec<CellRun> {
    let mut runs: Vec<CellRun> = Vec::new();
    let mut current: Option<CellRun> = None;

    for cell in snapshot.cells() {
        // Blanks paint nothing; breaking the run on them also keeps runs short.
        if cell.c == ' ' || cell.c == '\0' {
            if let Some(run) = current.take() {
                runs.push(run);
            }
            continue;
        }

        let continues = current.as_ref().is_some_and(|run| {
            run.line == cell.line
                && run.column + run.text.chars().count() as u16 == cell.column
                && run.fg == hsla(cell.fg)
                && run.bold == cell.bold
                && run.italic == cell.italic
                && run.underline == cell.underline
                && run.strikethrough == cell.strikethrough
        });

        if continues {
            if let Some(run) = current.as_mut() {
                run.text.push(cell.c);
            }
        } else {
            if let Some(run) = current.take() {
                runs.push(run);
            }
            let mut text = String::with_capacity(64);
            text.push(cell.c);
            current = Some(CellRun {
                text,
                line: cell.line,
                column: cell.column,
                fg: hsla(cell.fg),
                bold: cell.bold,
                italic: cell.italic,
                underline: cell.underline,
                strikethrough: cell.strikethrough,
            });
        }
    }
    if let Some(run) = current.take() {
        runs.push(run);
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::{click_is_local, echo_wait, fold_echo, whole_lines};

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn the_echo_is_looked_for_twice_as_long_as_it_takes() {
        assert_eq!(echo_wait(Some(MS * 40)), MS * 80);
    }

    /// Before any echo, and on a link too slow to bound, the full second.
    #[test]
    fn the_wait_is_bounded_both_ways() {
        assert_eq!(echo_wait(None), Duration::from_secs(1));
        assert_eq!(echo_wait(Some(MS * 5)), MS * 50);
        assert_eq!(echo_wait(Some(MS * 900)), Duration::from_secs(1));
    }

    #[test]
    fn one_slow_echo_moves_the_recent_time_without_deciding_it() {
        let recent = fold_echo(Some(MS * 40), MS * 200);
        assert_eq!(recent, Some(MS * 80));
    }

    /// A command that prints after two seconds is not an echo.
    #[test]
    fn late_output_is_not_taken_for_an_echo() {
        assert_eq!(fold_echo(Some(MS * 40), MS * 2000), Some(MS * 40));
        assert_eq!(fold_echo(None, MS * 30), Some(MS * 30));
    }

    const LINE: f32 = 16.0;

    #[test]
    fn a_full_line_of_wheel_is_one_line() {
        let mut carried = 0.0;
        assert_eq!(whole_lines(LINE, LINE, &mut carried), 1);
        assert_eq!(whole_lines(-LINE, LINE, &mut carried), -1);
    }

    /// The trackpad case. Four sub-line nudges are a line between them, and
    /// truncating each on its own would scroll nothing at all.
    #[test]
    fn small_deltas_accumulate_into_a_line() {
        let mut carried = 0.0;
        let quarter = LINE / 4.0;
        assert_eq!(whole_lines(quarter, LINE, &mut carried), 0);
        assert_eq!(whole_lines(quarter, LINE, &mut carried), 0);
        assert_eq!(whole_lines(quarter, LINE, &mut carried), 0);
        assert_eq!(whole_lines(quarter, LINE, &mut carried), 1);
    }

    /// The other way to get this wrong: carrying without subtracting what was
    /// already spent, so one flick scrolls twice.
    #[test]
    fn a_line_already_spent_is_not_spent_again() {
        let mut carried = 0.0;
        assert_eq!(whole_lines(LINE, LINE, &mut carried), 1);
        assert_eq!(whole_lines(0.0, LINE, &mut carried), 0);
        assert_eq!(whole_lines(LINE / 2.0, LINE, &mut carried), 0);
        assert_eq!(whole_lines(LINE / 2.0, LINE, &mut carried), 1);
    }

    #[test]
    fn a_reversal_cancels_what_was_carried() {
        let mut carried = 0.0;
        assert_eq!(whole_lines(LINE * 2.0, LINE, &mut carried), 2);
        assert_eq!(whole_lines(-LINE * 2.0, LINE, &mut carried), -2);
        assert_eq!(carried, 0.0);
    }

    /// Geometry before the first paint, and anything a platform hands over
    /// that is not a number. Neither is allowed to become a scroll.
    #[test]
    fn a_nonsense_delta_scrolls_nothing() {
        let mut carried = 0.0;
        assert_eq!(whole_lines(LINE, 0.0, &mut carried), 0);
        assert_eq!(whole_lines(f32::NAN, LINE, &mut carried), 0);
        assert_eq!(whole_lines(f32::INFINITY, LINE, &mut carried), 0);
        assert_eq!(carried, 0.0);
    }

    /// The check the benchmark header leans on: a proportional face is the one
    /// failure mode that makes every cell measurement meaningless.
    #[test]
    fn the_monospace_check_separates_a_grid_font_from_a_proportional_one() {
        let facts = |m: f32, i: f32| FontFacts {
            advance_m: m,
            advance_i: i,
            units_per_em: 2048,
            ascent: 12.09,
            descent: 3.03,
            cap_height: 9.36,
            x_height: 6.84,
        };
        assert!(facts(7.8, 7.8).is_monospace());
        assert!(!facts(11.2, 3.61).is_monospace());
    }

    use super::*;
    use berth_term::terminal::Terminal as CoreTerminal;

    fn snapshot_of(input: &[u8], cols: u16, rows: u16) -> Snapshot {
        let t = CoreTerminal::new(TermSize::new(cols, rows), CellSize::new(8.0, 16.0));
        t.advance(input);
        (*t.snapshot()).clone()
    }

    #[test]
    fn plain_text_becomes_one_run() {
        let runs = batch_runs(&snapshot_of(b"hello", 20, 3));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "hello");
        assert_eq!((runs[0].line, runs[0].column), (0, 0));
    }

    #[test]
    fn a_colour_change_splits_the_run() {
        let runs = batch_runs(&snapshot_of(b"ab\x1b[31mcd", 20, 3));
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "ab");
        assert_eq!(runs[1].text, "cd");
        assert_ne!(runs[0].fg, runs[1].fg);
    }

    #[test]
    fn a_gap_splits_the_run() {
        let runs = batch_runs(&snapshot_of(b"ab  cd", 20, 3));
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].column, 0);
        assert_eq!(runs[1].column, 4);
    }

    #[test]
    fn rows_never_merge() {
        let runs = batch_runs(&snapshot_of(b"ab\r\ncd", 20, 3));
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].line, 0);
        assert_eq!(runs[1].line, 1);
    }

    #[test]
    fn an_empty_grid_produces_no_runs() {
        assert!(batch_runs(&snapshot_of(b"", 20, 3)).is_empty());
    }

    #[test]
    fn attributes_split_runs() {
        let runs = batch_runs(&snapshot_of(b"a\x1b[1mb", 20, 3));
        assert_eq!(runs.len(), 2);
        assert!(!runs[0].bold);
        assert!(runs[1].bold);
    }

    /// Batching is the performance property; a full row of one style must not
    /// become one shaping call per glyph.
    #[test]
    fn a_uniform_row_is_a_single_run() {
        let runs = batch_runs(&snapshot_of(&b"x".repeat(80), 80, 3));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text.chars().count(), 80);
    }

    #[test]
    fn wide_glyphs_keep_their_column_positions() {
        let runs = batch_runs(&snapshot_of("日本".as_bytes(), 20, 3));
        // The wide glyphs occupy columns 0 and 2, so they cannot be one run.
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].column, 0);
        assert_eq!(runs[1].column, 2);
    }

    /// Shift-drag is how text gets copied out of a program that took the
    /// mouse. Without shift the program keeps its clicks.
    #[test]
    fn shift_takes_the_mouse_back_for_selection() {
        assert!(click_is_local(false, false));
        assert!(click_is_local(false, true));
        assert!(!click_is_local(true, false));
        assert!(click_is_local(true, true));
    }
}
