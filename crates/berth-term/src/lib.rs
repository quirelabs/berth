//! Terminal emulation. Owns the alacritty grid and the input encoding.
//!
//! Deliberately free of any UI dependency: the GPUI element that paints the
//! grid lives in `berth-app`, so everything here is testable headless.

pub mod keys;
pub mod size;
pub mod terminal;

pub use keys::{Key, KeyEvent, Modifiers, TermMode, encode};
pub use size::{CellSize, TermSize};
pub use terminal::{Cell, Cursor, Palette, Rgb, Snapshot, Terminal};
