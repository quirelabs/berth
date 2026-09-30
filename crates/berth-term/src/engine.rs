//! The terminal engine: libghostty-vt, on a thread of its own (D49).
//!
//! libghostty-vt's terminal can neither move between threads nor be shared, so
//! each one lives on its own thread and everything reaches it over a channel.
//! Nothing waits for it on the way in: [`Terminal::advance`] hands the bytes
//! over and returns, so a flood of output is parsed while the window keeps
//! drawing. After each batch of work, and at least every [`PUBLISH_EVERY`]
//! while output keeps coming, the engine publishes a copy of the screen, and
//! the renderer takes the latest one with [`Terminal::latest`]. Only the rows
//! that changed are built again; the rest are shared with the copy before.
//!
//! Reads that must see every byte already handed over wait for the engine to
//! catch up first: the modes a caller checks right after a write, the text
//! being copied, the screen saved on quit. The renderer never does.
//!
//! OSC 133 markers and Berth's agent signals are taken out of the stream on
//! the engine thread, so a marker records the cursor row at exactly its point
//! in the output.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use libghostty_vt::RenderState;
use libghostty_vt::render::{CellIteration, CellIterator, Dirty, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::{Palette as GhosttyPalette, RgbColor, Underline};
use libghostty_vt::terminal::{Mode, ScrollViewport};

use berth_proto::v1;

use crate::shell::{Piece, Scanner, ShellEvent};
use crate::size::{CellSize, TermSize};
use crate::terminal::{
    Cell, Cursor, MAX_QUEUED_SIGNALS, Modes, Palette, Rgb, SCROLLBACK_LINES, Scroll, Selection,
    Snapshot, Style, WheelAction,
};

/// The longest the engine parses before publishing what it has. Half a frame
/// at 60 Hz, so a frame drawn during a flood is never more than that behind.
pub const PUBLISH_EVERY: Duration = Duration::from_millis(8);

/// How long a read that needs the engine caught up waits before settling for
/// what was last published. Only reached if the engine is stuck.
const CATCH_UP_LIMIT: Duration = Duration::from_secs(5);

type Wake = Arc<dyn Fn() + Send + Sync>;

enum Command {
    Write(Vec<u8>),
    Resize(TermSize, CellSize),
    Scroll(Scroll),
    Palette(Palette),
    /// The live screen as escape sequences, wherever the view is scrolled to.
    LiveScreen(mpsc::SyncSender<String>),
    /// The active screen in Berth's wire format.
    WireScreen(mpsc::SyncSender<Option<v1::Screen>>),
    /// How many history lines the primary screen holds.
    HistoryHeld(mpsc::SyncSender<usize>),
    /// History lines older than `before`, or the newest if `None`.
    WireHistory {
        before: Option<u64>,
        max: u32,
        reply: mpsc::SyncSender<WireHistory>,
    },
}

/// How much history a terminal keeps. Both limits apply and the first reached
/// wins (D49), so both are always stated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scrollback {
    pub lines: Option<usize>,
    pub bytes: Option<usize>,
}

impl Scrollback {
    /// The app's own copy of a screen: the line count Berth means, and no byte
    /// limit, which left in silently caps the history far below the line count
    /// (found in the D49 spike).
    pub const VIEW: Scrollback = Scrollback {
        lines: Some(SCROLLBACK_LINES),
        bytes: None,
    };
}

/// History lines as the wire has them: newest first, numbered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WireHistory {
    /// The number of the first line in `lines`, counted from the first line
    /// the session ever produced. Meaningless when `lines` is empty.
    pub newest: u64,
    pub lines: Vec<v1::Row>,
}

/// A command, numbered in the order it was sent.
struct Message {
    seq: u64,
    command: Command,
}

/// What the engine has made public, and how far through the commands it is.
struct Published {
    /// The last command reflected here.
    done: u64,
    /// The engine thread has gone; nothing will ever catch up.
    exited: bool,
    snapshot: Arc<Snapshot>,
    /// Bumped whenever the screen changed in a way worth a repaint.
    revision: u64,
    shell_events: Vec<ShellEvent>,
    agent_signals: Vec<String>,
    seen_shell_marker: bool,
}

