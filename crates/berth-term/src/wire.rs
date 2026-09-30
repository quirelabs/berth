//! The terminal as Berth's wire format has it (D49, D50): cells with their
//! colours as the program set them, not resolved, so the app's own palette
//! applies at the other end. libghostty's own snapshot never leaves this
//! crate.
//!
//! Read cell by cell through grid references, which libghostty documents as
//! slower than its render state. That is fine for what this serves: a screen
//! on attach or when an app falls behind, at most 30 a second (D50), and
//! history in ranges on a lower-priority stream.

use berth_proto::v1;
use libghostty_vt::screen::{CellContentTag, CellWide};
use libghostty_vt::style::{Style, StyleColor, Underline};
use libghostty_vt::terminal::{Mode, Point, PointCoordinate};

use crate::engine::{Ghostty, modes_of};

fn color(c: StyleColor) -> v1::Color {
    match c {
        StyleColor::None => v1::Color::Default,
        StyleColor::Palette(index) => v1::Color::Palette(index.0),
        StyleColor::Rgb(rgb) => v1::Color::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

fn style_bits(s: &Style) -> u16 {
    use v1::style::*;
    [
        (s.bold, BOLD),
        (s.italic, ITALIC),
        (!matches!(s.underline, Underline::None), UNDERLINE),
        (s.strikethrough, STRIKETHROUGH),
        (s.inverse, INVERSE),
        (s.faint, FAINT),
        (s.blink, BLINK),
        (s.invisible, INVISIBLE),
    ]
    .into_iter()
    .filter(|(on, _)| *on)
    .fold(0, |bits, (_, bit)| bits | bit)
}

fn blank() -> v1::Cell {
    v1::Cell {
        text: String::new(),
        fg: v1::Color::Default,
        bg: v1::Color::Default,
        style: 0,
        width: 1,
    }
}

/// One cell, or a blank where libghostty has nothing to say about it.
fn cell(term: &Ghostty, point: Point, graphemes: &mut Vec<char>) -> v1::Cell {
    let Ok(grid) = term.grid_ref(point) else {
        return blank();
    };
    let Ok(raw) = grid.cell() else {
        return blank();
    };
    let style = grid.style().unwrap_or_default();
    let width = match raw.wide() {
        Ok(CellWide::Wide) => 2,
        Ok(CellWide::SpacerTail) => 0,
        _ => 1,
    };
    // A cell with no text can still carry a background of its own, outside
    // its style: an erase with a colour set.
    let bg = match raw.content_tag() {
        Ok(CellContentTag::BgColorPalette) => raw
            .bg_color_palette()
            .map(|p| v1::Color::Palette(p.0))
            .unwrap_or(v1::Color::Default),
        Ok(CellContentTag::BgColorRgb) => raw
            .bg_color_rgb()
            .map(|c| v1::Color::Rgb(c.r, c.g, c.b))
            .unwrap_or(v1::Color::Default),
        _ => color(style.bg_color),
    };
    let text = if width == 0 {
        String::new()
    } else {
        loop {
            match grid.graphemes(graphemes) {
                Ok(len) => break graphemes[..len].iter().collect(),
                Err(libghostty_vt::Error::OutOfSpace { required }) => {
                    graphemes.resize(required, '\0');
                }
                Err(_) => break String::new(),
            }
        }
    };
    v1::Cell {
        text,
        fg: color(style.fg_color),
        bg,
        style: style_bits(&style),
        width,
    }
}

fn row(term: &Ghostty, cols: u16, point: impl Fn(u16) -> Point) -> v1::Row {
    let mut graphemes = vec!['\0'; 16];
    let cells = (0..cols)
        .map(|x| cell(term, point(x), &mut graphemes))
        .collect();
    let wrapped = term
        .grid_ref(point(0))
        .and_then(|g| g.row())
        .and_then(|r| r.is_wrapped())
        .unwrap_or(false);
    v1::Row { cells, wrapped }
}

/// The active screen: what a fresh app copy needs to draw exactly this.
pub(crate) fn screen(term: &Ghostty, first_line: u64) -> Option<v1::Screen> {
    let cols = term.cols().ok()?;
    let rows = term.rows().ok()?;
    let lines = (0..rows)
        .map(|y| {
            row(term, cols, |x| {
                Point::Active(PointCoordinate { x, y: y.into() })
            })
        })
        .collect();
    let modes = modes_of(term);
    Some(v1::Screen {
        first_line,
        cols,
        rows,
        cursor: v1::Cursor {
            line: term.cursor_y().ok()?,
            column: term.cursor_x().ok()?,
            visible: term.mode(Mode::CURSOR_VISIBLE).unwrap_or(true),
        },
        modes: v1::Modes {
            alternate_screen: modes.alternate_screen,
            application_cursor: modes.application_cursor,
            mouse_reporting: modes.mouse_reporting,
            bracketed_paste: modes.bracketed_paste,
            alternate_scroll: modes.alternate_scroll,
        },
        lines,
    })
}

/// History lines `first..first + count`, counted from the oldest held, newest
/// first as the wire has them.
pub(crate) fn history(term: &Ghostty, first: u32, count: u32) -> Vec<v1::Row> {
    let cols = term.cols().unwrap_or(0);
    (first..first + count)
        .rev()
        .map(|y| row(term, cols, |x| Point::History(PointCoordinate { x, y })))
        .collect()
}

#[cfg(test)]
mod tests {
    use berth_proto::v1::{self, Color};

    use crate::engine::Scrollback;
    use crate::size::{CellSize, TermSize};
    use crate::terminal::Terminal;

    fn terminal(cols: u16, rows: u16, lines: usize) -> Terminal {
        Terminal::with_scrollback(
            TermSize::new(cols, rows),
            CellSize::new(8.0, 16.0),
            Scrollback {
                lines: Some(lines),
                bytes: None,
            },
        )
    }

    fn text(row: &v1::Row) -> String {
        row.cells
            .iter()
            .map(|c| c.text.as_str())
            .collect::<String>()
    }

    /// Colours go out as the program set them, so the app's palette applies.
    #[test]
    fn colours_are_sent_unresolved() {
        let t = terminal(20, 3, 100);
        t.advance(b"\x1b[31mA\x1b[38;2;1;2;3mB\x1b[0mC\x1b[44mD\x1b[0m\x1b[1;3;4;7mE");
        let screen = t.wire_screen().unwrap();
        let cells = &screen.lines[0].cells;
        assert_eq!(cells[0].fg, Color::Palette(1));
        assert_eq!(cells[1].fg, Color::Rgb(1, 2, 3));
        assert_eq!((cells[2].fg, cells[2].bg), (Color::Default, Color::Default));
        assert_eq!(cells[3].bg, Color::Palette(4));
        use v1::style::*;
        assert_eq!(cells[4].style, BOLD | ITALIC | UNDERLINE | INVERSE);
    }

    /// An erase with a background set colours cells that hold no text.
    #[test]
    fn an_erased_background_is_kept() {
        let t = terminal(10, 2, 100);
        t.advance(b"\x1b[42m\x1b[2K\x1b[0m");
        let screen = t.wire_screen().unwrap();
        assert_eq!(screen.lines[0].cells[5].bg, Color::Palette(2));
    }

    #[test]
    fn wide_glyphs_and_graphemes_keep_their_columns() {
        let t = terminal(10, 2, 100);
        t.advance("\u{65e5}e\u{301}x".as_bytes());
        let cells = &t.wire_screen().unwrap().lines[0].cells;
        assert_eq!((cells[0].text.as_str(), cells[0].width), ("\u{65e5}", 2));
        assert_eq!((cells[1].text.as_str(), cells[1].width), ("", 0));
        assert_eq!(cells[2].text, "e\u{301}");
        assert_eq!(cells[3].text, "x");
        assert_eq!(cells.len(), 10, "every column is sent");
    }

    #[test]
    fn the_cursor_modes_and_soft_wraps_are_sent() {
        let t = terminal(5, 3, 100);
        t.advance(b"\x1b[?1h\x1b[?2004habcdefg");
        let screen = t.wire_screen().unwrap();
        assert_eq!((screen.cols, screen.rows), (5, 3));
        assert_eq!((screen.cursor.line, screen.cursor.column), (1, 2));
        assert!(screen.modes.application_cursor && screen.modes.bracketed_paste);
        assert!(screen.lines[0].wrapped, "abcde runs on into fg");
        assert!(!screen.lines[1].wrapped);
        assert_eq!(text(&screen.lines[1]).trim_end(), "fg");
    }

    fn lines(t: &Terminal, from: u32, to: u32) {
        let out: String = (from..to).map(|n| format!("L{n}\r\n")).collect();
        t.advance(out.as_bytes());
    }

    /// Five rows, so after L0 to L29 the screen shows L26 to L29 and the
    /// cursor's empty row, and the history holds L0 to L25.
    #[test]
    fn history_comes_newest_first_and_numbered() {
        let t = terminal(10, 5, 1000);
        lines(&t, 0, 30);
        let newest = t.wire_history(None, 3);
        assert_eq!(newest.newest, 25);
        let got: Vec<String> = newest
            .lines
            .iter()
            .map(|r| text(r).trim_end().to_string())
            .collect();
        assert_eq!(got, ["L25", "L24", "L23"]);

        let older = t.wire_history(Some(23), 100);
        assert_eq!(older.newest, 22);
        assert_eq!(older.lines.len(), 23);
        assert_eq!(text(older.lines.last().unwrap()).trim_end(), "L0");
        assert!(t.wire_history(Some(0), 10).lines.is_empty());
    }

    fn numbered(t: &Terminal) -> Vec<(u64, String)> {
        let all = t.wire_history(None, u32::MAX);
        all.lines
            .iter()
            .enumerate()
            .map(|(i, row)| (all.newest - i as u64, text(row).trim_end().to_string()))
            .collect()
    }

    /// The point of the numbering: pruning the oldest lines does not shift
    /// the numbers of the rest, so ranges fetched at different times fit.
    #[test]
    fn a_line_keeps_its_number_through_pruning() {
        let t = terminal(80, 5, 100);
        let mut seen = std::collections::HashMap::new();
        let mut newest = 0;
        for chunk in 0..200 {
            lines(&t, chunk * 50, chunk * 50 + 50);
            let now = numbered(&t);
            assert!(now[0].0 >= newest, "numbers never go back");
            newest = now[0].0;
            for (number, line) in now {
                let first = *seen.entry(line.clone()).or_insert(number);
                assert_eq!(first, number, "{line} was renumbered");
            }
        }
        let held = numbered(&t).len();
        assert!(held < 10_000, "history was pruned: {held} held");
    }

    /// A write far longer than the history: nothing held collides with a
    /// number used before it.
    #[test]
    fn one_long_write_never_reuses_a_number() {
        let t = terminal(10, 5, 100);
        lines(&t, 0, 50);
        let before = numbered(&t)[0].0;
        lines(&t, 50, 20_050);
        let after = numbered(&t);
        assert!(after.last().map(|l| l.0) > Some(before));
        assert_eq!(after[0].1, "L20045");
    }

    /// Until anything is pruned, a line's number is its place in the output.
    #[test]
    fn numbers_count_lines_until_something_is_pruned() {
        let t = terminal(10, 5, 10_000);
        lines(&t, 0, 3000);
        for (number, line) in numbered(&t) {
            assert_eq!(line, format!("L{number}"));
        }
    }

    /// A scroll of more lines than the history holds, in a handful of bytes:
    /// the numbers skip ahead and never go back.
    #[test]
    fn numbers_only_grow_when_a_scroll_outruns_the_history() {
        let t = terminal(10, 5, 100);
        lines(&t, 0, 50);
        let before = t.wire_history(None, 1).newest;
        t.advance(b"\x1b[9999S");
        lines(&t, 50, 60);
        assert!(t.wire_history(None, 1).newest > before);
    }

    /// The screen's first row is the line after the newest in history, by
    /// the worker's numbers, through pruning and on the alternate screen.
    #[test]
    fn a_screen_carries_the_number_of_its_first_row() {
        let t = terminal(80, 5, 100);
        for chunk in 0..200 {
            lines(&t, chunk * 50, chunk * 50 + 50);
            let newest = t.wire_history(None, 1).newest;
            assert_eq!(t.wire_screen().unwrap().first_line, newest + 1);
        }
        let primary = t.wire_screen().unwrap().first_line;
        t.advance(b"\x1b[?1049h");
        assert_eq!(t.wire_screen().unwrap().first_line, primary);
    }

    #[test]
    fn the_alternate_screen_has_no_history_to_send() {
        let t = terminal(10, 5, 100);
        lines(&t, 0, 30);
        t.advance(b"\x1b[?1049h");
        assert!(t.wire_history(None, 10).lines.is_empty());
        assert!(t.wire_screen().unwrap().modes.alternate_screen);
        t.advance(b"\x1b[?1049l");
        assert_eq!(t.wire_history(None, 1).newest, 25);
    }
}
