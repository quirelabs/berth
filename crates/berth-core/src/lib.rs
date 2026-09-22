//! Transport, configuration and session lifecycle. No UI dependencies.

pub mod agent;
pub mod agents;
pub mod config;
pub mod connection;
pub mod env;
pub mod error;
pub mod forward;
pub mod host;
pub mod known_hosts;
pub mod local;
pub mod pty;
pub mod restore;
pub mod session;
pub mod settings;
pub mod transport;

pub use error::{Error, Result};
pub use host::Host;
pub use transport::Connection;
