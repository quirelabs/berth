//! The terminal model: an alacritty grid plus the parser that feeds it.
//!
//! Deliberately free of gpui. The renderer takes a [`Snapshot`], which is a
//! plain owned value, so painting never holds a lock on the grid and never
//! reaches back into the parser. This is the shape Zed uses and the reason a
//! firehose of output cannot stall the UI thread.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Processor};
use std::sync::{Arc, Mutex};

use crate::shell::{Scanner, ShellEvent};
use crate::size::{CellSize, TermSize};

/// Local scrollback. tmux keeps far more on the remote.
pub const SCROLLBACK_LINES: usize = 10_000;

/// A movement through the scrollback.
///
/// Berth's own enum rather than alacritty's, because nothing above this crate
/// depends on alacritty and this is the type the app has to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    /// Positive goes back into the history, negative returns towards the live
    /// grid. Matches a wheel, where pushing up shows what came before.
    Lines(i32),
    PageUp,
    PageDown,
    /// Back to the live grid, wherever the view was.
    Bottom,
}

/// What a wheel notch means here, which is not ours to decide alone: the
/// remote sets modes that claim it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelAction {
    /// The program asked for mouse events, so the notch is reported to it and
    /// moves nothing locally.
    Report,
    /// The alternate screen with alternate scroll set: a full-screen program
    /// with no scrollback of its own that expects arrow keys. This is what
    /// makes the wheel work in `less` and `man`.
    Arrows,
    /// Berth moves its own view through its own scrollback.
    View,
}

/// Collects the events alacritty raises while parsing.
#[derive(Clone, Default)]
pub struct Listener {
    events: Arc<Mutex<Vec<Event>>>,
}

impl Listener {
    pub fn drain(&self) -> Vec<Event> {
        match self.events.lock() {
            Ok(mut events) => std::mem::take(&mut *events),
            Err(_) => Vec::new(),
        }
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }
}

/// One rendered cell, resolved to concrete colours.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    pub c: char,
    pub column: u16,
    pub line: u16,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    /// True for the second half of a double-width glyph, which must not be
    /// painted; the wide char already covers this cell.
    pub wide_spacer: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub line: u16,
    pub column: u16,
    pub visible: bool,
}

/// Everything the renderer needs for one frame, owned outright.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub cells: Vec<Cell>,
    pub cursor: Cursor,
    pub size: TermSize,
    pub cell_size: CellSize,
    /// True while a full-screen application owns the screen. Predictive echo
    /// must never run in this mode.
    pub alternate_screen: bool,
    /// Set by the remote via DECCKM; changes what arrow keys encode to.
    pub application_cursor: bool,
    pub display_offset: usize,
}

impl Snapshot {
    pub fn is_scrolled_back(&self) -> bool {
        self.display_offset > 0
    }
}

/// A rectangular-by-rows selection: from one cell to another, following the
/// text as it reads rather than as a box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub start_line: u16,
    pub start_column: u16,
    pub end_line: u16,
    pub end_column: u16,
}

impl Selection {
    pub fn new(start_line: u16, start_column: u16, end_line: u16, end_column: u16) -> Self {
        Self {
            start_line,
            start_column,
            end_line,
            end_column,
        }
    }

    /// Put the anchor before the head, so dragging upwards or leftwards
    /// selects the same text as dragging the other way.
    pub fn normalised(self) -> Self {
        let before = (self.start_line, self.start_column) <= (self.end_line, self.end_column);
        if before {
            self
        } else {
            Self {
                start_line: self.end_line,
                start_column: self.end_column,
                end_line: self.start_line,
                end_column: self.start_column,
            }
        }
    }

    pub fn is_empty(self) -> bool {
        self.start_line == self.end_line && self.start_column == self.end_column
    }

    fn contains(self, line: u16, column: u16) -> bool {
        if line < self.start_line || line > self.end_line {
            return false;
        }
        if line == self.start_line && column < self.start_column {
            return false;
        }
        if line == self.end_line && column > self.end_column {
            return false;
        }
        true
    }
}

pub struct Terminal {
    term: Term<Listener>,
    parser: Processor,
    listener: Listener,
    size: TermSize,
    cell_size: CellSize,
    palette: Palette,
    /// Bumped whenever output changes the grid, so the renderer can skip work.
    revision: u64,
    scanner: Scanner,
    shell_events: Vec<ShellEvent>,
    seen_shell_marker: bool,
}

