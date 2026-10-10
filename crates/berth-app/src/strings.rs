//! Every user-visible string. Views never contain literals, so the whole
//! surface can be localised later without touching layout code.

pub const APP_NAME: &str = "Berth";

pub const FIRST_RUN_TITLE: &str = "No hosts configured";
pub const FIRST_RUN_BODY: &str =
    "Berth reads hosts from your ssh config and never writes to it. Add a Host entry, then reopen.";
pub const FIRST_RUN_PATH_LABEL: &str = "Looking for";
pub const FIRST_RUN_PATH_MISSING: &str = "not found";

pub const SIDEBAR_LOCAL: &str = "This machine";
pub const SIDEBAR_NEW_TERMINAL: &str = "+ New terminal";
pub const FOLDER_OPEN: &str = "\u{25be}";
pub const FOLDER_CLOSED: &str = "\u{25b8}";
/// Detach is not kill (D32): this is the row that proves it.
pub const SESSION_DETACHED_TOOLTIP: &str =
    "Detached. Still running on the server. Click to reattach in a new tab.";

/// What a local session is called everywhere a host would be named. Not a host
/// alias: a config with an entry called `local` must not collide with it.
pub const LOCAL_ALIAS: &str = "local";

/// On a local tab that runs a shell directly rather than on the local server
/// (D50), with why. Never a silent fallback.
pub fn local_direct_notice(reason: &str) -> String {
    format!("This session won't survive quitting: {reason}")
}
pub const LOCAL_DIRECT_BY_ENV: &str = "BERTH_LOCAL_DIRECT is set";
pub const LOCAL_DIRECT_BY_SETTING: &str = "local_direct is on in settings";
pub fn local_server_failed(error: &str) -> String {
    format!("the local server could not start ({error})")
}

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
/// The probe is in flight: Berth is asking the host for its key, and has not
/// authenticated to it.
pub const STATE_VERIFYING: &str = "Verifying";
pub const STATE_FAILED: &str = "Failed";

pub const VERIFY_TITLE: &str = "Trust this host key?";
pub const VERIFY_FINGERPRINT_LABEL: &str = "SHA256";
/// The only instruction that actually makes the answer meaningful. A
/// fingerprint nobody compares against anything is a dialog, not a check.
pub const VERIFY_HINT: &str = "Compare this against the server before accepting. On the host, \
     run: ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub";
pub const VERIFY_ACCEPT: &str = "Trust and connect";
pub const VERIFY_REJECT: &str = "Not now";
pub const VERIFY_RECORDS: &str =
    "Accepting appends it to your known_hosts, so ssh stops asking too.";

pub const RUNTIME_DROPPED: &str = "the runtime dropped the task";

pub fn verify_body(alias: &str, endpoint: &str) -> String {
    format!("{alias} ({endpoint}) is not in your known_hosts, so Berth has not connected to it.")
}

/// Never phrased as a question. There is nothing to accept here.
pub fn changed_key(line: usize, fingerprint: &str) -> String {
    format!(
        "Host key has CHANGED. known_hosts line {line} records a different key; \
         this host offered {fingerprint}. Berth will not connect."
    )
}

/// Shown on a forward's row, because this is where someone will be looking
/// when it confuses them.
pub const FORWARD_BIND_TOOLTIP: &str = "While Berth holds this port, a plain ssh to the same host will report \
     \"bind: Address already in use\" for it. Harmless: the forward is already up.";

/// Closing a tab and ending a session are different actions with different
/// costs, so they never share a word.
pub const TAB_DETACH: &str = "\u{00d7}";
pub const TAB_DETACH_TOOLTIP: &str = "Detach. The connection closes and the tab goes, but the tmux \
     session keeps running on the server with everything in it. Opening the \
     host again re-attaches to the same shell.";
/// A local shell has nothing running behind it once its window goes, so its
/// control says close rather than detach. Same glyph, honest word.
pub const TAB_CLOSE: &str = "\u{00d7}";
pub const TAB_CLOSE_TOOLTIP: &str =
    "Close. A local shell ends with its pane; there is nothing to reattach to.";
pub const TAB_KILL: &str = "\u{25a0}";
pub const TAB_KILL_TOOLTIP: &str = "End the session. Everything running in it stops and its scrollback \
     is gone. Asks first.";

/// Shown once, under an unsplit tab, because a split that nobody can find is
/// a feature that does not exist.
pub const SPLIT_HINT: &str =
    "Cmd+D splits right, Cmd+Shift+D splits down, Cmd+] moves between panes";