struct Shared {
    published: Mutex<Published>,
    /// Signalled on every publish.
    progressed: Condvar,
    /// Bytes handed over and not yet parsed.
    backlog: AtomicUsize,
    wake: Mutex<Option<Wake>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Published> {
        self.published
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The sending side, numbered under one lock so the numbers follow the order
/// the engine receives the commands in.
struct Outbox {
    engine: Option<mpsc::Sender<Message>>,
    sent: u64,
    size: TermSize,
    cell_size: CellSize,
}

/// One terminal. Shared freely: every method takes `&self`, so the output
/// pump and the UI hold the same one without a lock around it.
pub struct Terminal {
    outbox: Mutex<Outbox>,
    shared: Arc<Shared>,
}

impl Terminal {
    pub fn new(size: TermSize, cell_size: CellSize) -> Self {
        Self::with_scrollback(size, cell_size, Scrollback::VIEW)
    }

    pub fn with_scrollback(size: TermSize, cell_size: CellSize, scrollback: Scrollback) -> Self {
        let palette = Palette::default();
        let shared = Arc::new(Shared {
            published: Mutex::new(Published {
                done: 0,
                exited: false,
                snapshot: Arc::new(blank(size, cell_size, &palette)),
                revision: 0,
                shell_events: Vec::new(),
                agent_signals: Vec::new(),
                seen_shell_marker: false,
            }),
            progressed: Condvar::new(),
            backlog: AtomicUsize::new(0),
            wake: Mutex::new(None),
        });
        let (tx, rx) = mpsc::channel();
        let for_engine = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("berth-terminal".into())
            .spawn(move || {
                match Engine::new(size, cell_size, scrollback, for_engine.clone()) {
                    Some(engine) => engine.run(rx),
                    None => tracing::warn!("libghostty could not create a terminal"),
                }
                let mut published = for_engine.lock();
                published.exited = true;
                drop(published);
                for_engine.progressed.notify_all();
            });
        let engine = match spawned {
            Ok(_) => Some(tx),
            Err(error) => {
                tracing::warn!(%error, "could not start the terminal engine thread");
                shared.lock().exited = true;
                None
            }
        };
        Self {
            outbox: Mutex::new(Outbox {
                engine,
                sent: 0,
                size,
                cell_size,
            }),
            shared,
        }
    }

    fn outbox(&self) -> MutexGuard<'_, Outbox> {
        self.outbox
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Hand a command to the engine, and return its number.
    fn send(&self, command: Command) -> u64 {
        let mut outbox = self.outbox();
        outbox.sent += 1;
        let seq = outbox.sent;
        if let Some(engine) = &outbox.engine {
            let _ = engine.send(Message { seq, command });
        }
        seq
    }

    /// Wait until the engine has published everything sent so far.
    fn caught_up(&self) -> MutexGuard<'_, Published> {
        let target = self.outbox().sent;
        let deadline = Instant::now() + CATCH_UP_LIMIT;
        let mut published = self.shared.lock();
        while published.done < target && !published.exited {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                tracing::warn!("the terminal engine did not catch up in time");
                break;
            }
            published = match self.shared.progressed.wait_timeout(published, left) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        published
    }

    /// Called on the engine's thread each time it publishes a screen that
    /// differs from the last, or has new signals or shell events. Cheap work
    /// only: it is how the UI learns there is something to draw.
    pub fn on_publish(&self, wake: impl Fn() + Send + Sync + 'static) {
        if let Ok(mut slot) = self.shared.wake.lock() {
            *slot = Some(Arc::new(wake));
        }
    }

    /// Replace the colour palette. Owned by the app so every visual constant
    /// lives in one struct there.
    pub fn set_palette(&self, palette: Palette) {
        self.send(Command::Palette(palette));
    }

    pub fn size(&self) -> TermSize {
        self.outbox().size
    }

    /// Bumped whenever the screen changes in a way worth a repaint.
    pub fn revision(&self) -> u64 {
        self.caught_up().revision
    }

    /// Hand raw PTY bytes to the parser, without waiting for it. Returns the
    /// write's number, which [`Snapshot::seq`] reaches once it is on screen.
    pub fn advance(&self, bytes: &[u8]) -> u64 {
        if bytes.is_empty() {
            return self.outbox().sent;
        }
        self.shared
            .backlog
            .fetch_add(bytes.len(), Ordering::Relaxed);
        self.send(Command::Write(bytes.to_vec()))
    }

    /// Bytes handed over and not yet parsed.
    pub fn backlog(&self) -> usize {
        self.shared.backlog.load(Ordering::Relaxed)
    }

    /// Block until the backlog is at most `limit`, so a producer faster than
    /// the parser is held back rather than queueing without bound. For the
    /// output pump's thread; never call it on the UI thread.
    pub fn wait_for_room(&self, limit: usize) {
        let mut published = self.shared.lock();
        while self.backlog() > limit && !published.exited {
            published = match self
                .shared
                .progressed
                .wait_timeout(published, PUBLISH_EVERY)
            {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// The most recently published screen. Never waits: this is the frame's
    /// read.
    pub fn latest(&self) -> Arc<Snapshot> {
        self.shared.lock().snapshot.clone()
    }

    /// The screen with everything sent so far on it.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.caught_up().snapshot.clone()
    }

    /// Shell integration markers seen since the last call.
    pub fn drain_shell_events(&self) -> Vec<ShellEvent> {
        std::mem::take(&mut self.caught_up().shell_events)
    }

    /// Agent signal payloads seen since the last call, oldest first, once
    /// everything sent so far is parsed. Untrusted: the caller checks each
    /// against the pane's token before believing it.
    pub fn drain_agent_signals(&self) -> Vec<String> {
        std::mem::take(&mut self.caught_up().agent_signals)
    }

    /// As [`Terminal::drain_agent_signals`], without waiting: whatever the
    /// engine has published so far. What the UI uses.
    pub fn take_agent_signals(&self) -> Vec<String> {
        std::mem::take(&mut self.shared.lock().agent_signals)
    }

    /// Whether the remote has shell integration sourced. Until a marker has
    /// been seen, Berth cannot know command boundaries and the UI must say so.
    pub fn has_shell_integration(&self) -> bool {
        self.caught_up().seen_shell_marker
    }

    /// Change the grid's size. Never waits; the next published screen has it.
    pub fn resize(&self, size: TermSize, cell_size: CellSize) {
        let mut outbox = self.outbox();
        if outbox.size == size && outbox.cell_size == cell_size {
            return;
        }
        outbox.size = size;
        outbox.cell_size = cell_size;
        outbox.sent += 1;
        let seq = outbox.sent;
        if let Some(engine) = &outbox.engine {
            let _ = engine.send(Message {
                seq,
                command: Command::Resize(size, cell_size),
            });
        }
    }

    pub fn modes(&self) -> Modes {
        self.caught_up().snapshot.modes
    }

    pub fn alternate_screen(&self) -> bool {
        self.modes().alternate_screen
    }

    pub fn application_cursor(&self) -> bool {
        self.modes().application_cursor
    }

    pub fn mouse_reporting(&self) -> bool {
        self.modes().mouse_reporting
    }

    pub fn bracketed_paste(&self) -> bool {
        self.modes().bracketed_paste
    }

    /// How far back the view sits, in lines. Zero is the live grid.
    pub fn display_offset(&self) -> usize {
        self.caught_up().snapshot.display_offset
    }

    /// Move the view through the scrollback. Never waits.
    ///
    /// Does nothing on the alternate screen, which has no history: a
    /// full-screen program owns the whole grid and there is nothing behind it.
    pub fn scroll(&self, scroll: Scroll) {
        self.send(Command::Scroll(scroll));
    }

    /// What a wheel notch should do, given the modes the remote has set.
    pub fn wheel_action(&self, shift: bool) -> WheelAction {
        self.modes().wheel_action(shift)
    }

    /// The text inside a selection, as displayed.
    pub fn selection_text(&self, selection: Selection) -> String {
        self.snapshot().selection_text(selection)
    }

    /// The whole visible grid as text, for tests and for output indexing.
    pub fn visible_text(&self) -> String {
        self.snapshot().visible_text()
    }

    /// The live screen as escape sequences that redraw it.
    ///
    /// Feeding this to a fresh terminal of the same size reproduces the
    /// screen: characters, resolved colours, bold, italic, underline, strike
    /// and the cursor position. It is how a local session's last screen is
    /// kept across a quit. Always the live grid, never wherever the view was
    /// scrolled to, and never the scrollback.
    /// The active screen in Berth's wire format, with everything sent so far
    /// on it: colours as the program set them, for another copy to draw with
    /// its own palette.
    pub fn wire_screen(&self) -> Option<v1::Screen> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Command::WireScreen(tx));
        rx.recv_timeout(CATCH_UP_LIMIT).ok().flatten()
    }

    /// Up to `max` history lines older than line `before`, or the newest
    /// ones, newest first. Lines are numbered from the first the terminal
    /// ever produced, so ranges fetched at different times fit together. The
    /// alternate screen has no history, and gives none.
    pub fn wire_history(&self, before: Option<u64>, max: u32) -> WireHistory {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Command::WireHistory {
            before,
            max,
            reply: tx,
        });
        rx.recv_timeout(CATCH_UP_LIMIT).unwrap_or_default()
    }