impl Terminal {
    pub fn new(size: TermSize, cell_size: CellSize) -> Self {
        let listener = Listener::default();
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Default::default()
        };
        let term = Term::new(config, &size, listener.clone());
        Self {
            term,
            parser: Processor::new(),
            listener,
            size,
            cell_size,
            palette: Palette::default(),
            revision: 0,
            scanner: Scanner::new(),
            shell_events: Vec::new(),
            seen_shell_marker: false,
        }
    }

    /// Replace the colour palette. Owned by the app so every visual constant
    /// lives in one struct there; this crate only needs somewhere to resolve
    /// indexed and named colours against.
    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn size(&self) -> TermSize {
        self.size
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Feed raw PTY bytes to the parser.
    ///
    /// OSC 133 markers are scanned out as the stream goes past. Bytes are fed
    /// to alacritty in the same order and with nothing removed; the scanner
    /// only observes. Splitting at each marker is what lets an event record the
    /// cursor row at that exact point rather than at the end of the chunk.
    pub fn advance(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        for piece in self.scanner.scan(bytes) {
            match piece {
                crate::shell::Piece::Bytes(bytes) => self.parser.advance(&mut self.term, bytes),
                crate::shell::Piece::Marker(kind) => {
                    let line = self.term.grid().cursor.point.line.0.max(0) as u16;
                    self.seen_shell_marker = true;
                    self.shell_events.push(ShellEvent { kind, line });
                }
            }
        }
        self.revision = self.revision.wrapping_add(1);
    }

    /// Shell integration markers seen since the last call.
    ///
    /// Nothing consumes these yet. M1 uses them for command blocks and a styled
    /// prompt row; M3's timeline uses `CommandEnd` for exit codes.
    pub fn drain_shell_events(&mut self) -> Vec<ShellEvent> {
        std::mem::take(&mut self.shell_events)
    }

    /// Whether the remote has shell integration sourced. Until a marker has
    /// been seen, Berth cannot know command boundaries and the UI must say so.
    pub fn has_shell_integration(&self) -> bool {
        self.seen_shell_marker
    }

    /// Events alacritty raised since the last call, such as title changes.
    pub fn drain_events(&mut self) -> Vec<Event> {
        self.listener.drain()
    }

    pub fn resize(&mut self, size: TermSize, cell_size: CellSize) {
        if self.size == size && self.cell_size == cell_size {
            return;
        }
        self.size = size;
        self.cell_size = cell_size;
        self.term.resize(size);
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn alternate_screen(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    pub fn application_cursor(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// True while the remote wants mouse events. Clicks must then be forwarded
    /// rather than used to start a local selection.
    pub fn mouse_reporting(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.mode().intersects(TermMode::MOUSE_MODE)
    }

    /// True when the remote's line editor has asked for bracketed paste.
    pub fn bracketed_paste(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// How far back the view sits, in lines. Zero is the live grid.
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Move the view through the scrollback.
    ///
    /// Does nothing on the alternate screen, which has no history: a
    /// full-screen program owns the whole grid and there is nothing behind it.
    /// [`Terminal::wheel_action`] is what decides that a notch should have gone
    /// to the program instead.
    pub fn scroll(&mut self, scroll: Scroll) {
        use alacritty_terminal::grid::Scroll as GridScroll;

        if self.alternate_screen() {
            return;
        }
        let before = self.display_offset();
        self.term.scroll_display(match scroll {
            Scroll::Lines(lines) => GridScroll::Delta(lines),
            Scroll::PageUp => GridScroll::PageUp,
            Scroll::PageDown => GridScroll::PageDown,
            Scroll::Bottom => GridScroll::Bottom,
        });
        // Only a move is a repaint. Wheeling against the top of the history
        // would otherwise burn a frame per notch.
        if self.display_offset() != before {
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// What a wheel notch should do, given the modes the remote has set.
    ///
    /// Shift takes the wheel back from the program. Every terminal does this,
    /// and without it a mouse-reporting program leaves no way to look at
    /// anything above the screen.
    pub fn wheel_action(&self, shift: bool) -> WheelAction {
        use alacritty_terminal::term::TermMode;

        if shift {
            return WheelAction::View;
        }
        if self.mouse_reporting() {
            return WheelAction::Report;
        }
        if self
            .term
            .mode()
            .contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
        {
            return WheelAction::Arrows;
        }
        WheelAction::View
    }

    /// The text inside a selection, as displayed.
    ///
    /// Trailing blanks on each row are dropped, because a grid row is padded
    /// out to its full width and copying that padding gives a paste full of
    /// spaces.
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

    /// Build the frame's render data. The only place the grid is read.
    pub fn snapshot(&self) -> Snapshot {
        use alacritty_terminal::term::TermMode;

        let content = self.term.renderable_content();
        let mut cells = Vec::with_capacity(self.size.columns() * self.size.screen_lines());

        for indexed in content.display_iter {
            let cell = indexed.cell;
            let flags = cell.flags;
            // Skip the padding cell after a wide glyph, and cells that would
            // paint nothing at all.
            if flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let line = indexed.point.line.0;
            if line < 0 {
                continue;
            }

            let mut fg = self.palette.resolve(cell.fg, flags.contains(Flags::BOLD));
            let mut bg = self.palette.resolve(cell.bg, false);
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }

            cells.push(Cell {
                c: cell.c,
                column: indexed.point.column.0 as u16,
                line: line as u16,
                fg,
                bg,
                bold: flags.contains(Flags::BOLD),
                italic: flags.contains(Flags::ITALIC),
                underline: flags.intersects(Flags::ALL_UNDERLINES),
                strikethrough: flags.contains(Flags::STRIKEOUT),
                wide_spacer: false,
            });
        }

        let cursor_point: Point = content.cursor.point;
        let mode = self.term.mode();

        Snapshot {
            cells,
            cursor: Cursor {
                line: cursor_point.line.0.max(0) as u16,
                column: cursor_point.column.0 as u16,
                visible: mode.contains(TermMode::SHOW_CURSOR),
            },
            size: self.size,
            cell_size: self.cell_size,
            alternate_screen: mode.contains(TermMode::ALT_SCREEN),
            application_cursor: mode.contains(TermMode::APP_CURSOR),
            display_offset: content.display_offset,
        }
    }

    /// The live screen as escape sequences that redraw it.
    ///
    /// Feeding this to a fresh terminal of the same size reproduces the
    /// screen: characters, resolved colours, bold, italic, underline, strike
    /// and the cursor position. It is how a local session's last screen is
    /// kept across a quit. Always the live grid, never wherever the view was
    /// scrolled to, and never the scrollback.
    ///
    /// Colours are written resolved, as truecolour, so the picture does not
    /// depend on which palette reads it back. Trailing blanks on the default
    /// background are left out, because a saved screen is mostly padding.
    pub fn screen_ansi(&self) -> String {
        use alacritty_terminal::index::{Column, Line};
        use std::fmt::Write as _;

        let grid = self.term.grid();
        let lines = self.size.screen_lines();
        let columns = self.size.columns();
        let mut out = String::from("\x1b[0m\x1b[H\x1b[2J");

        for line in 0..lines {
            let row = &grid[Line(line as i32)];
            let mut cells = Vec::with_capacity(columns);
            for column in 0..columns {
                let cell = &row[Column(column)];
                let flags = cell.flags;
                if flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let mut fg = self.palette.resolve(cell.fg, flags.contains(Flags::BOLD));
                let mut bg = self.palette.resolve(cell.bg, false);
                if flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let style = Style {
                    fg,
                    bg,
                    bold: flags.contains(Flags::BOLD),
                    italic: flags.contains(Flags::ITALIC),
                    underline: flags.intersects(Flags::ALL_UNDERLINES),
                    strikethrough: flags.contains(Flags::STRIKEOUT),
                };
                cells.push((cell.c, style));
            }
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
                // A control character in a cell would be obeyed on the way back
                // in rather than drawn.
                out.push(if c.is_control() { ' ' } else { c });
            }
            out.push_str("\x1b[0m");
        }

        let cursor = grid.cursor.point;
        let _ = write!(
            out,
            "\x1b[{};{}H",
            cursor.line.0.max(0) + 1,
            cursor.column.0 + 1
        );
        out
    }

    /// The whole visible grid as text, for tests and for output indexing.
    pub fn visible_text(&self) -> String {
        let mut out = String::new();
        let mut last_line = 0i32;
        for indexed in self.term.renderable_content().display_iter {
            let line = indexed.point.line.0;
            if line != last_line {
                out.push('\n');
                last_line = line;
            }
            if !indexed.cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                out.push(indexed.cell.c);
            }
        }
        out
    }
}

/// The drawn style of one cell, for writing a screen back out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Style {
    fg: Rgb,
    bg: Rgb,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
}

impl Style {
    /// Underline and strike draw on a blank, so a blank carrying one is not
    /// padding.
    fn marks_blank(&self) -> bool {
        self.underline || self.strikethrough
    }

    fn write_sgr(&self, out: &mut String) {
        use std::fmt::Write as _;
        let _ = write!(
            out,
            "\x1b[0;38;2;{};{};{};48;2;{};{};{}",
            self.fg.r, self.fg.g, self.fg.b, self.bg.r, self.bg.g, self.bg.b
        );
        for (on, code) in [
            (self.bold, ";1"),
            (self.italic, ";3"),
            (self.underline, ";4"),
            (self.strikethrough, ";9"),
        ] {
            if on {
                out.push_str(code);
            }
        }
        out.push('m');
    }
}

/// Maps alacritty's colour model onto concrete RGB.
///
/// The 16 ANSI slots follow the theme; the 6x6x6 cube and the greyscale ramp
/// are computed, matching what xterm defines.
#[derive(Debug, Clone)]
pub struct Palette {
    pub ansi: [Rgb; 16],
    pub foreground: Rgb,
    pub background: Rgb,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            // A restrained, low-chroma set to match Berth's monochrome-plus-one
            // direction rather than the usual saturated defaults.
            ansi: [
                Rgb::new(0x18, 0x18, 0x1b),
                Rgb::new(0xd4, 0x55, 0x3d),
                Rgb::new(0x5f, 0xa3, 0x6b),
                Rgb::new(0xc2, 0x96, 0x40),
                Rgb::new(0x4c, 0x8d, 0xff),
                Rgb::new(0xa2, 0x71, 0xc4),
                Rgb::new(0x46, 0x9d, 0xa8),
                Rgb::new(0xd4, 0xd4, 0xd8),
                Rgb::new(0x52, 0x52, 0x5b),
                Rgb::new(0xe8, 0x6f, 0x57),
                Rgb::new(0x77, 0xbd, 0x83),
                Rgb::new(0xdc, 0xb0, 0x5a),
                Rgb::new(0x6f, 0xa5, 0xff),
                Rgb::new(0xba, 0x8b, 0xdc),
                Rgb::new(0x5f, 0xb7, 0xc2),
                Rgb::new(0xfa, 0xfa, 0xfa),
            ],
            foreground: Rgb::new(0xe4, 0xe4, 0xe7),
            background: Rgb::new(0x10, 0x10, 0x12),
        }
    }
}

