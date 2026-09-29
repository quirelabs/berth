//! Every environment variable Berth reads, and the only place any of them is
//! spelled.
//!
//! They had been scattered across five files at nine call sites, with no list
//! anywhere and no way to answer "what can I set" except by grepping. Worse,
//! two of them silently change where Berth writes: `BERTH_KNOWN_HOSTS` decides
//! which file a trusted key is appended to, and `BERTH_SSH_CONFIG` decides
//! which hosts exist at all. Anything with that reach gets one door.
//!
//! | Name                 | Effect                                          |
//! |----------------------|-------------------------------------------------|
//! | `BERTH_SSH_CONFIG`   | read hosts from this file, not `~/.ssh/config`  |
//! | `BERTH_KNOWN_HOSTS`  | trust and record keys here, not `~/.ssh/known_hosts` |
//! | `BERTH_SETTINGS`     | remember settings here, not in the config directory |
//! | `BERTH_RESTORE`      | save local sessions here, not in the data directory |
//! | `BERTH_DATA`         | keep the Claude plugin and shim here, not in the data directory |
//! | `BERTH_SHELL`        | run this as the local shell                     |
//! | `BERTH_SESSION`      | the tmux session slug, instead of the default   |
//! | `BERTH_HOST`         | open this alias at startup                      |
//! | `BERTH_BENCH`        | run a benchmark instead of the workspace        |
//! | `BERTH_BENCH_SETTLE` | seconds the cat benchmark waits before measuring |
//! | `BERTH_REFRESH_HZ`   | the display's refresh rate, for the frame budget |
//! | `BERTH_PIXEL_CHECK`  | run the pixel test, which needs a window server |
//! | `BERTH_TEST_HOST`    | the alias the live suites connect to            |
//! | `BERTH_TEST_CLAUDE`  | `1` runs the live Claude Code suite, which spends a few tokens |
//!
//! The last three are read by test harnesses rather than by Berth, and are named
//! here so the list is the whole list.
//!
//! Several more go the other way: Berth sets them in the panes it spawns and
//! never reads them itself (D48). `BERTH_PANE` is the pane's token, which the
//! Claude hook echoes back; `BERTH_TTY` is the pane's terminal device, where
//! the hook writes outside tmux; `BERTH_CLAUDE_SHIM` lists the shim
//! directories a `claude` call has already passed through, so shims never loop
//! or add the plugin twice. `BERTH_SHIM_DIR`, `BERTH_USER_ZDOTDIR` and
//! `BERTH_ZDOTDIR_WAS_SET` are read by Berth's shell startup wrappers, to load
//! the user's own files and then put the shim back in front, and
//! `BERTH_RESUME` tells a wrapper which Claude conversation to resume before
//! the first prompt.
//!
//! Berth also reads variables it does not own: `SHELL` and `ComSpec` for the
//! local shell, `USER` and `USERNAME` when ssh config names no user. Those stay
//! where they are used, because they are the platform's answer to a question
//! asked in one place, not Berth's own settings.
//!
//! Nothing is cached. The fixture-driven ones are set per test, and freezing
//! them at startup would break every test that points at a `/tmp` fixture.

use std::path::PathBuf;

pub const SSH_CONFIG: &str = "BERTH_SSH_CONFIG";
pub const KNOWN_HOSTS: &str = "BERTH_KNOWN_HOSTS";
pub const SETTINGS: &str = "BERTH_SETTINGS";
pub const RESTORE: &str = "BERTH_RESTORE";
pub const DATA: &str = "BERTH_DATA";
pub const PANE: &str = "BERTH_PANE";
pub const TTY: &str = "BERTH_TTY";
pub const CLAUDE_SHIM: &str = "BERTH_CLAUDE_SHIM";
pub const SHIM_DIR: &str = "BERTH_SHIM_DIR";
pub const USER_ZDOTDIR: &str = "BERTH_USER_ZDOTDIR";
pub const ZDOTDIR_WAS_SET: &str = "BERTH_ZDOTDIR_WAS_SET";
pub const RESUME: &str = "BERTH_RESUME";
pub const SHELL: &str = "BERTH_SHELL";
pub const SESSION: &str = "BERTH_SESSION";
pub const HOST: &str = "BERTH_HOST";
pub const BENCH: &str = "BERTH_BENCH";
pub const BENCH_SETTLE: &str = "BERTH_BENCH_SETTLE";
pub const REFRESH_HZ: &str = "BERTH_REFRESH_HZ";
pub const PIXEL_CHECK: &str = "BERTH_PIXEL_CHECK";
pub const TEST_HOST: &str = "BERTH_TEST_HOST";
pub const TEST_CLAUDE: &str = "BERTH_TEST_CLAUDE";