    /// How many history lines the primary screen holds now.
    pub fn history_held(&self) -> usize {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Command::HistoryHeld(tx));
        rx.recv_timeout(CATCH_UP_LIMIT).unwrap_or(0)
    }

    pub fn screen_ansi(&self) -> String {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Command::LiveScreen(tx));
        rx.recv_timeout(CATCH_UP_LIMIT)
            .unwrap_or_else(|_| String::from("\x1b[0m\x1b[H\x1b[2J"))
    }
}

/// A screen of blanks, until the engine publishes its first.
fn blank(size: TermSize, cell_size: CellSize, palette: &Palette) -> Snapshot {
    let rows = (0..size.rows)
        .map(|line| {
            (0..size.cols)
                .map(|column| Cell {
                    c: ' ',
                    column,
                    line,
                    fg: palette.foreground,
                    bg: palette.background,
                    bold: false,
                    italic: false,
                    underline: false,
                    strikethrough: false,
                    wide_spacer: false,
                })
                .collect()
        })
        .collect();
    Snapshot {
        rows,
        cursor: Cursor {
            line: 0,
            column: 0,
            visible: true,
        },
        size,
        cell_size,
        modes: Modes::default(),
        display_offset: 0,
        seq: 0,
    }
}

/// Written as sequences that redraw it. Colours are written resolved, as
/// truecolour, so the picture does not depend on which palette reads it back.
/// Trailing blanks on the default background are left out, because a saved
/// screen is mostly padding.
fn ansi_of(snapshot: &Snapshot, palette: &Palette) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("\x1b[0m\x1b[H\x1b[2J");
    for (line, row) in snapshot.rows.iter().enumerate() {
        let mut cells: Vec<(char, Style)> = row.iter().map(|c| (c.c, Style::of(c))).collect();
        while cells.last().is_some_and(|(c, style)| {
            *c == ' ' && style.bg == palette.background && !style.marks_blank()
        }) {
            cells.pop();
        }
        if cells.is_empty() {
            continue;
        }
        let _ = write!(out, "\x1b[{};1H", line + 1);
        let mut current: Option<Style> = None;
        for (c, style) in cells {
            if current != Some(style) {
                style.write_sgr(&mut out);
                current = Some(style);
            }
            // A control character in a cell would be obeyed on the way back
            // in rather than drawn.
            out.push(if c.is_control() { ' ' } else { c });
        }
        out.push_str("\x1b[0m");
    }
    let _ = write!(
        out,
        "\x1b[{};{}H",
        snapshot.cursor.line + 1,
        snapshot.cursor.column + 1
    );
    out
}