impl Palette {
    pub fn resolve(&self, color: AnsiColor, bold: bool) -> Rgb {
        match color {
            AnsiColor::Named(named) => self.named(named, bold),
            AnsiColor::Spec(rgb) => Rgb::new(rgb.r, rgb.g, rgb.b),
            AnsiColor::Indexed(i) => self.indexed(i, bold),
        }
    }

    fn named(&self, named: NamedColor, bold: bool) -> Rgb {
        match named {
            NamedColor::Foreground => self.foreground,
            NamedColor::Background => self.background,
            NamedColor::Black => self.ansi[if bold { 8 } else { 0 }],
            NamedColor::Red => self.ansi[if bold { 9 } else { 1 }],
            NamedColor::Green => self.ansi[if bold { 10 } else { 2 }],
            NamedColor::Yellow => self.ansi[if bold { 11 } else { 3 }],
            NamedColor::Blue => self.ansi[if bold { 12 } else { 4 }],
            NamedColor::Magenta => self.ansi[if bold { 13 } else { 5 }],
            NamedColor::Cyan => self.ansi[if bold { 14 } else { 6 }],
            NamedColor::White => self.ansi[if bold { 15 } else { 7 }],
            NamedColor::BrightBlack => self.ansi[8],
            NamedColor::BrightRed => self.ansi[9],
            NamedColor::BrightGreen => self.ansi[10],
            NamedColor::BrightYellow => self.ansi[11],
            NamedColor::BrightBlue => self.ansi[12],
            NamedColor::BrightMagenta => self.ansi[13],
            NamedColor::BrightCyan => self.ansi[14],
            NamedColor::BrightWhite => self.ansi[15],
            NamedColor::BrightForeground => self.foreground,
            NamedColor::DimForeground => dim(self.foreground),
            NamedColor::DimBlack => dim(self.ansi[0]),
            NamedColor::DimRed => dim(self.ansi[1]),
            NamedColor::DimGreen => dim(self.ansi[2]),
            NamedColor::DimYellow => dim(self.ansi[3]),
            NamedColor::DimBlue => dim(self.ansi[4]),
            NamedColor::DimMagenta => dim(self.ansi[5]),
            NamedColor::DimCyan => dim(self.ansi[6]),
            NamedColor::DimWhite => dim(self.ansi[7]),
            NamedColor::Cursor => self.foreground,
        }
    }

