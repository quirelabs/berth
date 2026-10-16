//! Asking a host what it is, before anything is offered (D55).
//!
//! One exec of a small script, sent on standard input so no shell has to be
//! trusted with its quoting. It changes nothing on the host. What comes back
//! is the host's own account of itself and is read as such: every field is
//! checked before it is used to build a path or a command.

use super::manifest::Target;
use super::{Runs, quoted};
use crate::server::paths;

pub const SCRIPT: &str = include_str!("probe.sh");

/// The tool a host has for checking a SHA-256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashTool {
    Sha256sum,
    Shasum,
}

impl HashTool {
    /// The command that prints a file's SHA-256 first on its line.
    pub fn command(self) -> &'static str {
        match self {
            HashTool::Sha256sum => "sha256sum",
            HashTool::Shasum => "shasum -a 256",
        }
    }
}

/// What a host said of itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// `uname -s` and `uname -m`, as given.
    pub os: String,
    pub arch: String,
    /// The server target for that, when there is one.
    pub target: Option<Target>,
    pub home: String,
    /// Berth's data directory there: `~/Library/Application Support/berth-host`
    /// on macOS, never the one the Berth app's own local server uses there;
    /// on Linux `$XDG_DATA_HOME/berth`, else `~/.local/share/berth`.
    pub data: String,
    /// The builds installed there.
    pub builds: Vec<String>,
    /// `None` when the host has neither tool: the hash cannot be checked
    /// there, and the user is told so.
    pub hash_tool: Option<HashTool>,
    pub gzip: bool,
    /// Whether logind ends the user's processes when their last login goes
    /// (`KillUserProcesses`), which ends sessions on a disconnect. `None`
    /// when the host could not say.
    pub kills_at_logout: Option<bool>,
    pub linger: Option<bool>,
    /// Sessions running on the server there, by its pid files.
    pub sessions: usize,
}