fn rgb(c: RgbColor) -> Rgb {
    Rgb::new(c.r, c.g, c.b)
}

fn ghostty_rgb(c: Rgb) -> RgbColor {
    RgbColor {
        r: c.r,
        g: c.g,
        b: c.b,
    }
}

pub(crate) type Ghostty = libghostty_vt::Terminal<'static, 'static>;

pub(crate) fn modes_of(term: &Ghostty) -> Modes {
    Modes {
        alternate_screen: matches!(
            term.active_screen(),
            Ok(libghostty_vt::screen::Screen::Alternate)
        ),
        application_cursor: term.mode(Mode::DECCKM).unwrap_or(false),
        mouse_reporting: term.is_mouse_tracking().unwrap_or(false),
        bracketed_paste: term.mode(Mode::BRACKETED_PASTE).unwrap_or(false),
        alternate_scroll: term.mode(Mode::ALT_SCROLL).unwrap_or(false),
    }
}

fn display_offset(term: &Ghostty) -> usize {
    term.scrollbar()
        .map(|s| s.total.saturating_sub(s.offset + s.len) as usize)
        .unwrap_or(0)
}

/// The engine's own thread: one libghostty terminal, and nothing else ever
/// touches it.
struct Engine {
    term: Ghostty,
    screen: Screen,
    scanner: Scanner,
    palette: Palette,
    cell_size: CellSize,
    shared: Arc<Shared>,
    /// The last command handled.
    seq: u64,
    /// Something changed that the screen's own dirty tracking does not cover.
    rebuild: bool,
    /// The view moved, which is a repaint even where no cell changed.
    moved: bool,
    shell_events: Vec<ShellEvent>,
    agent_signals: Vec<String>,
    seen_shell_marker: bool,
    numbering: Numbering,
    /// The most bytes parsed between two numbering updates.
    slice: usize,
}

/// Numbers history lines so that a line keeps its number for as long as it
/// is held, and no number is ever used twice. libghostty's own history
/// coordinates start at whatever is oldest now, and shift every time old
/// lines are pruned.
///
/// A tracked reference sits on the bottom row of the live screen, and is put
/// back there after every slice of a write. How far it moved up in the full
/// screen is how many lines were pruned in between. libghostty prunes a whole
/// page at a time, and a page can reach past every row that existed before the
/// slice, the reference's included (found 2026-09-30). Then nothing that was
/// ever numbered is left, and the rows that remain are numbered on from past
/// the reference. The numbers can so fall behind the count of lines the
/// session produced, but never collide.
///
/// Physical lines: a resize that reflows renumbers what it moves, so history
/// fetched before a resize is fetched again after it.
#[derive(Default)]
struct Numbering {
    /// Lines pruned since the start, which is the number of the oldest line
    /// held.
    pruned: u64,
    /// The primary screen's history length at the last update, for numbering
    /// its first row while the alternate screen is up.
    held: u64,
    anchor: Option<(libghostty_vt::screen::TrackedGridRef, u32)>,
}

impl Numbering {
    /// Account for what changed since the last call. Only the primary screen
    /// has history; while the alternate one is up the primary does not move.
    fn update(&mut self, term: &Ghostty) {
        if modes_of(term).alternate_screen {
            return;
        }
        self.update_primary(term);
        self.held = term.scrollback_rows().unwrap_or(0) as u64;
    }

