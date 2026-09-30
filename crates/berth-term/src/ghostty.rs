//! libghostty-vt behind `berth-term`'s interface, for the D49 engine spike.
//!
//! libghostty-vt's terminal can neither move between threads nor be shared, so
//! each one lives on a thread of its own and everything reaches it over a
//! channel (D49, "pin each terminal to its own thread"). Writes are
//! synchronous: they return once the bytes are parsed, so the modes a caller
//! reads next are the ones those bytes set. The renderer asks for a fresh copy
//! of the screen each frame, built on the engine's thread from its render
//! state, and never touches the engine itself.
//!
//! OSC 133 markers and Berth's agent signals are still taken out of the stream
//! here, by the same scanner the alacritty engine uses, before the bytes are
//! handed over.

use std::sync::mpsc;

use alacritty_terminal::event::Event;
use libghostty_vt::render::{CellIterator, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::{Palette as GhosttyPalette, RgbColor};
use libghostty_vt::terminal::{Mode, ScrollViewport};
use libghostty_vt::RenderState;

use crate::shell::{Piece, Scanner, ShellEvent};
use crate::size::{CellSize, TermSize};
use crate::terminal::{
    Cell, Cursor, MAX_QUEUED_SIGNALS, Palette, Rgb, SCROLLBACK_LINES, Scroll, Selection, Snapshot, Style, WheelAction,
};


/// The terminal modes a caller asks about between frames, copied out after
/// every write so answering them never waits on the engine thread.
#[derive(Debug, Clone, Copy, Default)]
struct Modes {
    alternate_screen: bool,
    application_cursor: bool,
    mouse_reporting: bool,
    bracketed_paste: bool,
    alternate_scroll: bool,
    display_offset: usize,
    cursor_line: u16,
}

enum Command {
    Write(Vec<u8>, mpsc::SyncSender<Modes>),
    Resize(u16, u16, u32, u32, mpsc::SyncSender<Modes>),
    Scroll(ScrollViewport, mpsc::SyncSender<Modes>),
    Palette(Palette),
    Snapshot(TermSize, CellSize, mpsc::SyncSender<Snapshot>),
    /// The live screen, wherever the view is scrolled to.
    LiveSnapshot(TermSize, CellSize, mpsc::SyncSender<Snapshot>),
}

pub struct Terminal {
    engine: Option<mpsc::Sender<Command>>,
    size: TermSize,
    cell_size: CellSize,
    palette: Palette,
    modes: Modes,
    revision: u64,
    scanner: Scanner,
    shell_events: Vec<ShellEvent>,
    seen_shell_marker: bool,
    agent_signals: Vec<String>,
}

impl Terminal {
    pub fn new(size: TermSize, cell_size: CellSize) -> Self {
        let palette = Palette::default();
        let (tx, rx) = mpsc::channel();
        let (cols, rows) = (size.cols, size.rows);
        let spawned = std::thread::Builder::new()
            .name("berth-ghostty".into())
            .spawn(move || engine(rx, cols, rows));
        let engine = match spawned {
            Ok(_) => Some(tx),
            Err(error) => {
                tracing::warn!(%error, "could not start the terminal engine thread");
                None
            }
        };
        let term = Self {
            engine,
            size,
            cell_size,
            palette: palette.clone(),
            modes: Modes::default(),
            revision: 0,
            scanner: Scanner::new(),
            shell_events: Vec::new(),
            seen_shell_marker: false,
            agent_signals: Vec::new(),
        };
        term.send(Command::Palette(palette));
        term
    }

    fn send(&self, command: Command) {
        if let Some(engine) = &self.engine {
            let _ = engine.send(command);
        }
    }

    /// Send a command that answers with the modes after it, and keep them.
    fn round_trip(&mut self, make: impl FnOnce(mpsc::SyncSender<Modes>) -> Command) {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(make(tx));
        if let Ok(modes) = rx.recv() {
            self.modes = modes;
        }
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette.clone();
        self.send(Command::Palette(palette));
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn size(&self) -> TermSize {
        self.size
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut scanner = std::mem::take(&mut self.scanner);
        for piece in scanner.scan(bytes) {
            match piece {
                Piece::Bytes(bytes) => {
                    let owned = bytes.to_vec();
                    self.round_trip(|tx| Command::Write(owned, tx));
                }
                Piece::Marker(kind) => {
                    self.seen_shell_marker = true;
                    self.shell_events.push(ShellEvent {
                        kind,
                        line: self.modes.cursor_line,
                    });
                }
                Piece::Agent(payload) => {
                    if self.agent_signals.len() >= MAX_QUEUED_SIGNALS {
                        self.agent_signals.remove(0);
                    }
                    self.agent_signals.push(payload);
                }
            }
        }
        self.scanner = scanner;
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn drain_shell_events(&mut self) -> Vec<ShellEvent> {
        std::mem::take(&mut self.shell_events)
    }

    pub fn drain_agent_signals(&mut self) -> Vec<String> {
        std::mem::take(&mut self.agent_signals)
    }

    pub fn has_shell_integration(&self) -> bool {
        self.seen_shell_marker
    }

    /// The alacritty engine's event queue. Nothing reads it in the app, and
    /// libghostty reports its effects through callbacks instead.
    pub fn drain_events(&mut self) -> Vec<Event> {
        Vec::new()
    }

    pub fn resize(&mut self, size: TermSize, cell_size: CellSize) {
        if self.size == size && self.cell_size == cell_size {
            return;
        }
        self.size = size;
        self.cell_size = cell_size;
        let (w, h) = (cell_size.width.max(1.0) as u32, cell_size.height.max(1.0) as u32);
        self.round_trip(|tx| Command::Resize(size.cols, size.rows, w, h, tx));
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn alternate_screen(&self) -> bool {
        self.modes.alternate_screen
    }

    pub fn application_cursor(&self) -> bool {
        self.modes.application_cursor
    }

    pub fn mouse_reporting(&self) -> bool {
        self.modes.mouse_reporting
    }

    pub fn bracketed_paste(&self) -> bool {
        self.modes.bracketed_paste
    }

    pub fn display_offset(&self) -> usize {
        self.modes.display_offset
    }

    pub fn scroll(&mut self, scroll: Scroll) {
        if self.alternate_screen() {
            return;
        }
        let before = self.display_offset();
        let viewport = match scroll {
            // Berth's positive is back into history; libghostty's is forward.
            Scroll::Lines(lines) => ScrollViewport::Delta(-(lines as isize)),
            Scroll::PageUp => ScrollViewport::Delta(-(self.size.rows as isize)),
            Scroll::PageDown => ScrollViewport::Delta(self.size.rows as isize),
            Scroll::Bottom => ScrollViewport::Bottom,
        };
        self.round_trip(|tx| Command::Scroll(viewport, tx));
        if self.display_offset() != before {
            self.revision = self.revision.wrapping_add(1);
        }
    }

    pub fn wheel_action(&self, shift: bool) -> WheelAction {
        if shift {
            return WheelAction::View;
        }
        if self.mouse_reporting() {
            return WheelAction::Report;
        }
        if self.modes.alternate_screen && self.modes.alternate_scroll {
            return WheelAction::Arrows;
        }
        WheelAction::View
    }

    /// A copy of the screen as it is now, built on the engine thread.
    pub fn snapshot(&self) -> Snapshot {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Command::Snapshot(self.size, self.cell_size, tx));
        rx.recv().unwrap_or_else(|_| blank(self.size, self.cell_size))
    }

    pub fn selection_text(&self, selection: Selection) -> String {
        let selection = selection.normalised();
        let mut out = String::new();
        let mut row = String::new();
        let mut current_line: Option<u16> = None;
        for cell in self.snapshot().cells {
            if !selection.contains(cell.line, cell.column) {
                continue;
            }
            match current_line {
                Some(line) if line == cell.line => {}
                Some(_) => {
                    out.push_str(row.trim_end());
                    out.push('\n');
                    row.clear();
                }
                None => {}
            }
            current_line = Some(cell.line);
            row.push(cell.c);
        }
        if current_line.is_some() {
            out.push_str(row.trim_end());
        }
        out
    }

    pub fn visible_text(&self) -> String {
        let mut out = String::new();
        let mut last = 0u16;
        for cell in self.snapshot().cells {
            if cell.line != last {
                out.push('\n');
                last = cell.line;
            }
            out.push(cell.c);
        }
        out
    }

    /// The live screen as sequences that redraw it, from the snapshot. The
    /// spike's stand-in for D46's saved screen; the server will use
    /// libghostty's own snapshot format instead.
    pub fn screen_ansi(&self) -> String {
        use std::fmt::Write as _;
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Command::LiveSnapshot(self.size, self.cell_size, tx));
        let snapshot = rx
            .recv()
            .unwrap_or_else(|_| blank(self.size, self.cell_size));
        let mut out = String::from("\x1b[0m\x1b[H\x1b[2J");
        let mut rows: Vec<Vec<(char, Style)>> = vec![Vec::new(); snapshot.size.rows as usize];
        for cell in &snapshot.cells {
            if let Some(row) = rows.get_mut(cell.line as usize) {
                row.push((
                    cell.c,
                    Style {
                        fg: cell.fg,
                        bg: cell.bg,
                        bold: cell.bold,
                        italic: cell.italic,
                        underline: cell.underline,
                        strikethrough: cell.strikethrough,
                    },
                ));
            }
        }
        for (line, mut cells) in rows.into_iter().enumerate() {
            while cells.last().is_some_and(|(c, style)| {
                *c == ' ' && style.bg == self.palette.background && !style.marks_blank()
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
}

fn blank(size: TermSize, cell_size: CellSize) -> Snapshot {
    Snapshot {
        cells: Vec::new(),
        cursor: Cursor {
            line: 0,
            column: 0,
            visible: false,
        },
        size,
        cell_size,
        alternate_screen: false,
        application_cursor: false,
        display_offset: 0,
    }
}

fn rgb(c: RgbColor) -> Rgb {
    Rgb::new(c.r, c.g, c.b)
}

/// The engine's own thread: one libghostty terminal, and nothing else ever
/// touches it.
fn engine(commands: mpsc::Receiver<Command>, cols: u16, rows: u16) {
    let Ok(mut term) = libghostty_vt::Terminal::new(cols.max(1), rows.max(1)) else {
        tracing::warn!("libghostty could not create a terminal");
        return;
    };
    // Both limits apply and the first reached wins; lines is the one Berth
    // means, so the byte limit goes (found in the spike).
    let _ = term.set_scrollback_max_bytes(None);
    let _ = term.set_scrollback_max_lines(Some(SCROLLBACK_LINES));
    let (Ok(mut state), Ok(mut row_iter), Ok(mut cell_iter)) =
        (RenderState::new(), RowIterator::new(), CellIterator::new())
    else {
        tracing::warn!("libghostty could not create a render state");
        return;
    };
    let mut palette = Palette::default();

    let modes = |term: &libghostty_vt::Terminal<'static, 'static>| {
        let scrollbar = term.scrollbar().ok();
        Modes {
            alternate_screen: matches!(
                term.active_screen(),
                Ok(libghostty_vt::screen::Screen::Alternate)
            ),
            application_cursor: term.mode(Mode::DECCKM).unwrap_or(false),
            mouse_reporting: term.is_mouse_tracking().unwrap_or(false),
            bracketed_paste: term.mode(Mode::BRACKETED_PASTE).unwrap_or(false),
            alternate_scroll: term.mode(Mode::ALT_SCROLL).unwrap_or(false),
            display_offset: scrollbar
                .map(|s| s.total.saturating_sub(s.offset + s.len) as usize)
                .unwrap_or(0),
            cursor_line: term.cursor_y().unwrap_or(0),
        }
    };

    while let Ok(command) = commands.recv() {
        match command {
            Command::Write(bytes, reply) => {
                term.vt_write(&bytes);
                let _ = reply.send(modes(&term));
            }
            Command::Resize(cols, rows, w, h, reply) => {
                let _ = term.resize(cols.max(1), rows.max(1), w, h);
                let _ = reply.send(modes(&term));
            }
            Command::Scroll(viewport, reply) => {
                term.scroll_viewport(viewport);
                let _ = reply.send(modes(&term));
            }
            Command::Palette(new) => {
                let table = new.table().map(|c| RgbColor {
                    r: c.r,
                    g: c.g,
                    b: c.b,
                });
                let _ = term.set_default_color_palette(Some(GhosttyPalette(table)));
                let fg = new.foreground;
                let bg = new.background;
                let _ = term.set_default_fg_color(Some(RgbColor { r: fg.r, g: fg.g, b: fg.b }));
                let _ = term.set_default_bg_color(Some(RgbColor { r: bg.r, g: bg.g, b: bg.b }));
                palette = new;
            }
            Command::LiveSnapshot(size, cell_size, reply) => {
                let offset = modes(&term).display_offset;
                term.scroll_viewport(ScrollViewport::Bottom);
                let snapshot = build(
                    &term, &mut state, &mut row_iter, &mut cell_iter, &palette, size, cell_size,
                )
                .unwrap_or_else(|| blank(size, cell_size));
                if offset > 0 {
                    term.scroll_viewport(ScrollViewport::Delta(-(offset as isize)));
                }
                let _ = reply.send(snapshot);
            }
            Command::Snapshot(size, cell_size, reply) => {
                let snapshot = build(
                    &term, &mut state, &mut row_iter, &mut cell_iter, &palette, size, cell_size,
                )
                .unwrap_or_else(|| blank(size, cell_size));
                let _ = reply.send(snapshot);
            }
        }
    }
}

/// The screen as Berth's renderer takes it: owned cells with concrete colours.
fn build(
    term: &libghostty_vt::Terminal<'static, 'static>,
    state: &mut RenderState<'static>,
    row_iter: &mut RowIterator<'static>,
    cell_iter: &mut CellIterator<'static>,
    palette: &Palette,
    size: TermSize,
    cell_size: CellSize,
) -> Option<Snapshot> {
    let snapshot = state.update(term).ok()?;
    let cursor = snapshot.cursor_viewport().ok().flatten();
    let visible = snapshot.cursor_visible().unwrap_or(true);
    let mut cells = Vec::with_capacity(size.cols as usize * size.rows as usize);
    let mut rows = row_iter.update(&snapshot).ok()?;
    let mut line = 0u16;
    while let Some(row) = rows.next() {
        let mut row_cells = cell_iter.update(row).ok()?;
        let mut column = 0u16;
        while let Some(cell) = row_cells.next() {
            let wide = cell.raw_cell().ok().and_then(|c| c.wide().ok());
            if !matches!(wide, Some(CellWide::SpacerTail | CellWide::SpacerHead)) {
                let style = cell.style().ok();
                let mut fg = cell.fg_color().ok().flatten().map(rgb).unwrap_or(palette.foreground);
                let mut bg = cell.bg_color().ok().flatten().map(rgb).unwrap_or(palette.background);
                if style.is_some_and(|s| s.inverse) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let c = cell
                    .graphemes()
                    .ok()
                    .and_then(|g| g.first().copied())
                    .unwrap_or(' ');
                cells.push(Cell {
                    c,
                    column,
                    line,
                    fg,
                    bg,
                    bold: style.is_some_and(|s| s.bold),
                    italic: style.is_some_and(|s| s.italic),
                    underline: style.is_some_and(|s| !matches!(s.underline, libghostty_vt::style::Underline::None)),
                    strikethrough: style.is_some_and(|s| s.strikethrough),
                    wide_spacer: false,
                });
            }
            column += 1;
        }
        line += 1;
    }
    let modes_alt = matches!(term.active_screen(), Ok(libghostty_vt::screen::Screen::Alternate));
    let scrollbar = term.scrollbar().ok();
    Some(Snapshot {
        cells,
        cursor: Cursor {
            line: cursor.map(|c| c.y).unwrap_or(0),
            column: cursor.map(|c| c.x).unwrap_or(0),
            visible: visible && cursor.is_some(),
        },
        size,
        cell_size,
        alternate_screen: modes_alt,
        application_cursor: term.mode(Mode::DECCKM).unwrap_or(false),
        display_offset: scrollbar
            .map(|s| s.total.saturating_sub(s.offset + s.len) as usize)
            .unwrap_or(0),
    })
}
