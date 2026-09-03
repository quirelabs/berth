//! Local session recording, search and replay.
//!
//! Everything lives in one unencrypted SQLite file under the user's home
//! directory. Nothing leaves the machine.

pub mod ansi;
pub mod db;
pub mod query;
pub mod retention;

pub use db::Timeline;
pub use query::Query;

use std::path::PathBuf;

/// Raw output is buffered to roughly this size before being compressed.
pub const CHUNK_TARGET_BYTES: usize = 64 * 1024;
/// zstd level 3: the default trade-off until profiling says otherwise.
pub const ZSTD_LEVEL: i32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("no home directory")]
    NoHome,
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn default_path() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|h| h.join(".berth").join("timeline.db"))
        .ok_or(Error::NoHome)
}
