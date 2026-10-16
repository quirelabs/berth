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
pub mod remove;
pub mod supervisor;
pub mod sync;
mod transport;
pub mod worker_host;

#[cfg(all(test, unix))]
mod tests;

pub use transport::{Listener, Stream, connect};

/// What this build announces in its handshake. Nothing optional yet.
pub const FEATURES: &[&str] = &[];

/// This build, as its handshake names it: the version and a hash of the
/// binary it runs from, `0.1.0+3f9c2a1b7d4e5f60` ([`paths::build_of`]). Two
/// builds of one version differ in it, which is how a server left running by
/// another build is told from this one (D54), and how the supervisor knows
/// which copies running workers use (D50).
///
/// A binary that cannot be read names no build that another could match.
pub fn build() -> String {
    static OWN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    OWN.get_or_init(|| {
        std::env::current_exe()
            .and_then(|exe| paths::build_of(&exe))
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "could not name this build");
                format!(
                    "{}+unreadable-{}",
                    env!("CARGO_PKG_VERSION"),
                    std::process::id()
                )
            })
    })
    .clone()
}
