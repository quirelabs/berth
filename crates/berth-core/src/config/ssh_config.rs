//! Reader for `~/.ssh/config`. Berth never writes to this file.
//!
//! Supported: `Host` patterns, `HostName`, `Port`, `User`, `ProxyJump`,
//! `IdentityAgent`, and `Include`. `Match` blocks are skipped with a warning.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::host::Host;

#[derive(Debug, Default, Clone)]
pub struct SshConfig {
    pub hosts: Vec<Host>,
    /// Directives Berth recognised but chose to ignore, surfaced in the UI.
    pub warnings: Vec<String>,
}

/// One `Host` stanza before pattern expansion.
#[derive(Debug, Clone, Default)]
struct Stanza {
    patterns: Vec<String>,
    hostname: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    proxy_jump: Option<String>,
    identity_agent: Option<String>,
    forwards: Vec<crate::forward::Forward>,
    group: Option<String>,
}

/// Every directive Berth or ssh might see at the start of a line.
///
/// Used to tell a heading from a commented-out directive, because `# Host old`
/// is disabled configuration and naming a group after it would be nonsense.
const DIRECTIVES: &[&str] = &[
    "host",
    "hostname",
    "port",
    "user",
    "proxyjump",
    "proxycommand",
    "identityagent",
    "identityfile",
    "localforward",
    "remoteforward",
    "match",
    "include",
    "forwardagent",
    "serveraliveinterval",
];

/// The longest a comment can be and still be a heading rather than a note.
const HEADING_MAX: usize = 40;

/// A comment that names a group, or `None` if it is an ordinary remark.
fn heading(comment: &str) -> Option<String> {
    let text = comment.trim().trim_end_matches(':').trim();
    if text.is_empty() || text.len() > HEADING_MAX {
        return None;
    }
    // A row of dashes or equals signs is a separator, not a name.
    if !text.chars().any(char::is_alphanumeric) {
        return None;
    }
    let first = text.split_whitespace().next()?.to_ascii_lowercase();
    if DIRECTIVES.contains(&first.as_str()) {
        return None;
    }
    Some(text.to_string())
}

