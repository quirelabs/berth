//! Grid geometry.
//!
//! Cell metrics are plain `f32` rather than `gpui::Pixels`, because this crate
//! may not depend on gpui. `berth-app` measures the font, passes the cell size
//! in, and converts back at the boundary.

/// The pixel size of one cell, measured by the renderer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellSize {
    pub width: f32,
    pub height: f32,
}

impl CellSize {
    pub fn new(width: f32, height: f32) -> Self {
        // A zero or negative cell would make every division below explode.
        Self {
            width: width.max(1.0),
            height: height.max(1.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
}

impl TermSize {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(MIN_COLS),
            rows: rows.max(MIN_ROWS),
        }
    }

    /// Fit a grid to a pixel area.
    pub fn from_pixels(width: f32, height: f32, cell: CellSize) -> Self {
        Self::new(fit(width, cell.width), fit(height, cell.height))
    }
}

/// A grid one column wide cannot hold a wide glyph, which terminal engines
/// handle badly (zed-industries/zed#2750), so it never gets narrower than two.
pub const MIN_COLS: u16 = 2;
pub const MIN_ROWS: u16 = 1;

/// How many whole cells fit in `available`.
///
/// `next_up` before flooring because `n * cell / cell` can land a hair under
/// `n` in f32, and flooring that silently drops the last row off the screen.
/// Copied from Zed's `TerminalBounds::num_lines`, which carries the same note.
fn fit(available: f32, cell: f32) -> u16 {
    if !available.is_finite() || available <= 0.0 {
        return 1;
    }
    let raw = available / cell;
    raw.next_up().floor().clamp(1.0, u16::MAX as f32) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_whole_cells() {
        let cell = CellSize::new(10.0, 20.0);
        let size = TermSize::from_pixels(100.0, 200.0, cell);
        assert_eq!((size.cols, size.rows), (10, 10));
    }

    /// The precision guard: exact multiples must not lose a row to f32 error.
    #[test]
    fn exact_multiples_do_not_lose_a_row() {
        for n in 1..200u16 {
            let cell = CellSize::new(7.0, 17.5);
            let size = TermSize::from_pixels(n as f32 * 7.0, n as f32 * 17.5, cell);
            assert_eq!(size.rows, n.max(MIN_ROWS), "lost a row at n={n}");
            assert_eq!(size.cols, n.max(MIN_COLS), "lost a column at n={n}");
        }
    }

    #[test]
    fn partial_cells_are_not_counted() {
        let cell = CellSize::new(10.0, 20.0);
        let size = TermSize::from_pixels(105.0, 219.0, cell);
        assert_eq!((size.cols, size.rows), (10, 10));
    }

    #[test]
    fn never_smaller_than_the_minimum() {
        let cell = CellSize::new(10.0, 20.0);
        let size = TermSize::from_pixels(1.0, 1.0, cell);
        assert_eq!(size.cols, MIN_COLS);
        assert_eq!(size.rows, MIN_ROWS);
    }

    #[test]
    fn degenerate_input_does_not_panic() {
        let cell = CellSize::new(0.0, 0.0);
        assert!(cell.width >= 1.0 && cell.height >= 1.0);
        for bad in [0.0, -50.0, f32::NAN, f32::INFINITY] {
            let size = TermSize::from_pixels(bad, bad, CellSize::new(8.0, 16.0));
            assert!(size.cols >= MIN_COLS && size.rows >= MIN_ROWS, "{bad}");
        }
    }
}