/// Says presence and nothing else. Berth does not know whether the agent is
/// working, waiting or finished, and a tooltip that implied otherwise would be
/// the first step towards pretending it does.
pub const AGENT_TOOLTIP: &str = "An agent is running in this session.";

/// Claude's state, from its own hooks (D48). Each says what Claude reported and
/// nothing Berth inferred.
pub const CLAUDE_WORKING: &str = "Claude is working.";
pub const CLAUDE_PERMISSION: &str = "Claude needs your permission.";
pub const CLAUDE_IDLE: &str = "Claude is waiting for your input.";
pub const CLAUDE_YOUR_MOVE: &str = "Claude finished its turn. Your move.";
pub const CLAUDE_FINISHED: &str = "Claude has exited.";
pub const CLAUDE_INTERRUPTED: &str = "Claude exited in the middle of a turn.";

/// What a program says of itself through the Program Status Protocol (OSC
/// 7501, D53). The program's own words, shown as it reported them: `who` is
/// the name it gave, and its message follows. Any program in the session can
/// report anything, so this says "reports", never that it is so.
pub const STATUS_ANONYMOUS: &str = "A program";

pub fn status_tooltip(
    who: &str,
    state: berth_proto::v1::StatusState,
    kind: Option<berth_proto::v1::StatusKind>,
    progress: Option<u8>,
    msg: Option<&str>,
) -> String {
    use berth_proto::v1::{StatusKind, StatusState};
    let mut text = match (state, kind) {
        (StatusState::Working, _) => match progress {
            Some(progress) => format!("{who} reports it is working ({progress}%)."),
            None => format!("{who} reports it is working."),
        },
        (StatusState::Blocked, Some(StatusKind::Permission)) => {
            format!("{who} reports it needs your permission.")
        }
        (StatusState::Blocked, Some(StatusKind::Question)) => {
            format!("{who} reports it has a question for you.")
        }
        (StatusState::Blocked, Some(StatusKind::Auth)) => {
            format!("{who} reports it needs you to sign in.")
        }
        (StatusState::Blocked, None) => format!("{who} reports it is waiting on you."),
        (StatusState::Done, _) => format!("{who} reports it has finished."),
        (StatusState::Error, _) => format!("{who} reports it has failed."),
        (StatusState::Idle, _) => format!("{who} reports it is waiting for input."),
    };
    if let Some(msg) = msg {
        text.push(' ');
        text.push_str(msg);
    }
    text
}

/// What the status mark on a tab means (D53).
pub const STATUS_NEEDS_YOU: &str = "Needs you.";
pub const STATUS_DONE: &str = "Finished.";
pub const STATUS_ERROR: &str = "Failed.";
pub const STATUS_WORKING: &str = "Working.";

/// Typed or pasted input the session's program did not take (D50).
pub const INPUT_NOT_DELIVERED: &str = "Input not delivered: the program isn't reading.";

pub const RENAME_TITLE: &str = "Name this session";
pub const RENAME_ACCEPT: &str = "Rename";
pub const RENAME_REJECT: &str = "Cancel";
pub const RENAME_BODY: &str =
    "Renames the tmux session on the server too, so reconnecting still finds it.";
pub const RENAME_BODY_LOCAL: &str =
    "A local session has no tmux behind it, so this is just a label.";

pub fn rename_taken(slug: &str, alias: &str) -> String {
    format!("{alias} already has a session called {slug}")
}

pub const RENAME_HOST_TITLE: &str = "Name this host";

pub fn rename_host_body(alias: &str) -> String {
    format!(
        "Shown in Berth instead of {alias}. Your ssh config is not changed. \
         Leave it empty to show {alias} again."
    )
}

pub const MENU_RENAME: &str = "Rename\u{2026}";
pub const MENU_DETACH: &str = "Detach";
pub const MENU_CLOSE: &str = "Close";
pub const MENU_END_SESSION: &str = "End session\u{2026}";
pub const MENU_OPEN: &str = "Open";
pub const MENU_NEW_TERMINAL: &str = "New terminal";
pub const MENU_CONNECT: &str = "Connect";
pub const MENU_RENAME_HOST: &str = "Rename\u{2026}";

pub const KILL_TITLE: &str = "End this session?";
pub const KILL_ACCEPT: &str = "End session";
pub const KILL_REJECT: &str = "Cancel";
pub const KILL_ALTERNATIVE: &str =
    "To close the tab and leave the session running, use detach instead.";

pub fn kill_body(alias: &str, session: &str) -> String {
    format!(
        "{session} on {alias} will be killed. Everything running in it stops \
         and its scrollback is gone. This cannot be undone."
    )
}

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
