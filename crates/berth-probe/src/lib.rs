//! The remote probe script and its typed parser.
//!
//! `scripts/probe.sh` is embedded at build time and pushed to `~/.berth/probe.sh`
//! on first connect. It is the only thing Berth installs on a server.

pub mod model;
pub mod poll;

pub use model::{Agent, AgentKind, Disk, Port, Session, SysInfo, Window};
pub use poll::{PollKind, PollSchedule};

use sha2::{Digest, Sha256};

/// The canonical probe, embedded so the binary and the script cannot drift.
pub const SCRIPT: &str = include_str!("../../../scripts/probe.sh");

/// Where the script lives on the remote.
pub const REMOTE_DIR: &str = "~/.berth";
pub const REMOTE_PATH: &str = "~/.berth/probe.sh";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("probe {mode}: malformed output: {source}")]
    Parse {
        mode: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("probe {mode} failed: {message}")]
    Remote { mode: &'static str, message: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// SHA-256 of the embedded script, shown in the UI before the first push and
/// compared against the remote copy to decide whether to re-push.
pub fn script_hash() -> String {
    hash_of(SCRIPT)
}

pub fn hash_of(text: &str) -> String {
    // Normalise line endings so a checkout on Windows pushes the same bytes.
    let normalised = text.replace("\r\n", "\n");
    hex::encode(Sha256::digest(normalised.as_bytes()))
}

/// The mode argument passed to the script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Sys,
    Ports,
    Sessions,
    Agents,
    Remove,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Sys => "sys",
            Mode::Ports => "ports",
            Mode::Sessions => "sessions",
            Mode::Agents => "agents",
            Mode::Remove => "remove",
        }
    }

    pub fn command(self) -> String {
        format!("sh {REMOTE_PATH} {}", self.as_str())
    }
}

/// Shell command that tails a file. The path is single-quoted, so a hostile
/// path cannot break out into another command.
pub fn tail_command(path: &str, lines: u32) -> String {
    format!(
        "sh {REMOTE_PATH} tail '{}' {lines}",
        path.replace('\'', r"'\''")
    )
}

/// Command that reports the remote script's hash, or `missing`, in one round
/// trip so a push can be skipped when the copy is current.
pub fn hash_command() -> String {
    format!(
        "if [ -r {REMOTE_PATH} ]; then (sha256sum {REMOTE_PATH} 2>/dev/null || shasum -a 256 {REMOTE_PATH}) | cut -d' ' -f1; else echo missing; fi"
    )
}

/// The exact bytes `push_command` causes to land on the remote.
///
/// CRLF is stripped because a checkout on Windows would otherwise push a script
/// whose shebang the remote sh cannot parse. Keeping this separate from the
/// command matters: [`script_hash`] must equal the remote's `sha256sum` of this
/// content, or Berth would decide the copy is stale and re-push on every
/// connect.
pub fn written_script(script: &str) -> String {
    let mut normalised = script.replace("\r\n", "\n").trim_end().to_string();
    normalised.push('\n');
    normalised
}

/// Command that writes the script to the remote in one exec channel.
pub fn push_command(script: &str) -> String {
    // A quoted heredoc delimiter stops the remote shell expanding the body, and
    // supplies the trailing newline that `written_script` accounts for.
    let body = written_script(script);
    format!(
        "mkdir -p {REMOTE_DIR} && cat > {REMOTE_PATH} <<'BERTH_EOF'\n{}BERTH_EOF\nchmod 700 {REMOTE_PATH}",
        body
    )
}

pub fn parse_sys(out: &str) -> Result<SysInfo> {
    serde_json::from_str(out.trim()).map_err(|source| Error::Parse {
        mode: "sys",
        source,
    })
}

pub fn parse_ports(out: &str) -> Result<Vec<Port>> {
    let wrapper: model::Ports =
        serde_json::from_str(out.trim()).map_err(|source| Error::Parse {
            mode: "ports",
            source,
        })?;
    Ok(wrapper.ports)
}

pub fn parse_sessions(out: &str) -> Result<Vec<Session>> {
    let wrapper: model::Sessions =
        serde_json::from_str(out.trim()).map_err(|source| Error::Parse {
            mode: "sessions",
            source,
        })?;
    Ok(wrapper.sessions)
}