impl Probe {
    /// Whether sessions there would end when the connection does.
    pub fn sessions_end_at_logout(&self) -> bool {
        self.kills_at_logout == Some(true) && self.linger != Some(true)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The command ran and what came back was not the probe's whole answer:
    /// no `sh`, or a shell that printed something else instead.
    #[error("the host did not answer the probe: {0}")]
    NoAnswer(String),
    #[error("{0}")]
    Connection(String),
}

/// The command that runs the probe, its script on standard input. `data`
/// overrides the data directory, for tests under a scratch directory.
pub fn command(data: Option<&str>) -> String {
    match data {
        Some(data) => format!("sh -s -- {}", quoted(data)),
        None => "sh -s".to_string(),
    }
}

/// Probe `host`.
pub async fn probe(host: &impl Runs, data: Option<&str>) -> Result<Probe, Error> {
    let out = host
        .run(&command(data), SCRIPT.as_bytes())
        .await
        .map_err(|e| Error::Connection(e.to_string()))?;
    parse(&out.stdout_string()).ok_or_else(|| {
        let said = format!("{}{}", out.stdout_string(), out.stderr_string());
        Error::NoAnswer(said.chars().take(200).collect())
    })
}

fn yes_no(value: &str) -> Option<bool> {
    match value {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

/// Read the script's answer. Lines before its first line are skipped: a
/// shell's startup files may print. Without its last line the answer is not
/// whole, and is not used.
pub fn parse(text: &str) -> Option<Probe> {
    let mut lines = text
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .skip_while(|line| *line != "berth-probe=1")
        .skip(1);
    let mut probe = Probe {
        os: String::new(),
        arch: String::new(),
        target: None,
        home: String::new(),
        data: String::new(),
        builds: Vec::new(),
        hash_tool: None,
        gzip: false,
        kills_at_logout: None,
        linger: None,
        sessions: 0,
    };
    let mut whole = false;
    for line in &mut lines {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "os" => probe.os = value.to_string(),
            "arch" => probe.arch = value.to_string(),
            "home" => probe.home = value.to_string(),
            "data" => probe.data = value.to_string(),
            // Used as a directory name: only what is shaped like one.
            "build" if paths::is_build(value) => probe.builds.push(value.to_string()),
            "sha" => {
                probe.hash_tool = match value {
                    "sha256sum" => Some(HashTool::Sha256sum),
                    "shasum" => Some(HashTool::Shasum),
                    _ => None,
                }
            }
            "gzip" => probe.gzip = value == "yes",
            "kills" => probe.kills_at_logout = yes_no(value),
            "linger" => probe.linger = yes_no(value),
            "sessions" => probe.sessions = value.parse().unwrap_or(0),
            "berth-probe" if value == "end" => {
                whole = true;
                break;
            }
            _ => {}
        }
    }
    if !whole || probe.os.is_empty() || probe.data.is_empty() {
        return None;
    }
    probe.target = Target::for_host(&probe.os, &probe.arch);
    Some(probe)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = "Welcome to the machine\n\
        berth-probe=1\n\
        os=Linux\n\
        arch=x86_64\n\
        home=/home/u\n\
        data=/home/u/.local/share/berth\n\
        build=0.1.0+0123456789abcdef\n\
        build=../../etc\n\
        sha=sha256sum\n\
        gzip=yes\n\
        kills=yes\n\
        linger=no\n\
        sessions=3\n\
        berth-probe=end\n";

    #[test]
    fn an_answer_is_read_past_what_a_shell_printed_first() {
        let probe = parse(ANSWER).unwrap();
        assert_eq!(probe.target, Some(Target::LinuxX86_64));
        assert_eq!(probe.data, "/home/u/.local/share/berth");
        assert_eq!(
            probe.builds,
            ["0.1.0+0123456789abcdef"],
            "only real build names"
        );
        assert_eq!(probe.hash_tool, Some(HashTool::Sha256sum));
        assert!(probe.gzip);
        assert_eq!(probe.sessions, 3);
        assert!(probe.sessions_end_at_logout());
    }

    #[test]
    fn lingering_keeps_sessions_where_logout_would_end_them() {
        let mut probe = parse(ANSWER).unwrap();
        probe.linger = Some(true);
        assert!(!probe.sessions_end_at_logout());
        probe.linger = Some(false);
        probe.kills_at_logout = None;
        assert!(
            !probe.sessions_end_at_logout(),
            "not known is not a warning"
        );
    }

    /// Cut short, or not the probe at all: no answer, rather than half of one.
    #[test]
    fn half_an_answer_is_no_answer() {
        let cut = ANSWER.replace("berth-probe=end\n", "");
        assert_eq!(parse(&cut), None);
        assert_eq!(parse("sh: command not found\n"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn a_host_without_the_tools_says_so() {
        let answer = ANSWER
            .replace("sha=sha256sum", "sha=none")
            .replace("gzip=yes", "gzip=no")
            .replace("kills=yes", "kills=unknown")
            .replace("os=Linux", "os=OpenBSD");
        let probe = parse(&answer).unwrap();
        assert_eq!(probe.hash_tool, None);
        assert!(!probe.gzip);
        assert_eq!(probe.kills_at_logout, None);
        assert_eq!(probe.target, None);
        assert_eq!(probe.os, "OpenBSD");
    }

    /// The script itself, run here: it answers whole, names this machine,
    /// finds a build put where one would be installed, and changes nothing.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_script_answers_on_this_machine_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("berth data");
        let build = "0.1.0+0123456789abcdef";
        let installed = data.join("bin").join(build);
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(installed.join("berth-server"), b"#!/bin/sh\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let exec = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(installed.join("berth-server"), exec).unwrap();
        }
        // Not a build: a directory with no server in it.
        std::fs::create_dir_all(data.join("bin").join("0.1.0+ffffffffffffffff")).unwrap();
        let before = std::fs::read_dir(&data).unwrap().count();

        let probe = probe(&super::super::ThisMachine, data.to_str())
            .await
            .unwrap();
        assert_eq!(probe.data, data.to_str().unwrap());
        assert_eq!(probe.builds, [build]);
        assert!(probe.target.is_some(), "{probe:?}");
        assert!(probe.hash_tool.is_some());
        assert!(probe.gzip);
        assert_eq!(probe.sessions, 0);
        assert_eq!(std::fs::read_dir(&data).unwrap().count(), before);
        assert!(!data.join("run").exists());
    }

    /// Without a directory given, the host's own: under the home it names,
    /// and on a Mac never the directory the app's local server uses.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_data_directory_is_the_platforms_when_none_is_given() {
        let probe = probe(&super::super::ThisMachine, None).await.unwrap();
        if cfg!(target_os = "macos") {
            let own = format!("{}/Library/Application Support/berth-host", probe.home);
            assert_eq!(probe.data, own);
        } else {
            assert!(probe.data.starts_with(&probe.home) || std::env::var("XDG_DATA_HOME").is_ok());
            assert!(probe.data.ends_with("/berth"), "{}", probe.data);
        }
    }
}