    /// The number the live screen's first row has: the line after the newest
    /// in history.
    fn first_line(&self, term: &Ghostty) -> u64 {
        if modes_of(term).alternate_screen {
            self.pruned + self.held
        } else {
            self.pruned + term.scrollback_rows().unwrap_or(0) as u64
        }
    }

    fn update_primary(&mut self, term: &Ghostty) {
        use libghostty_vt::terminal::{Point, PointCoordinate, PointSpace};
        if let Some((anchor, was)) = &self.anchor {
            match anchor.point(PointSpace::Screen) {
                Ok(Some(now)) => {
                    self.pruned += u64::from(was.saturating_sub(now.y));
                }
                // Pruned with its page. Every row older went too, so every
                // line ever numbered is gone: the survivors are rows this
                // slice made, never numbered, and numbering them from here
                // cannot collide with anything already sent.
                _ => self.pruned += u64::from(*was) + 1,
            }
        }
        let bottom = u32::from(term.rows().unwrap_or(1).saturating_sub(1));
        self.anchor = term
            .track_grid_ref(Point::Active(PointCoordinate { x: 0, y: bottom }))
            .ok()
            .and_then(|anchor| {
                let at = anchor.point(PointSpace::Screen).ok()??.y;
                Some((anchor, at))
            });
    }
}

impl Engine {
    fn new(
        size: TermSize,
        cell_size: CellSize,
        scrollback: Scrollback,
        shared: Arc<Shared>,
    ) -> Option<Self> {
        let mut term = libghostty_vt::Terminal::new(size.cols.max(1), size.rows.max(1)).ok()?;
        term.set_scrollback_max_bytes(scrollback.bytes).ok()?;
        term.set_scrollback_max_lines(scrollback.lines).ok()?;
        let mut engine = Self {
            term,
            screen: Screen::new()?,
            scanner: Scanner::new(),
            palette: Palette::default(),
            cell_size,
            shared,
            seq: 0,
            rebuild: true,
            moved: false,
            shell_events: Vec::new(),
            agent_signals: Vec::new(),
            seen_shell_marker: false,
            numbering: Numbering::default(),
            slice: scrollback.lines.unwrap_or(usize::MAX).clamp(256, 4096),
        };
        engine.numbering.update(&engine.term);
        engine.apply_palette(Palette::default());
        Some(engine)
    }