impl SshConfig {
    pub fn default_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".ssh").join("config"))
    }

    /// A missing file is not an error; it yields an empty config, which drives
    /// the first-run experience.
    pub fn load_default() -> Result<Self> {
        match Self::default_path() {
            Some(p) if p.exists() => Self::load(&p),
            _ => Ok(Self::default()),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let mut out = SshConfig::default();
        let mut seen = HashSet::new();
        out.load_into(path, &mut seen, 0)?;
        Ok(out)
    }

    fn load_into(&mut self, path: &Path, seen: &mut HashSet<PathBuf>, depth: usize) -> Result<()> {
        if depth > 16 {
            self.warnings
                .push(format!("Include nesting too deep at {}", path.display()));
            return Ok(());
        }
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !seen.insert(canonical) {
            return Ok(()); // Include cycle
        }
        let text = std::fs::read_to_string(path).map_err(|e| Error::SshConfig {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        self.parse_text(&text, path, seen, depth);
        Ok(())
    }

    fn parse_text(&mut self, text: &str, path: &Path, seen: &mut HashSet<PathBuf>, depth: usize) {
        let mut current: Option<Stanza> = None;
        let mut in_match = false;
        let mut group: Option<String> = None;

        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(comment) = line.strip_prefix('#') {
                // A comment heading names the group for the hosts under it.
                // People already write their ssh config this way, so groups
                // cost no new file and no new syntax. A comment that does not
                // look like a heading leaves the current group alone.
                if let Some(heading) = heading(comment) {
                    group = Some(heading);
                }
                continue;
            }
            let Some((key, value)) = split_directive(line) else {
                continue;
            };
            let key_lower = key.to_ascii_lowercase();

            match key_lower.as_str() {
                "host" => {
                    in_match = false;
                    if let Some(s) = current.take() {
                        self.push_stanza(s);
                    }
                    current = Some(Stanza {
                        patterns: value.split_whitespace().map(str::to_string).collect(),
                        group: group.clone(),
                        ..Default::default()
                    });
                }
                "match" => {
                    if let Some(s) = current.take() {
                        self.push_stanza(s);
                    }
                    in_match = true;
                    self.warnings.push(format!(
                        "skipped unsupported `Match {value}` in {}",
                        path.display()
                    ));
                }
                "include" => {
                    for pattern in value.split_whitespace() {
                        self.include(pattern, path, seen, depth);
                    }
                }
                _ if in_match => {}
                _ => {
                    // Directives before any Host line are global defaults, which
                    // Berth does not apply.
                    let Some(stanza) = current.as_mut() else {
                        continue;
                    };
                    match key_lower.as_str() {
                        "hostname" => stanza.hostname = Some(value.to_string()),
                        "port" => stanza.port = value.parse().ok(),
                        "user" => stanza.user = Some(value.to_string()),
                        "proxyjump" => stanza.proxy_jump = Some(value.to_string()),
                        "identityagent" => stanza.identity_agent = Some(value.to_string()),
                        // Repeatable, unlike most directives, so these
                        // accumulate rather than overwrite. An unparseable one
                        // is dropped rather than guessed at.
                        "localforward" => {
                            if let Some(forward) = crate::forward::parse_local_forward(value) {
                                stanza.forwards.push(forward);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some(s) = current.take() {
            self.push_stanza(s);
        }
    }

    fn include(&mut self, pattern: &str, parent: &Path, seen: &mut HashSet<PathBuf>, depth: usize) {
        let expanded = shellexpand::tilde(pattern).into_owned();
        let mut candidate = PathBuf::from(&expanded);
        if candidate.is_relative() {
            // OpenSSH resolves relative includes against the containing directory.
            let base = parent.parent().map(Path::to_path_buf).unwrap_or_default();
            candidate = base.join(&candidate);
        }
        let matches = expand_glob(&candidate);
        if matches.is_empty() {
            self.warnings
                .push(format!("Include {pattern} matched nothing"));
        }
        for m in matches {
            if let Err(e) = self.load_into(&m, seen, depth + 1) {
                self.warnings.push(e.to_string());
            }
        }
    }

    fn push_stanza(&mut self, stanza: Stanza) {
        for pattern in &stanza.patterns {
            // Wildcard stanzas set defaults for other hosts; they are not
            // connectable targets, so they never reach the sidebar.
            if pattern.contains('*') || pattern.contains('?') || pattern.starts_with('!') {
                continue;
            }
            let hostname = stanza.hostname.clone().unwrap_or_else(|| pattern.clone());
            let mut host = Host::new(pattern.clone(), hostname);
            host.port = stanza.port.unwrap_or(22);
            host.user = stanza.user.clone();
            host.proxy_jump = stanza.proxy_jump.clone();
            host.identity_agent = stanza.identity_agent.clone();
            host.forwards = stanza.forwards.clone();
            host.group = stanza.group.clone();
            self.hosts.push(host);
        }
    }

    pub fn get(&self, alias: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.alias == alias)
    }
}

/// Split `Key value` or `Key=value`, tolerating the whitespace both allow.
fn split_directive(line: &str) -> Option<(&str, &str)> {
    let key_end = line
        .bytes()
        .position(|b| b.is_ascii_whitespace() || b == b'=')?;
    let key = &line[..key_end];
    let value = line[key_end..]
        .trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '=')
        .trim();
    if key.is_empty() {
        None
    } else {
        Some((key, value))
    }
}

/// Narrow glob support: literal paths, or a single `*` in the final component.
fn expand_glob(candidate: &Path) -> Vec<PathBuf> {
    let Some(name) = candidate.file_name().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    if !name.contains('*') {
        return if candidate.is_file() {
            vec![candidate.to_path_buf()]
        } else {
            Vec::new()
        };
    }
    let Some(dir) = candidate.parent() else {
        return Vec::new();
    };
    let Some((prefix, suffix)) = name.split_once('*') else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|n| n.starts_with(prefix) && n.ends_with(suffix))
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod group_tests {
    use super::*;
    use std::io::Write;

    fn parse(text: &str) -> SshConfig {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(text.as_bytes()).unwrap();
        f.flush().unwrap();
        SshConfig::load(f.path()).unwrap()
    }

    fn group_of<'a>(config: &'a SshConfig, alias: &str) -> Option<&'a str> {
        config.get(alias)?.group.as_deref()
    }

    #[test]
    fn a_comment_heading_names_the_hosts_under_it() {
        let config = parse(
            "# Production\nHost web-1\n  HostName 10.0.0.1\n\nHost web-2\n  HostName 10.0.0.2\n\n# Staging\nHost stage-1\n  HostName 10.0.1.1\n",
        );
        assert_eq!(group_of(&config, "web-1"), Some("Production"));
        assert_eq!(
            group_of(&config, "web-2"),
            Some("Production"),
            "a group runs until the next heading"
        );
        assert_eq!(group_of(&config, "stage-1"), Some("Staging"));
    }

    #[test]
    fn a_config_without_comments_has_no_groups() {
        let config = parse("Host solo\n  HostName 10.0.0.9\n");
        assert_eq!(group_of(&config, "solo"), None);
    }

    /// `# Host old` is disabled configuration. Naming a group after it would
    /// be nonsense, and this is the most likely comment in a real file.
    #[test]
    fn a_commented_out_directive_is_not_a_heading() {
        let config = parse(
            "# Production\nHost web-1\n  HostName 10.0.0.1\n# Host retired-box\n#   HostName 10.0.0.5\nHost web-2\n  HostName 10.0.0.2\n",
        );
        assert_eq!(group_of(&config, "web-2"), Some("Production"));
    }

    #[test]
    fn a_separator_or_a_sentence_is_not_a_heading() {
        let config = parse(
            "# Production\nHost web-1\n  HostName 10.0.0.1\n# ----------------\nHost web-2\n  HostName 10.0.0.2\n",
        );
        assert_eq!(group_of(&config, "web-2"), Some("Production"));

        let long = "# this box is the one that runs the nightly export job, do not reboot";
        let config = parse(&format!(
            "# Production\nHost web-1\n  HostName 10.0.0.1\n{long}\nHost web-2\n  HostName 10.0.0.2\n"
        ));
        assert_eq!(
            group_of(&config, "web-2"),
            Some("Production"),
            "a sentence is a note, not a heading"
        );
    }

    #[test]
    fn a_trailing_colon_is_not_part_of_the_name() {
        let config = parse("# Databases:\nHost db-1\n  HostName 10.0.2.1\n");
        assert_eq!(group_of(&config, "db-1"), Some("Databases"));
    }

    #[test]
    fn hosts_before_any_heading_are_ungrouped() {
        let config = parse(
            "Host loose\n  HostName 10.0.0.1\n# Production\nHost web-1\n  HostName 10.0.0.2\n",
        );
        assert_eq!(group_of(&config, "loose"), None);
        assert_eq!(group_of(&config, "web-1"), Some("Production"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> SshConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, text).unwrap();
        SshConfig::load(&path).unwrap()
    }

    #[test]
    fn parses_basic_stanza() {
        let c = parse("Host web-1\n  HostName 10.0.0.1\n  User deploy\n  Port 2222\n");
        let h = c.get("web-1").unwrap();
        assert_eq!(h.hostname, "10.0.0.1");
        assert_eq!(h.user.as_deref(), Some("deploy"));
        assert_eq!(h.port, 2222);
    }

    #[test]
    fn hostname_defaults_to_alias() {
        let c = parse("Host example.com\n  User root\n");
        assert_eq!(c.get("example.com").unwrap().hostname, "example.com");
    }

    #[test]
    fn accepts_equals_separator() {
        let c = parse("Host=web-1\nHostName=10.0.0.1\n");
        assert_eq!(c.get("web-1").unwrap().hostname, "10.0.0.1");
    }

    #[test]
    fn one_stanza_many_aliases() {
        let c = parse("Host a b\n  HostName shared\n");
        assert_eq!(c.hosts.len(), 2);
        assert_eq!(c.get("b").unwrap().hostname, "shared");
    }

    #[test]
    fn wildcard_stanzas_are_not_targets() {
        let c = parse("Host *\n  User root\nHost real\n  HostName r\n");
        assert_eq!(c.hosts.len(), 1);
        assert_eq!(c.hosts[0].alias, "real");
    }

    #[test]
    fn match_block_is_skipped_with_warning() {
        let c = parse("Match host bastion\n  User jump\nHost web\n  HostName w\n");
        assert_eq!(c.hosts.len(), 1);
        assert!(c.warnings.iter().any(|w| w.contains("Match")));
    }

    #[test]
    fn local_forwards_accumulate_rather_than_overwrite() {
        let c = parse(
            "Host db\n  HostName h\n  LocalForward 5432 db.internal:5432\n  \
             LocalForward 127.0.0.1:6379 cache:6379\n",
        );
        let host = c.get("db").unwrap();
        assert_eq!(
            host.forwards,
            vec![
                crate::forward::Forward::new(5432, "db.internal", 5432),
                crate::forward::Forward::new(6379, "cache", 6379),
            ]
        );
    }

    /// A line that cannot be understood is dropped rather than guessed at: a
    /// forward bound to the wrong port is worse than no forward.
    #[test]
    fn an_unparseable_local_forward_is_dropped() {
        let c = parse("Host db\n  HostName h\n  LocalForward nonsense\n");
        let host = c.get("db").unwrap();
        assert!(host.forwards.is_empty());
    }

    #[test]
    fn proxy_jump_and_identity_agent() {
        let c = parse(
            "Host inner\n  HostName 10.0.0.9\n  ProxyJump bastion\n  IdentityAgent ~/.1p/agent.sock\n",
        );
        let h = c.get("inner").unwrap();
        assert_eq!(h.proxy_jump.as_deref(), Some("bastion"));
        assert!(h.identity_agent.as_deref().unwrap().ends_with("agent.sock"));
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let c = parse("# lead\n\nHost web\n  # inner\n  HostName w\n\n");
        assert_eq!(c.get("web").unwrap().hostname, "w");
    }

    #[test]
    fn include_pulls_in_nested_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("extra"),
            "Host inner\n  HostName 10.1.1.1\n",
        )
        .unwrap();
        let main = dir.path().join("config");
        std::fs::write(&main, "Include extra\nHost outer\n  HostName 10.2.2.2\n").unwrap();
        let c = SshConfig::load(&main).unwrap();
        assert_eq!(c.get("inner").unwrap().hostname, "10.1.1.1");
        assert_eq!(c.get("outer").unwrap().hostname, "10.2.2.2");
    }
}
