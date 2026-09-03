//! tmux-backed interactive sessions.
//!
//! Every shell Berth opens is a tmux session named `berth/<slug>`, so a dropped
//! connection loses nothing: reconnecting re-attaches and tmux repaints.

/// Namespace for every session Berth creates, so `remove` can find them all.
pub const SESSION_PREFIX: &str = "berth/";
pub const DEFAULT_SLUG: &str = "main";
pub const TERM: &str = "xterm-256color";

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
    pub fn attach_command(&self) -> String {
        format!(
            "tmux -u new-session -A -s {} -x {} -y {}",
            shell_quote(&self.tmux_name()),
            self.cols,
            self.rows
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
    fn attach_command_matches_the_spec() {
        let s = Session::new("main", 120, 40);
        assert_eq!(
            s.attach_command(),
            "tmux -u new-session -A -s 'berth/main' -x 120 -y 40"
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