    fn run(mut self, commands: mpsc::Receiver<Message>) {
        self.publish();
        while let Ok(first) = commands.recv() {
            self.handle(first);
            // Everything already queued, up to the publishing interval, so a
            // flood is parsed in large steps and still shown as it goes.
            let started = Instant::now();
            let mut open = true;
            while started.elapsed() < PUBLISH_EVERY {
                match commands.try_recv() {
                    Ok(message) => self.handle(message),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        open = false;
                        break;
                    }
                }
            }
            self.publish();
            if !open {
                break;
            }
        }
    }

    fn handle(&mut self, message: Message) {
        self.seq = message.seq;
        match message.command {
            Command::Write(bytes) => {
                self.write(&bytes);
                self.shared
                    .backlog
                    .fetch_sub(bytes.len(), Ordering::Relaxed);
            }
            Command::Resize(size, cell_size) => {
                let (w, h) = (
                    cell_size.width.max(1.0) as u32,
                    cell_size.height.max(1.0) as u32,
                );
                let _ = self.term.resize(size.cols.max(1), size.rows.max(1), w, h);
                self.numbering.update(&self.term);
                self.cell_size = cell_size;
                self.rebuild = true;
            }
            Command::Scroll(scroll) => self.scroll(scroll),
            Command::Palette(palette) => self.apply_palette(palette),
            Command::LiveScreen(reply) => {
                let _ = reply.send(self.live_screen());
            }
            Command::WireScreen(reply) => {
                let first_line = self.numbering.first_line(&self.term);
                let _ = reply.send(crate::wire::screen(&self.term, first_line));
            }
            Command::HistoryHeld(reply) => {
                let held = if modes_of(&self.term).alternate_screen {
                    0
                } else {
                    self.term.scrollback_rows().unwrap_or(0)
                };
                let _ = reply.send(held);
            }
            Command::WireHistory { before, max, reply } => {
                let _ = reply.send(self.wire_history(before, max));
            }
        }
    }

    /// Feed bytes to the parser, taking out markers and agent signals.
    ///
    /// Bytes are fed in the same order and with nothing removed but those; the
    /// scanner only observes. Splitting at each marker is what lets an event
    /// record the cursor row at that exact point rather than at the end of the
    /// chunk.
    fn write(&mut self, bytes: &[u8]) {
        let mut scanner = std::mem::take(&mut self.scanner);
        for piece in scanner.scan(bytes) {
            match piece {
                Piece::Bytes(bytes) => {
                    for slice in bytes.chunks(self.slice) {
                        self.term.vt_write(slice);
                        self.numbering.update(&self.term);
                    }
                }
                Piece::Marker(kind) => {
                    self.seen_shell_marker = true;
                    self.shell_events.push(ShellEvent {
                        kind,
                        line: self.term.cursor_y().unwrap_or(0),
                    });
                }
                Piece::Agent(payload) => self.agent_signals.push(payload),
            }
        }
        self.scanner = scanner;
    }

    fn wire_history(&self, before: Option<u64>, max: u32) -> WireHistory {
        if modes_of(&self.term).alternate_screen {
            return WireHistory::default();
        }
        let held = self.term.scrollback_rows().unwrap_or(0) as u64;
        let oldest = self.numbering.pruned;
        // One past the newest line wanted, in libghostty's coordinates.
        let end = match before {
            Some(before) => before.saturating_sub(oldest).min(held),
            None => held,
        };
        let count = end.min(u64::from(max));
        if count == 0 {
            return WireHistory::default();
        }
        let first = end - count;
        WireHistory {
            newest: oldest + end - 1,
            lines: crate::wire::history(&self.term, first as u32, count as u32),
        }
    }

    fn scroll(&mut self, scroll: Scroll) {
        if modes_of(&self.term).alternate_screen {
            return;
        }
        let rows = self.term.rows().unwrap_or(1) as isize;
        let before = display_offset(&self.term);
        self.term.scroll_viewport(match scroll {
            // Berth's positive is back into history; libghostty's is forward.
            Scroll::Lines(lines) => ScrollViewport::Delta(-(lines as isize)),
            Scroll::PageUp => ScrollViewport::Delta(-rows),
            Scroll::PageDown => ScrollViewport::Delta(rows),
            Scroll::Bottom => ScrollViewport::Bottom,
        });
        // Only a move is a repaint. Wheeling against the top of the history
        // would otherwise burn a frame per notch.
        if display_offset(&self.term) != before {
            self.moved = true;
        }
    }

    fn apply_palette(&mut self, palette: Palette) {
        let table = palette.table().map(ghostty_rgb);
        let _ = self
            .term
            .set_default_color_palette(Some(GhosttyPalette(table)));
        let _ = self
            .term
            .set_default_fg_color(Some(ghostty_rgb(palette.foreground)));
        let _ = self
            .term
            .set_default_bg_color(Some(ghostty_rgb(palette.background)));
        if palette != self.palette {
            self.rebuild = true;
        }
        self.palette = palette;
    }

    fn live_screen(&mut self) -> String {
        let offset = display_offset(&self.term);
        if offset > 0 {
            self.term.scroll_viewport(ScrollViewport::Bottom);
        }
        let live = self
            .screen
            .build(&self.term, &self.palette, self.cell_size, true);
        if offset > 0 {
            self.term
                .scroll_viewport(ScrollViewport::Delta(-(offset as isize)));
        }
        // The cached rows are the live screen's now, not the view's.
        self.rebuild = true;
        match live {
            Some((snapshot, _)) => ansi_of(&snapshot, &self.palette),
            None => String::from("\x1b[0m\x1b[H\x1b[2J"),
        }
    }

    /// Make the current screen public and tell the UI, if anything changed.
    fn publish(&mut self) {
        let rebuild = std::mem::take(&mut self.rebuild);
        let moved = std::mem::take(&mut self.moved);
        let built = self
            .screen
            .build(&self.term, &self.palette, self.cell_size, rebuild);

        let mut published = self.shared.lock();
        let mut news = !self.shell_events.is_empty() || !self.agent_signals.is_empty();
        if let Some((mut snapshot, cells_changed)) = built {
            snapshot.seq = self.seq;
            let before = &published.snapshot;
            let changed = cells_changed
                || moved
                || snapshot.cursor != before.cursor
                || snapshot.modes != before.modes
                || snapshot.display_offset != before.display_offset
                || snapshot.size != before.size;
            if changed {
                published.revision = published.revision.wrapping_add(1);
                news = true;
            }
            published.snapshot = Arc::new(snapshot);
        }
        published.done = self.seq;
        published.shell_events.append(&mut self.shell_events);
        let signals = &mut published.agent_signals;
        signals.append(&mut self.agent_signals);
        // Bounded, so a pane nobody drains, or a program printing the sequence
        // in a loop, cannot grow this without limit.
        if signals.len() > MAX_QUEUED_SIGNALS {
            let excess = signals.len() - MAX_QUEUED_SIGNALS;
            signals.drain(..excess);
        }
        published.seen_shell_marker |= self.seen_shell_marker;
        drop(published);
        self.shared.progressed.notify_all();

        if news {
            let wake = self.shared.wake.lock().ok().and_then(|w| w.clone());
            if let Some(wake) = wake {
                wake();
            }
        }
    }
}