/// Every name above, so the test below can hold them to one shape.
#[cfg(test)]
const NAMES: &[&str] = &[
    SSH_CONFIG,
    KNOWN_HOSTS,
    SETTINGS,
    RESTORE,
    DATA,
    PANE,
    TTY,
    CLAUDE_SHIM,
    SHIM_DIR,
    USER_ZDOTDIR,
    ZDOTDIR_WAS_SET,
    RESUME,
    SHELL,
    SESSION,
    HOST,
    BENCH,
    BENCH_SETTLE,
    REFRESH_HZ,
    PIXEL_CHECK,
    TEST_HOST,
    TEST_CLAUDE,
];

/// Unset and empty mean the same thing.
///
/// `BERTH_SESSION=` in a shell profile is somebody meaning "not set", and an
/// empty value would otherwise become an empty tmux session name or a path that
/// cannot open, several layers away from the mistake.
fn present(raw: Option<String>) -> Option<String> {
    raw.filter(|value| !value.trim().is_empty())
}

fn text(name: &str) -> Option<String> {
    present(std::env::var(name).ok())
}

fn path(name: &str) -> Option<PathBuf> {
    text(name).map(PathBuf::from)
}

fn number<T: std::str::FromStr>(name: &str) -> Option<T> {
    text(name)?.trim().parse().ok()
}

/// Read hosts from here rather than from the user's own ssh config, which Berth
/// has no business depending on the contents of during a test.
pub fn ssh_config() -> Option<PathBuf> {
    path(SSH_CONFIG)
}

/// Trust and record keys here. A test must never write to the user's own.
pub fn known_hosts() -> Option<PathBuf> {
    path(KNOWN_HOSTS)
}

/// Remember settings here rather than in the platform's config directory, so a
/// test never writes into the user's own.
pub fn settings() -> Option<PathBuf> {
    path(SETTINGS)
}

/// Save local sessions here rather than in the platform's data directory, so a
/// test never writes into the user's own.
pub fn restore() -> Option<PathBuf> {
    path(RESTORE)
}

/// Keep the Claude plugin and shim here rather than in the platform's data
/// directory, so a test never writes into the user's own.
pub fn data() -> Option<PathBuf> {
    path(DATA)
}

/// The local shell to run. First choice, ahead of the platform's own answer,
/// so a settings page has something to set.
pub fn shell() -> Option<String> {
    text(SHELL)
}

/// The tmux session slug to use instead of the default.
pub fn session() -> Option<String> {
    text(SESSION)
}

/// An alias to open at startup.
pub fn host() -> Option<String> {
    text(HOST)
}

/// Which benchmark to run instead of the workspace. Never set in normal use.
pub fn bench() -> Option<String> {
    text(BENCH)
}

/// Seconds the cat benchmark waits for the transfer to settle before it starts
/// measuring frames.
pub fn bench_settle_secs() -> Option<u64> {
    number(BENCH_SETTLE)
}

/// The display's refresh rate. Overridable because the frame budget is derived
/// from it and no API reports it reliably on every platform.
pub fn refresh_hz() -> Option<f64> {
    number(REFRESH_HZ)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_value_reads_as_unset() {
        assert_eq!(present(None), None);
        assert_eq!(present(Some(String::new())), None);
        assert_eq!(present(Some("  ".into())), None);
        assert_eq!(present(Some("berth".into())), Some("berth".to_string()));
    }

    /// The prefix is the whole convention: anything Berth reads is obviously
    /// Berth's, and nothing here can collide with a variable someone else owns.
    #[test]
    fn every_name_is_prefixed_and_unique() {
        for name in NAMES {
            assert!(name.starts_with("BERTH_"), "{name}");
        }
        let mut sorted = NAMES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), NAMES.len(), "a name is listed twice");
    }
}
