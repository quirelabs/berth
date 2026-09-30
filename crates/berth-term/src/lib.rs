//! Terminal emulation. Owns the engine, libghostty-vt on its own thread, and the
//! input encoding.
//!
//! Deliberately free of any UI dependency: the GPUI element that paints the
//! grid lives in `berth-app`, so everything here is testable headless.

pub mod engine;
pub mod keys;
pub mod paste;
pub mod shell;
pub mod size;
pub mod terminal;

pub use keys::{Key, KeyEvent, Modifiers, TermMode, alt_scroll, encode};
pub use paste::{MouseButton, mouse_sgr, paste_bytes};
pub use shell::{ShellEvent, ShellEventKind};
pub use size::{CellSize, TermSize};
pub use terminal::{
    Cell, Cursor, Modes, Palette, Rgb, Scroll, Selection, Snapshot, Terminal, WheelAction,
};