/// The screen as Berth's renderer takes it, kept between publishes so only
/// the rows libghostty marks dirty are built again.
struct Screen {
    state: RenderState<'static>,
    row_iter: RowIterator<'static>,
    cell_iter: CellIterator<'static>,
    rows: Vec<Arc<[Cell]>>,
    /// What the cached rows were built against. Any change rebuilds them all.
    shape: Option<Shape>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    cols: u16,
    rows: u16,
    alternate: bool,
    total: u64,
    offset: u64,
}

impl Screen {
    fn new() -> Option<Self> {
        Some(Self {
            state: RenderState::new().ok()?,
            row_iter: RowIterator::new().ok()?,
            cell_iter: CellIterator::new().ok()?,
            rows: Vec::new(),
            shape: None,
        })
    }

    /// The screen now, and whether any cell differs from the last build.
    fn build(
        &mut self,
        term: &Ghostty,
        palette: &Palette,
        cell_size: CellSize,
        force: bool,
    ) -> Option<(Snapshot, bool)> {
        let modes = modes_of(term);
        let scrollbar = term.scrollbar().ok();
        let cols = term.cols().ok()?;
        let rows = term.rows().ok()?;
        let shape = Shape {
            cols,
            rows,
            alternate: modes.alternate_screen,
            total: scrollbar.map(|s| s.total).unwrap_or(0),
            offset: scrollbar.map(|s| s.offset).unwrap_or(0),
        };
        let full = force || self.shape != Some(shape);
        self.shape = Some(shape);

        let snapshot = self.state.update(term).ok()?;
        let dirty = snapshot.dirty().ok()?;
        let full = full || dirty == Dirty::Full;
        let mut changed = false;
        if full || dirty != Dirty::Clean {
            let mut iteration = self.row_iter.update(&snapshot).ok()?;
            let mut line = 0usize;
            while let Some(row) = iteration.next() {
                if full || line >= self.rows.len() || row.dirty().unwrap_or(true) {
                    let mut cells = self.cell_iter.update(row).ok()?;
                    let built = build_row(&mut cells, line as u16, palette);
                    if line < self.rows.len() {
                        if *self.rows[line] != *built {
                            self.rows[line] = built;
                            changed = true;
                        }
                    } else {
                        self.rows.push(built);
                        changed = true;
                    }
                    let _ = row.set_dirty(false);
                }
                line += 1;
            }
            if self.rows.len() != line {
                self.rows.truncate(line);
                changed = true;
            }
            let _ = snapshot.set_dirty(Dirty::Clean);
        }

        let cursor = snapshot.cursor_viewport().ok().flatten();
        let visible = snapshot.cursor_visible().unwrap_or(true);
        Some((
            Snapshot {
                rows: self.rows.clone(),
                cursor: Cursor {
                    line: cursor.map(|c| c.y).unwrap_or(0),
                    column: cursor.map(|c| c.x).unwrap_or(0),
                    visible: visible && cursor.is_some(),
                },
                size: TermSize { cols, rows },
                cell_size,
                modes,
                display_offset: display_offset(term),
                seq: 0,
            },
            changed,
        ))
    }
}

