//! Berth's own server on this machine (D49 step 2, D50).
//!
//! One supervisor per user, and one worker process per session. The app
//! connects to the supervisor; the supervisor relays each session's traffic to
//! its worker and back. Every process listens on its own endpoint in the
//! runtime directory, so a supervisor that crashes or is restarted finds its
//! workers again from their files there, and a worker never depends on the
//! process that started it.
//!
//! Everything speaks Berth's wire format ([`berth_proto`]): a handshake, then
//! v1 messages in length-prefixed frames.
//!
//! Only the user who started a process can connect to it, checked twice: the
//! runtime directory is private to them (a DACL for the pipe on Windows), and
//! on Unix every connection's peer is checked against the endpoint's owner.

pub mod bench;
pub mod cli;
pub mod client;
pub mod hook;
pub mod link;
pub mod local;
pub mod paths;
pub mod supervisor;
pub mod sync;
mod transport;
pub mod worker_host;

#[cfg(all(test, unix))]
mod tests;

pub use transport::{Listener, Stream, connect};

/// What this build announces in its handshake. Nothing optional yet.
pub const FEATURES: &[&str] = &[];

/// This build, as its handshake names it: the version, and the name of the
/// binary it runs from, which for a worker is its versioned copy. The
/// supervisor keeps the copies running workers name (D50).
pub fn build() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_default();
    format!("berth {} ({exe})", env!("CARGO_PKG_VERSION"))
}
