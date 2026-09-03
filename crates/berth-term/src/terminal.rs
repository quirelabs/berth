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

use crate::size::{CellSize, TermSize};

/// Local scrollback. tmux keeps far more on the remote.
pub const SCROLLBACK_LINES: usize = 10_000;

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

pub struct Terminal {
    term: Term<Listener>,
    parser: Processor,
    listener: Listener,
    size: TermSize,
    cell_size: CellSize,
    palette: Palette,
    /// Bumped whenever output changes the grid, so the renderer can skip work.
    revision: u64,
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
        }
    }

    pub fn size(&self) -> TermSize {
        self.size
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Feed raw PTY bytes to the parser.
    pub fn advance(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.parser.advance(&mut self.term, bytes);
        self.revision = self.revision.wrapping_add(1);
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
    fn a_torrent_of_output_does_not_panic() {
        let mut t = terminal();
        let noise: Vec<u8> = (0..100_000u32).map(|n| (n % 251) as u8).collect();
        t.advance(&noise);
        let snap = t.snapshot();
        assert_eq!(snap.cells.len(), 20 * 5);
    }
}