    /// The xterm 256-colour layout: 0-15 ANSI, 16-231 a 6x6x6 cube, 232-255
    /// a 24-step greyscale ramp.
    fn indexed(&self, index: u8, bold: bool) -> Rgb {
        match index {
            0..=15 => {
                let i = if bold && index < 8 {
                    index as usize + 8
                } else {
                    index as usize
                };
                self.ansi[i]
            }
            16..=231 => {
                let i = index as u16 - 16;
                let level = |n: u16| -> u8 { if n == 0 { 0 } else { (n * 40 + 55) as u8 } };
                Rgb::new(level(i / 36), level((i / 6) % 6), level(i % 6))
            }
            232..=255 => {
                let v = (index as u16 - 232) * 10 + 8;
                Rgb::new(v as u8, v as u8, v as u8)
            }
        }
    }
}

fn dim(c: Rgb) -> Rgb {
    Rgb::new(
        (c.r as f32 * 0.66) as u8,
        (c.g as f32 * 0.66) as u8,
        (c.b as f32 * 0.66) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal() -> Terminal {
        Terminal::new(TermSize::new(20, 5), CellSize::new(8.0, 16.0))
    }

    fn text_of(term: &Terminal) -> String {
        term.visible_text().replace(' ', "").replace('\n', "|")
    }

    #[test]
    fn plain_text_lands_in_the_grid() {
        let mut t = terminal();
        t.advance(b"hello");
        assert!(t.visible_text().starts_with("hello"));
    }

    #[test]
    fn a_newline_moves_to_the_next_row() {
        let mut t = terminal();
        t.advance(b"ab\r\ncd");
        assert_eq!(text_of(&t), "ab|cd|||");
    }

    #[test]
    fn empty_input_is_a_no_op() {
        let mut t = terminal();
        let before = t.revision();
        t.advance(b"");
        assert_eq!(t.revision(), before);
    }

    #[test]
    fn output_bumps_the_revision() {
        let mut t = terminal();
        let before = t.revision();
        t.advance(b"x");
        assert_ne!(t.revision(), before);
    }

    #[test]
    fn colours_reach_the_snapshot() {
        let mut t = terminal();
        t.advance(b"\x1b[31mR");
        let snap = t.snapshot();
        let cell = snap.cells.iter().find(|c| c.c == 'R').unwrap();
        assert_eq!(cell.fg, Palette::default().ansi[1]);
    }

    #[test]
    fn true_colour_is_preserved_exactly() {
        let mut t = terminal();
        t.advance(b"\x1b[38;2;10;20;30mT");
        let snap = t.snapshot();
        let cell = snap.cells.iter().find(|c| c.c == 'T').unwrap();
        assert_eq!(cell.fg, Rgb::new(10, 20, 30));
    }

    #[test]
    fn inverse_swaps_foreground_and_background() {
        let mut t = terminal();
        t.advance(b"\x1b[7mI");
        let snap = t.snapshot();
        let cell = snap.cells.iter().find(|c| c.c == 'I').unwrap();
        assert_eq!(cell.fg, Palette::default().background);
    }

    #[test]
    fn attributes_reach_the_snapshot() {
        let mut t = terminal();
        t.advance(b"\x1b[1mB\x1b[0m\x1b[3mI\x1b[0m\x1b[4mU\x1b[0m\x1b[9mS");
        let snap = t.snapshot();
        let find = |c: char| *snap.cells.iter().find(|x| x.c == c).unwrap();
        assert!(find('B').bold);
        assert!(find('I').italic);
        assert!(find('U').underline);
        assert!(find('S').strikethrough);
    }

    #[test]
    fn the_alternate_screen_is_detected() {
        let mut t = terminal();
        assert!(!t.alternate_screen());
        t.advance(b"\x1b[?1049h");
        assert!(t.alternate_screen(), "vim and htop rely on this");
        t.advance(b"\x1b[?1049l");
        assert!(!t.alternate_screen());
    }

    #[test]
    fn application_cursor_mode_is_detected() {
        let mut t = terminal();
        assert!(!t.application_cursor());
        t.advance(b"\x1b[?1h");
        assert!(t.application_cursor());
    }

    #[test]
    fn the_cursor_moves_with_output() {
        let mut t = terminal();
        t.advance(b"abc");
        let snap = t.snapshot();
        assert_eq!(snap.cursor.column, 3);
        assert_eq!(snap.cursor.line, 0);
    }

    #[test]
    fn cursor_positioning_escapes_are_honoured() {
        let mut t = terminal();
        t.advance(b"\x1b[3;5H");
        let snap = t.snapshot();
        assert_eq!((snap.cursor.line, snap.cursor.column), (2, 4));
    }

    #[test]
    fn resizing_changes_the_grid() {
        let mut t = terminal();
        t.resize(TermSize::new(40, 10), CellSize::new(8.0, 16.0));
        assert_eq!(t.size(), TermSize::new(40, 10));
        assert_eq!(t.snapshot().size, TermSize::new(40, 10));
    }

    #[test]
    fn resizing_to_the_same_size_is_a_no_op() {
        let mut t = terminal();
        let before = t.revision();
        t.resize(TermSize::new(20, 5), CellSize::new(8.0, 16.0));
        assert_eq!(t.revision(), before);
    }

    #[test]
    fn wide_characters_do_not_emit_a_spacer_cell() {
        let mut t = terminal();
        t.advance("日本".as_bytes());
        let snap = t.snapshot();
        let glyphs: Vec<char> = snap
            .cells
            .iter()
            .filter(|c| c.c != ' ')
            .map(|c| c.c)
            .collect();
        assert_eq!(glyphs, vec!['日', '本']);
        // The second column of a wide glyph is skipped, so columns advance by 2.
        let cols: Vec<u16> = snap
            .cells
            .iter()
            .filter(|c| c.c != ' ')
            .map(|c| c.column)
            .collect();
        assert_eq!(cols, vec![0, 2]);
    }

    #[test]
    fn a_snapshot_covers_the_whole_visible_grid() {
        let t = terminal();
        let snap = t.snapshot();
        assert_eq!(snap.cells.len(), 20 * 5);
    }

    #[test]
    fn clearing_the_screen_empties_the_grid() {
        let mut t = terminal();
        t.advance(b"noise everywhere");
        t.advance(b"\x1b[2J\x1b[H");
        assert_eq!(text_of(&t), "||||");
    }

    #[test]
    fn the_256_colour_cube_matches_xterm() {
        let p = Palette::default();
        // Known xterm values.
        assert_eq!(p.indexed(16, false), Rgb::new(0, 0, 0));
        assert_eq!(p.indexed(21, false), Rgb::new(0, 0, 255));
        assert_eq!(p.indexed(231, false), Rgb::new(255, 255, 255));
        assert_eq!(p.indexed(232, false), Rgb::new(8, 8, 8));
        assert_eq!(p.indexed(255, false), Rgb::new(238, 238, 238));
    }

    #[test]
    fn scrollback_is_reported() {
        let mut t = terminal();
        for i in 0..20 {
            t.advance(format!("line{i}\r\n").as_bytes());
        }
        let snap = t.snapshot();
        assert!(!snap.is_scrolled_back(), "the view follows new output");
    }

    #[test]
    fn shell_events_carry_the_row_they_landed_on() {
        use crate::shell::ShellEventKind;
        let mut t = terminal();
        // Prompt on row 0, command runs, output on row 1, end on row 2.
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n");
        t.advance(b"\x1b]133;C\x07out\r\n\x1b]133;D;0\x07");

        let events = t.drain_shell_events();
        let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ShellEventKind::PromptStart,
                ShellEventKind::CommandStart,
                ShellEventKind::OutputStart,
                ShellEventKind::CommandEnd { exit_code: Some(0) },
            ]
        );
        assert_eq!(events[0].line, 0, "prompt starts on the first row");
        assert_eq!(events[1].line, 0, "command starts on the prompt row");
        assert_eq!(events[2].line, 1, "output starts on the next row");
        assert_eq!(events[3].line, 2);
    }

    #[test]
    fn draining_shell_events_clears_them() {
        let mut t = terminal();
        t.advance(b"\x1b]133;A\x07");
        assert_eq!(t.drain_shell_events().len(), 1);
        assert!(t.drain_shell_events().is_empty());
    }

    #[test]
    fn shell_integration_is_only_reported_once_a_marker_arrives() {
        let mut t = terminal();
        assert!(!t.has_shell_integration());
        t.advance(b"just output\r\n");
        assert!(!t.has_shell_integration());
        t.advance(b"\x1b]133;A\x07");
        assert!(t.has_shell_integration());
    }

    /// The markers must not end up drawn on screen.
    #[test]
    fn markers_leave_no_visible_text() {
        let mut t = terminal();
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        let text = t.visible_text();
        assert!(!text.contains("133"), "markers must not render: {text:?}");
        assert!(text.contains('$'));
    }

    #[test]
    fn mouse_reporting_is_detected_and_released() {
        let mut t = terminal();
        assert!(!t.mouse_reporting());
        t.advance(b"\x1b[?1000h");
        assert!(t.mouse_reporting());
        t.advance(b"\x1b[?1000l");
        assert!(!t.mouse_reporting());
    }

    #[test]
    fn bracketed_paste_mode_is_detected() {
        let mut t = terminal();
        assert!(!t.bracketed_paste());
        t.advance(b"\x1b[?2004h");
        assert!(t.bracketed_paste());
        t.advance(b"\x1b[?2004l");
        assert!(!t.bracketed_paste());
    }

    #[test]
    fn selection_returns_the_text_on_screen() {
        let mut t = terminal();
        t.advance(b"hello world");
        let text = t.selection_text(Selection::new(0, 0, 0, 4));
        assert_eq!(text, "hello");
    }

    #[test]
    fn selection_spans_rows_and_drops_row_padding() {
        let mut t = terminal();
        t.advance(b"ab\r\ncd");
        let text = t.selection_text(Selection::new(0, 0, 1, 19));
        assert_eq!(text, "ab\ncd", "trailing grid padding must not be copied");
    }

    #[test]
    fn a_backwards_selection_reads_the_same() {
        let mut t = terminal();
        t.advance(b"hello");
        assert_eq!(
            t.selection_text(Selection::new(0, 4, 0, 0)),
            t.selection_text(Selection::new(0, 0, 0, 4))
        );
    }

    #[test]
    fn an_empty_selection_yields_one_cell() {
        let mut t = terminal();
        t.advance(b"hello");
        assert!(Selection::new(0, 0, 0, 0).is_empty());
        assert_eq!(t.selection_text(Selection::new(0, 1, 0, 1)), "e");
    }

    #[test]
    fn a_torrent_of_output_does_not_panic() {
        let mut t = terminal();
        let noise: Vec<u8> = (0..100_000u32).map(|n| (n % 251) as u8).collect();
        t.advance(&noise);
        let snap = t.snapshot();
        assert_eq!(snap.cells.len(), 20 * 5);
    }

    /// Five visible rows, so twenty lines of output leaves fifteen behind.
    fn with_history() -> Terminal {
        let mut t = terminal();
        for n in 0..20 {
            t.advance(format!("line{n}\r\n").as_bytes());
        }
        t
    }

    /// The gap this closes: the history was kept and could not be looked at.
    #[test]
    fn the_history_can_be_scrolled_into() {
        let mut t = with_history();
        assert_eq!(t.display_offset(), 0);
        assert!(!text_of(&t).contains("line3|"));

        t.scroll(Scroll::Lines(10));

        assert_eq!(t.display_offset(), 10);
        assert!(text_of(&t).contains("line6"), "{}", text_of(&t));
    }

    #[test]
    fn scrolling_to_the_bottom_comes_back_to_the_live_grid() {
        let mut t = with_history();
        let live = text_of(&t);
        t.scroll(Scroll::PageUp);
        assert!(t.display_offset() > 0);

        t.scroll(Scroll::Bottom);

        assert_eq!(t.display_offset(), 0);
        assert_eq!(text_of(&t), live);
    }

    #[test]
    fn scrolling_stops_at_the_oldest_line_it_has() {
        let mut t = with_history();
        t.scroll(Scroll::Lines(10_000));
        let top = t.display_offset();
        assert!(top > 0);

        t.scroll(Scroll::Lines(10_000));

        assert_eq!(t.display_offset(), top, "the top is the top");
    }

    /// A page down from the bottom must not walk into negative offsets, which
    /// is the shape of bug that shows an empty grid.
    #[test]
    fn scrolling_down_from_the_bottom_does_nothing() {
        let mut t = with_history();
        t.scroll(Scroll::PageDown);
        assert_eq!(t.display_offset(), 0);
    }

    /// Output arriving while scrolled back must not yank the view to the
    /// bottom. Reading something while a build scrolls past is the whole point.
    #[test]
    fn new_output_does_not_move_a_scrolled_back_view() {
        let mut t = with_history();
        t.scroll(Scroll::Lines(8));
        let showing = text_of(&t);

        t.advance(b"noise\r\nmore noise\r\n");

        assert_eq!(text_of(&t), showing, "the view followed the output");
    }

    /// The alternate screen has no history behind it, so there is nothing to
    /// scroll into and a full-screen program owns the grid.
    #[test]
    fn the_alternate_screen_has_nothing_to_scroll_into() {
        let mut t = with_history();
        t.advance(b"\x1b[?1049h");
        assert!(t.alternate_screen());

        t.scroll(Scroll::PageUp);

        assert_eq!(t.display_offset(), 0);
    }

    #[test]
    fn a_move_that_changes_nothing_does_not_ask_for_a_repaint() {
        let mut t = with_history();
        let revision = t.revision();
        t.scroll(Scroll::PageDown);
        assert_eq!(t.revision(), revision);

        t.scroll(Scroll::PageUp);
        assert_ne!(t.revision(), revision);
    }

    #[test]
    fn a_plain_grid_takes_the_wheel_itself() {
        assert_eq!(terminal().wheel_action(false), WheelAction::View);
    }

    #[test]
    fn a_mouse_reporting_program_gets_the_wheel() {
        let mut t = terminal();
        t.advance(b"\x1b[?1000h");
        assert_eq!(t.wheel_action(false), WheelAction::Report);
    }

    /// Without this a program that grabs the mouse leaves no way to look at
    /// anything above the screen.
    #[test]
    fn shift_takes_the_wheel_back_from_the_program() {
        let mut t = terminal();
        t.advance(b"\x1b[?1000h");
        assert_eq!(t.wheel_action(true), WheelAction::View);
    }

    /// What makes the wheel work in `less` and `man`.
    #[test]
    fn alternate_scroll_asks_for_arrow_keys() {
        let mut t = terminal();
        t.advance(b"\x1b[?1049h\x1b[?1007h");
        assert_eq!(t.wheel_action(false), WheelAction::Arrows);
    }

    /// The alternate screen alone is not enough: a program that turned
    /// alternate scroll off means it.
    #[test]
    fn the_alternate_screen_alone_does_not_ask_for_arrows() {
        let mut t = terminal();
        t.advance(b"\x1b[?1049h\x1b[?1007l");
        assert_eq!(t.wheel_action(false), WheelAction::View);
    }

    fn replayed(from: &Terminal) -> Terminal {
        let mut to = Terminal::new(from.size(), CellSize::new(8.0, 16.0));
        to.advance(from.screen_ansi().as_bytes());
        to
    }

    /// The point of the whole thing: what comes back is what was saved.
    #[test]
    fn a_saved_screen_redraws_the_same() {
        let mut term = terminal();
        term.advance(b"plain\r\n\x1b[1;31mbold red\x1b[0m \x1b[4mund\x1b[0m\r\n\x1b[44m blue bg \x1b[0m\r\n$ ");
        let back = replayed(&term);

        assert_eq!(back.visible_text(), term.visible_text());
        let (before, after) = (term.snapshot(), back.snapshot());
        assert_eq!(before.cursor, after.cursor);
        for (a, b) in before.cells.iter().zip(after.cells.iter()) {
            assert_eq!(
                (a.c, a.fg, a.bg, a.bold, a.underline),
                (b.c, b.fg, b.bg, b.bold, b.underline),
                "{a:?}"
            );
        }
    }

    /// Inverse is saved as the colours it produced, since the reader has no
    /// reason to know a swap happened.
    #[test]
    fn inverse_comes_back_as_its_colours() {
        let mut term = terminal();
        term.advance(b"\x1b[7minv\x1b[0m");
        let back = replayed(&term);
        let (a, b) = (&term.snapshot().cells[0], &back.snapshot().cells[0]);
        assert_eq!((a.fg, a.bg), (b.fg, b.bg));
    }

    #[test]
    fn wide_characters_keep_their_columns() {
        let mut term = terminal();
        term.advance("\u{4e2d}\u{6587}x".as_bytes());
        let back = replayed(&term);
        assert_eq!(back.visible_text(), term.visible_text());
        assert_eq!(back.snapshot().cursor, term.snapshot().cursor);
    }

    /// The live screen, not wherever the view was left.
    #[test]
    fn a_scrolled_view_saves_the_live_screen() {
        let mut term = terminal();
        for n in 0..30 {
            term.advance(format!("line {n}\r\n").as_bytes());
        }
        let live = term.visible_text();
        term.scroll(Scroll::Lines(10));
        assert_ne!(term.visible_text(), live);
        assert_eq!(replayed(&term).visible_text(), live);
    }

    /// Padding is not worth saving. A blank screen is a clear and a cursor.
    #[test]
    fn a_blank_screen_saves_almost_nothing() {
        let term = terminal();
        assert_eq!(term.screen_ansi(), "\x1b[0m\x1b[H\x1b[2J\x1b[1;1H");
    }
}