/// One row's cells with concrete colours. The second half of a wide glyph is
/// left out; the glyph covers it.
fn build_row(cells: &mut CellIteration<'_, '_>, line: u16, palette: &Palette) -> Arc<[Cell]> {
    let mut out = Vec::new();
    let mut column = 0u16;
    let mut graphemes = ['\0'; 16];
    while let Some(cell) = cells.next() {
        let wide = cell.raw_cell().ok().and_then(|c| c.wide().ok());
        if !matches!(wide, Some(CellWide::SpacerTail | CellWide::SpacerHead)) {
            let c = match cell.graphemes_len() {
                Ok(0) | Err(_) => ' ',
                Ok(len) if len <= graphemes.len() => cell
                    .graphemes_buf(&mut graphemes)
                    .map(|_| graphemes[0])
                    .unwrap_or(' '),
                Ok(_) => cell
                    .graphemes()
                    .ok()
                    .and_then(|g| g.first().copied())
                    .unwrap_or(' '),
            };
            let style = cell.style().ok();
            let mut fg = cell
                .fg_color()
                .ok()
                .flatten()
                .map(rgb)
                .unwrap_or(palette.foreground);
            let mut bg = cell
                .bg_color()
                .ok()
                .flatten()
                .map(rgb)
                .unwrap_or(palette.background);
            if style.as_ref().is_some_and(|s| s.inverse) {
                std::mem::swap(&mut fg, &mut bg);
            }
            out.push(Cell {
                c,
                column,
                line,
                fg,
                bg,
                bold: style.as_ref().is_some_and(|s| s.bold),
                italic: style.as_ref().is_some_and(|s| s.italic),
                underline: style
                    .as_ref()
                    .is_some_and(|s| !matches!(s.underline, Underline::None)),
                strikethrough: style.as_ref().is_some_and(|s| s.strikethrough),
                wide_spacer: false,
            });
        }
        column += 1;
    }
    out.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Build the same terminal's screen incrementally and from nothing, and
    /// compare. What makes copying only the changed rows safe to rely on.
    fn incremental_matches_full(chunks: &[Vec<u8>], scrollback: usize) {
        let palette = Palette::default();
        let cell = CellSize::new(8.0, 16.0);
        let mut term = libghostty_vt::Terminal::new(12, 4).expect("terminal");
        term.set_scrollback_max_bytes(None).expect("bytes");
        term.set_scrollback_max_lines(Some(scrollback))
            .expect("lines");
        let mut kept = Screen::new().expect("screen");
        for chunk in chunks {
            term.vt_write(chunk);
            let (incremental, _) = kept.build(&term, &palette, cell, false).expect("built");
            let (full, _) = Screen::new()
                .expect("screen")
                .build(&term, &palette, cell, true)
                .expect("built");
            assert_eq!(
                incremental,
                full,
                "after {:?}",
                String::from_utf8_lossy(chunk)
            );
        }
    }

    fn piece() -> impl Strategy<Value = Vec<u8>> {
        prop_oneof![
            "[a-z ]{0,20}".prop_map(String::into_bytes),
            Just(b"\r\n".to_vec()),
            Just(b"\x1b[2J".to_vec()),
            Just(b"\x1b[H".to_vec()),
            Just(b"\x1b[31m".to_vec()),
            Just(b"\x1b[0m".to_vec()),
            Just(b"\x1b[7m".to_vec()),
            Just(b"\x1b[K".to_vec()),
            Just(b"\x1b[2;3H".to_vec()),
            Just(b"\x1b[?1049h".to_vec()),
            Just(b"\x1b[?1049l".to_vec()),
            Just(b"\x1b[1S".to_vec()),
            Just(b"\x1b[1T".to_vec()),
            Just(b"\x1b[2;3r".to_vec()),
            Just(b"\x1b[r".to_vec()),
            Just("\u{65e5}".as_bytes().to_vec()),
        ]
    }

    proptest! {
        #[test]
        fn only_the_changed_rows_are_built_and_nothing_is_missed(
            chunks in prop::collection::vec(prop::collection::vec(piece(), 1..6).prop_map(|p| p.concat()), 1..40),
            scrollback in prop_oneof![Just(0usize), Just(3), Just(100)],
        ) {
            incremental_matches_full(&chunks, scrollback);
        }
    }

    /// A full history keeps its size while the screen scrolls, so nothing but
    /// libghostty's own tracking says the rows moved.
    #[test]
    fn scrolling_with_a_full_history_is_seen() {
        let lines: Vec<Vec<u8>> = (0..40)
            .map(|n| format!("row {n}\r\n").into_bytes())
            .collect();
        incremental_matches_full(&lines, 5);
    }

    #[test]
    fn a_frame_never_waits_for_the_parser() {
        let term = Terminal::new(TermSize::new(80, 24), CellSize::new(8.0, 16.0));
        let flood = "0123456789abcdef\r\n".repeat(64 * 1024).into_bytes();
        let started = Instant::now();
        term.advance(&flood);
        let _ = term.latest();
        assert!(started.elapsed() < Duration::from_millis(50));
        let text = term.visible_text();
        assert!(text.contains("0123456789abcdef"), "{text:?}");
    }

    #[test]
    fn a_publish_wakes_the_ui() {
        let term = Terminal::new(TermSize::new(20, 5), CellSize::new(8.0, 16.0));
        let (tx, rx) = mpsc::channel();
        term.on_publish(move || {
            let _ = tx.send(());
        });
        term.advance(b"x");
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn a_published_screen_carries_the_writes_it_includes() {
        let term = Terminal::new(TermSize::new(20, 5), CellSize::new(8.0, 16.0));
        let first = term.advance(b"a");
        let second = term.advance(b"b");
        assert!(second > first);
        assert!(term.snapshot().seq >= second);
    }

    /// Rows nothing touched are the same allocation as before, not a copy.
    #[test]
    fn unchanged_rows_are_shared_between_screens() {
        let term = Terminal::new(TermSize::new(20, 5), CellSize::new(8.0, 16.0));
        term.advance(b"top\r\n");
        let before = term.snapshot();
        term.advance(b"second");
        let after = term.snapshot();
        assert!(Arc::ptr_eq(&before.rows[0], &after.rows[0]));
        assert!(!Arc::ptr_eq(&before.rows[1], &after.rows[1]));
    }

    #[test]
    fn the_backlog_drains() {
        let term = Terminal::new(TermSize::new(20, 5), CellSize::new(8.0, 16.0));
        term.advance(&vec![b'x'; 1 << 20]);
        term.wait_for_room(0);
        assert_eq!(term.backlog(), 0);
    }
}