pub fn parse_agents(out: &str) -> Result<Vec<Agent>> {
    let wrapper: model::Agents =
        serde_json::from_str(out.trim()).map_err(|source| Error::Parse {
            mode: "agents",
            source,
        })?;
    Ok(wrapper.agents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_is_embedded_and_hashable() {
        assert!(SCRIPT.contains("mode_sys"));
        assert_eq!(script_hash().len(), 64);
    }

    #[test]
    fn hash_ignores_line_ending_style() {
        assert_eq!(hash_of("a\nb\n"), hash_of("a\r\nb\r\n"));
    }

    #[test]
    fn script_stays_within_its_size_budget() {
        assert!(
            SCRIPT.lines().count() < 200,
            "probe.sh is {} lines",
            SCRIPT.lines().count()
        );
    }

    /// The payload stays inside single quotes: every embedded quote is closed,
    /// escaped and reopened, so no shell metacharacter is ever interpreted.
    #[test]
    fn tail_command_quotes_hostile_paths() {
        let cmd = tail_command("/var/log/a'; rm -rf /; '", 10);
        assert_eq!(
            cmd,
            r#"sh ~/.berth/probe.sh tail '/var/log/a'\''; rm -rf /; '\''' 10"#
        );
        // An odd number of quotes would let the payload escape.
        assert_eq!(cmd.matches('\'').count() % 2, 0, "{cmd}");
    }

    #[test]
    fn tail_command_leaves_ordinary_paths_alone() {
        assert_eq!(
            tail_command("/var/log/nginx/error.log", 500),
            "sh ~/.berth/probe.sh tail '/var/log/nginx/error.log' 500"
        );
    }

    #[test]
    fn push_command_uses_a_quoted_heredoc() {
        let cmd = push_command("echo $HOME");
        assert!(cmd.contains("<<'BERTH_EOF'"));
        assert!(cmd.contains("echo $HOME"));
    }

    /// A CRLF checkout would push a script the remote sh cannot run.
    #[test]
    fn pushed_script_never_contains_carriage_returns() {
        assert!(!push_command("a\r\nb\r\n").contains('\r'));
        assert!(!push_command(SCRIPT).contains('\r'));
    }

    /// Verified against a real host: the remote `sha256sum` of the pushed file
    /// equals `script_hash()`. If these ever diverge Berth re-pushes the script
    /// on every single connect, silently and forever.
    #[test]
    fn the_hash_matches_what_is_actually_written() {
        assert_eq!(script_hash(), hash_of(&written_script(SCRIPT)));
    }

    #[test]
    fn written_script_ends_in_exactly_one_newline() {
        for input in ["a", "a\n", "a\n\n\n", "a\r\n"] {
            let written = written_script(input);
            assert!(written.ends_with('\n'), "{input:?}");
            assert!(!written.ends_with("\n\n"), "{input:?}");
        }
    }

    #[test]
    fn parses_sys_output() {
        let json = r#"{"host":"web-1","uptime_s":551565,"load":[0.1,0.2,0.3],"mem_total":100,"mem_used":40,"disks":[{"mount":"/","total":10,"used":4}],"os":"Debian 12"}"#;
        let sys = parse_sys(json).unwrap();
        assert_eq!(sys.host, "web-1");
        assert_eq!(sys.load, [0.1, 0.2, 0.3]);
        assert_eq!(sys.disks[0].mount, "/");
        assert_eq!(sys.mem_used, 40);
    }

    #[test]
    fn parses_empty_collections() {
        assert!(parse_ports(r#"{"ports":[]}"#).unwrap().is_empty());
        assert!(parse_sessions(r#"{"sessions":[]}"#).unwrap().is_empty());
        assert!(parse_agents(r#"{"agents":[]}"#).unwrap().is_empty());
    }

    #[test]
    fn parses_sessions_with_windows() {
        let json = r#"{"sessions":[{"name":"berth/main","created":1700000000,"attached":true,"windows":[{"index":0,"cmd":"claude","pid":42}]}]}"#;
        let s = parse_sessions(json).unwrap();
        assert_eq!(s[0].name, "berth/main");
        assert!(s[0].attached);
        assert_eq!(s[0].windows[0].cmd, "claude");
    }

    #[test]
    fn parses_agent_kinds() {
        let json = r#"{"agents":[{"session":"berth/main","window":0,"kind":"claude-code","pid":1,"alive":true},{"session":"berth/x","window":1,"kind":"weird","pid":2,"alive":false}]}"#;
        let a = parse_agents(json).unwrap();
        assert_eq!(a[0].kind, AgentKind::ClaudeCode);
        assert_eq!(a[1].kind, AgentKind::Other, "unknown kinds fall back");
    }

    #[test]
    fn malformed_output_is_an_error_not_a_panic() {
        assert!(parse_sys("not json").is_err());
        assert!(parse_ports("").is_err());
    }
}
