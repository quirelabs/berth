//! Whether an agent is running in a session.
//!
//! Presence, never state (D34). This answers "something is running here" from
//! the foreground process name and nothing else. It never reads screen content,
//! it is the same one comparison for every agent, and the worst it can do when
//! it is wrong is fail to show an icon.
//!
//! The thing it deliberately does not do is report what the agent is doing.
//! Working, blocked and done need per-agent screen scraping that breaks every
//! time an agent changes its output, which is a permanent maintenance cost in
//! exchange for competing where the competition is strongest.

/// Process names that mean an agent. Matched on the command tmux reports, which
/// is the executable name rather than the full line.
///
/// Being wrong in either direction is cheap: a missing name is a missing icon,
/// and a wrong one is an icon on a shell. Neither is worth a heuristic.
const AGENTS: &[&str] = &[
    "aider",
    "amp",
    "claude",
    "cline",
    "codex",
    "crush",
    "cursor-agent",
    "gemini",
    "goose",
    "opencode",
];

/// One line of `list-panes` output: a session and what is running in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    /// The tmux session name, including the `berth/` prefix.
    pub session: String,
    pub command: String,
}

impl Running {
    pub fn is_agent(&self) -> bool {
        is_agent(&self.command)
    }
}

/// Whether a foreground process name is an agent.
pub fn is_agent(command: &str) -> bool {
    let name = command
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim_start_matches('-');
    AGENTS.contains(&name.to_ascii_lowercase().as_str())
}

/// Ask one host about every session at once.
///
/// One exec per connection rather than per session: a workspace of ten panes
/// polling separately would be ten channel opens every few seconds, for an
/// answer tmux can give in a single line each.
pub fn scan_command() -> String {
    "tmux list-panes -a -F '#{session_name}\t#{pane_current_command}'".to_string()
}

/// Parse what [`scan_command`] returned.
///
/// Anything unexpected is skipped rather than guessed at. This runs every few
/// seconds against a server whose tmux version is not ours to choose, so a
/// format that has moved must degrade to no icons rather than to wrong ones.
pub fn parse_scan(output: &str) -> Vec<Running> {
    output
        .lines()
        .filter_map(|line| {
            let (session, command) = line.trim_end().split_once('\t')?;
            let command = command.trim();
            if session.is_empty() || command.is_empty() {
                return None;
            }
            Some(Running {
                session: session.to_string(),
                command: command.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_agents_are_recognised() {
        for name in AGENTS {
            assert!(is_agent(name), "{name}");
        }
    }

    #[test]
    fn a_shell_is_not_an_agent() {
        for name in ["zsh", "bash", "fish", "sh", "vim", "nvim", "tmux", "htop"] {
            assert!(!is_agent(name), "{name}");
        }
    }

    /// tmux usually reports a bare name, but a path or a login shell's leading
    /// dash both turn up, and neither should hide an agent.
    #[test]
    fn a_path_or_a_login_dash_still_matches() {
        assert!(is_agent("/usr/local/bin/claude"));
        assert!(is_agent("-claude"));
        assert!(is_agent("CLAUDE"));
        assert!(!is_agent(""));
    }

    /// A name that merely contains an agent's is not one. `claude-wrapper` is
    /// somebody's script.
    #[test]
    fn a_partial_name_does_not_match() {
        assert!(!is_agent("claude-wrapper"));
        assert!(!is_agent("myamp"));
    }

    #[test]
    fn a_scan_is_parsed_into_sessions() {
        let output = "berth/main\tzsh\nberth/deploy\tclaude\nother\tvim\n";
        let running = parse_scan(output);
        assert_eq!(running.len(), 3);
        assert_eq!(running[0].session, "berth/main");
        assert!(!running[0].is_agent());
        assert_eq!(running[1].command, "claude");
        assert!(running[1].is_agent());
    }

    /// The format is not ours to rely on. A tmux that answers differently must
    /// produce no icons rather than wrong ones.
    #[test]
    fn unexpected_output_yields_nothing_rather_than_guesses() {
        for junk in [
            "",
            "no tabs here at all\n",
            "\t\n",
            "berth/main\t\n",
            "\tclaude\n",
            "error: no server running on /tmp/tmux-501/default\n",
        ] {
            assert!(parse_scan(junk).is_empty(), "{junk:?}");
        }
    }

    /// The command must name one session per line and never change anything.
    #[test]
    fn the_scan_command_only_reads() {
        let command = scan_command();
        assert!(command.starts_with("tmux list-panes"));
        assert!(command.contains("session_name"));
        assert!(command.contains("pane_current_command"));
        for destructive in ["kill", "rename", "new-session", "send-keys"] {
            assert!(!command.contains(destructive), "{command}");
        }
    }

    /// The separator has to reach tmux as a tab, not as a backslash and a `t`.
    /// tmux does no escape processing in `-F`, so the wrong one produces output
    /// that parses to nothing and an icon that never appears, with no error
    /// anywhere. Verified against tmux 3.4 on the test host.
    #[test]
    fn the_scan_separator_is_a_real_tab() {
        let command = scan_command();
        assert!(command.contains('\t'), "{command}");
        assert!(!command.contains("\\t"), "a literal backslash-t: {command}");
        // And the parser agrees on what it is splitting.
        assert_eq!(parse_scan("berth/main\tclaude").len(), 1);
    }
}
