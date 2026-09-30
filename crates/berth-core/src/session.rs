//! tmux-backed interactive sessions.
//!
//! Every shell Berth opens is a tmux session named `berth/<slug>`, so a dropped
//! connection loses nothing: reconnecting re-attaches and tmux repaints.

/// Namespace for every session Berth creates, so `remove` can find them all.
pub const SESSION_PREFIX: &str = "berth/";
pub const DEFAULT_SLUG: &str = "main";
/// What Berth's terminal says it is (D51). xterm-256color because every host
/// has its terminfo; Ghostty's own `xterm-ghostty` would follow the user into
/// nested ssh sessions on hosts without it, where programs then fail with
/// "unknown terminal".
pub const TERM: &str = "xterm-256color";
/// The engine draws 24-bit colour, and terminfo has no reliable way to say so.
pub const COLORTERM: &str = "truecolor";
/// How a program can tell it is in Berth, as `TERM` no longer can.
pub const TERM_PROGRAM: &str = "Berth";
pub const TERM_PROGRAM_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub slug: String,
    pub cols: u16,
    pub rows: u16,
}

impl Session {
    pub fn new(slug: impl Into<String>, cols: u16, rows: u16) -> Self {
        Self {
            slug: sanitise_slug(&slug.into()),
            cols: cols.max(1),
            rows: rows.max(1),
        }
    }

    pub fn tmux_name(&self) -> String {
        format!("{SESSION_PREFIX}{}", self.slug)
    }

    /// `new-session -A` attaches if the session exists and creates it if not,
    /// which makes reconnect and first connect the same code path.
    ///
    /// `allow-passthrough` is set on this session first, because tmux drops
    /// OSC sequences it does not recognise and shell integration markers
    /// otherwise never reach Berth at all (measured on tmux 3.4). The option is
    /// set per session rather than globally, so nothing about the user's other
    /// tmux sessions changes, and it disappears with the session.
    pub fn attach_command(&self) -> String {
        let name = shell_quote(&self.tmux_name());
        format!(
            "tmux -u new-session -A -s {name} -x {} -y {} \\; \
             set-option -t {name} allow-passthrough on",
            self.cols, self.rows
        )
    }

    /// End the session and everything running in it.
    ///
    /// Never reached without an explicit confirmation. Closing a tab detaches,
    /// which is reversible and loses nothing; this is the other thing, and the
    /// two are kept as separate commands so no code path can reach this one by
    /// meaning the other.
    pub fn kill_command(&self) -> String {
        format!("tmux kill-session -t {}", shell_quote(&self.tmux_name()))
    }

    /// Rename this session on the server.
    ///
    /// The tmux name is what makes a reattach land in the same shell, so a
    /// rename that changed only Berth's label would silently break reconnect:
    /// the next attach would create a second session under the old name and
    /// the running one would be orphaned. Both move together or neither does.
    pub fn rename_command(&self, to: &Session) -> String {
        format!(
            "tmux rename-session -t {} {}",
            shell_quote(&self.tmux_name()),
            shell_quote(&to.tmux_name())
        )
    }

    /// Fallback when the server has no tmux. The UI shows a banner offering to
    /// install it; Berth never installs anything itself.
    pub fn plain_shell_command() -> &'static str {
        "exec \"$SHELL\" -l"
    }
}

/// tmux session names cannot contain `.` or `:`, and whitespace makes the
/// listing format ambiguous.
fn sanitise_slug(slug: &str) -> String {
    let cleaned: String = slug
        .chars()
        .map(|c| match c {
            '.' | ':' | '\t' | '\n' | ' ' => '-',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        DEFAULT_SLUG.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Single-quote for POSIX sh, escaping embedded quotes.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_moves_one_session_to_another_name() {
        let from = Session::new("main", 80, 24);
        let to = Session::new("deploy", 80, 24);
        assert_eq!(
            from.rename_command(&to),
            "tmux rename-session -t 'berth/main' 'berth/deploy'"
        );
    }

    /// Both ends are user input reaching a shell, and the target is the half
    /// that is easy to forget.
    #[test]
    fn rename_quotes_both_names() {
        let from = Session::new("main", 80, 24);
        let to = Session::new("x'; rm -rf /; echo '", 80, 24);
        let command = from.rename_command(&to);
        assert!(!command.contains("rm -rf /;"), "{command}");
    }

    /// A rename stays inside Berth's namespace, so it can never take over a
    /// session the user runs by hand.
    #[test]
    fn rename_cannot_leave_the_berth_namespace() {
        let from = Session::new("main", 80, 24);
        let to = Session::new("../escape", 80, 24);
        let command = from.rename_command(&to);
        let target = command.rsplit(" -t ").next().unwrap();
        assert_eq!(target.matches(SESSION_PREFIX).count(), 2, "{command}");
        assert!(!command.contains(".."), "{command}");
    }

    #[test]
    fn kill_names_only_this_session() {
        let s = Session::new("main", 80, 24);
        let command = s.kill_command();
        assert_eq!(command, "tmux kill-session -t 'berth/main'");
        // Anything that could end more than the one session named here.
        assert!(!command.contains("kill-server"), "{command}");
        assert!(!command.contains("-a"), "{command}");
    }

    /// A slug is user input and reaches a shell, so it stays quoted.
    #[test]
    fn kill_quotes_a_hostile_slug() {
        let s = Session::new("main'; rm -rf /; echo '", 80, 24);
        let command = s.kill_command();
        assert!(!command.contains("rm -rf /;"), "{command}");
        assert!(
            command.starts_with("tmux kill-session -t 'berth/"),
            "{command}"
        );
    }

    #[test]
    fn attach_command_matches_the_spec() {
        let s = Session::new("main", 120, 40);
        let command = s.attach_command();
        assert!(
            command.starts_with("tmux -u new-session -A -s 'berth/main' -x 120 -y 40"),
            "{command}"
        );
    }

    /// Without this, tmux swallows the shell integration markers entirely.
    #[test]
    fn the_attach_enables_passthrough_for_this_session_only() {
        let command = Session::new("main", 80, 24).attach_command();
        assert!(command.contains("allow-passthrough on"), "{command}");
        assert!(
            command.contains("-t 'berth/main'"),
            "must target this session, not global: {command}"
        );
        assert!(
            !command.contains("set-option -g"),
            "must not change the user's global tmux options: {command}"
        );
    }

    #[test]
    fn slug_is_sanitised() {
        assert_eq!(Session::new("my.session:1", 80, 24).slug, "my-session-1");
        assert_eq!(Session::new("   ", 80, 24).slug, DEFAULT_SLUG);
        assert_eq!(Session::new("--", 80, 24).slug, DEFAULT_SLUG);
    }

    #[test]
    fn quoting_survives_an_embedded_quote() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn dimensions_are_never_zero() {
        let s = Session::new("main", 0, 0);
        assert_eq!((s.cols, s.rows), (1, 1));
    }
}
