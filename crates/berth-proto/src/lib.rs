//! Berth's own wire format (D49, D50): how the app talks to the server that
//! holds its sessions, over a local socket now and an SSH channel from step 3.
//!
//! A connection is a stream of frames: a little-endian `u32` length, then that
//! many bytes of payload. The first frame each way is a [`Hello`], whose
//! encoding never changes, and the two sides agree on the newest protocol
//! version both speak. Every frame after that is one message of that version,
//! encoded with postcard. Screens and history travel zstd-compressed inside
//! their messages ([`Packed`]).
//!
//! Each version's messages live in their own module and are frozen once
//! released: a newer build keeps every older module, so an old worker still
//! running after an update is always reachable. `tests/frozen.rs` holds the
//! bytes of every frozen message and fails if an edit changes one.
//!
//! libghostty's own snapshot format never crosses the wire (D49): it belongs to
//! one build of the engine, and the two ends of a connection may be different
//! builds.
//!
//! Pure: no sockets, no tasks, no terminal. Anything that can be wrong about
//! bytes is tested here without a network.

mod error;
mod frame;
mod hello;
mod packed;
pub mod v1;

pub use error::Error;
pub use frame::{FrameReader, MAX_FRAME, frame};
pub use hello::{Agreed, Hello, MAGIC, NEWEST, OLDEST, negotiate};
pub use packed::{MAX_UNPACKED, Packed};

/// Which way a message travels. Part of an unknown message's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ToServer,
    ToApp,
}

impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Direction::ToServer => "to the server",
            Direction::ToApp => "to the app",
        })
    }
}

/// Decode exactly one value from `bytes`, refusing anything left over: a
/// payload with trailing bytes was written by something that does not speak
/// this format, and reading past a guess is how a protocol drifts.
pub(crate) fn decode_exact<'a, T: serde::Deserialize<'a>>(
    bytes: &'a [u8],
    what: &'static str,
) -> Result<T, Error> {
    let (value, rest) = postcard::take_from_bytes(bytes).map_err(|e| Error::Malformed {
        what,
        detail: e.to_string(),
    })?;
    if !rest.is_empty() {
        return Err(Error::Malformed {
            what,
            detail: format!("{} bytes left over", rest.len()),
        });
    }
    Ok(value)
}

pub(crate) fn encode<T: serde::Serialize>(value: &T, what: &'static str) -> Result<Vec<u8>, Error> {
    postcard::to_stdvec(value).map_err(|e| Error::Malformed {
        what,
        detail: e.to_string(),
    })
}
