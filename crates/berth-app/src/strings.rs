//! Every user-visible string. Views never contain literals, so the whole
//! surface can be localised later without touching layout code.

pub const APP_NAME: &str = "Berth";

pub const FIRST_RUN_TITLE: &str = "No hosts configured";
pub const FIRST_RUN_BODY: &str =
    "Berth reads hosts from your ssh config and never writes to it. Add a Host entry, then reopen.";
pub const FIRST_RUN_PATH_LABEL: &str = "Looking for";
pub const FIRST_RUN_PATH_MISSING: &str = "not found";

pub const SIDEBAR_HOSTS: &str = "Hosts";

pub const STATE_DISCONNECTED: &str = "Disconnected";
pub const STATE_CONNECTING: &str = "Connecting";
pub const STATE_CONNECTED: &str = "Connected";
pub const STATE_DEGRADED: &str = "Degraded";
pub const STATE_RECONNECTING: &str = "Reconnecting";
/// Not an error and not an absence: a host Berth deliberately did not warm,
/// because doing so would have raised a trust prompt at launch.
pub const STATE_VERIFY: &str = "Verify";

/// The row is short; the reason lives in the tooltip, where someone will look
/// once the one-word label has made them curious.
pub const VERIFY_TOOLTIP: &str = "Not connected at startup because this host is not in known_hosts, and \
     warming it would have asked you to trust a key before you asked for it. \
     Open this host to see the fingerprint and decide.";
pub const STATE_FAILED: &str = "Failed";

/// Shown on a forward's row, because this is where someone will be looking
/// when it confuses them.
pub const FORWARD_BIND_TOOLTIP: &str = "While Berth holds this port, a plain ssh to the same host will report \
     \"bind: Address already in use\" for it. Harmless: the forward is already up.";

pub const STATUS_NO_HOST: &str = "No host";
pub const STATUS_NOT_RECORDING: &str = "Not recording";

use berth_core::connection::ConnectionState;

pub fn connection_state(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Disconnected => STATE_DISCONNECTED,
        ConnectionState::Connecting => STATE_CONNECTING,
        ConnectionState::Connected => STATE_CONNECTED,
        ConnectionState::Degraded => STATE_DEGRADED,
        ConnectionState::Reconnecting => STATE_RECONNECTING,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_connection_state_has_a_label() {
        for state in [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::Connected,
            ConnectionState::Degraded,
            ConnectionState::Reconnecting,
        ] {
            assert!(!connection_state(state).is_empty());
        }
    }
}
