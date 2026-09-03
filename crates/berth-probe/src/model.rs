//! Typed mirrors of the probe's JSON. Every field the script emits is
//! optional-tolerant, so an older script on a server cannot break the client.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SysInfo {
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub uptime_s: u64,
    #[serde(default)]
    pub load: [f32; 3],
    #[serde(default)]
    pub mem_total: u64,
    #[serde(default)]
    pub mem_used: u64,
    #[serde(default)]
    pub disks: Vec<Disk>,
    #[serde(default)]
    pub os: String,
}

impl SysInfo {
    pub fn mem_fraction(&self) -> f32 {
        if self.mem_total == 0 {
            0.0
        } else {
            self.mem_used as f32 / self.mem_total as f32
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Disk {
    pub mount: String,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub used: u64,
}

impl Disk {
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.used as f32 / self.total as f32
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Ports {
    #[serde(default)]
    pub ports: Vec<Port>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Port {
    #[serde(default = "tcp")]
    pub proto: String,
    pub port: u16,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub cmd: String,
}

fn tcp() -> String {
    "tcp".to_string()
}

impl Port {
    /// The owning process, when the remote user could see it. `ss` and
    /// `netstat` only report pid and command for sockets the caller owns, so an
    /// unprivileged probe legitimately returns neither and the UI must show the
    /// port without inventing a pid of 0.
    pub fn process(&self) -> Option<(u32, &str)> {
        if self.pid == 0 || self.cmd.is_empty() {
            None
        } else {
            Some((self.pid, self.cmd.as_str()))
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Sessions {
    #[serde(default)]
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub attached: bool,
    #[serde(default)]
    pub windows: Vec<Window>,
}

impl Session {
    /// The user-facing part of `berth/<slug>`.
    pub fn slug(&self) -> &str {
        self.name.strip_prefix("berth/").unwrap_or(&self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub cmd: String,
    #[serde(default)]
    pub pid: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Agents {
    #[serde(default)]
    pub agents: Vec<Agent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub session: String,
    #[serde(default)]
    pub window: u32,
    #[serde(default)]
    pub kind: AgentKind,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub alive: bool,
}

/// Detected from the process name alone. Berth never reads pane contents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    ClaudeCode,
    Codex,
    Opencode,
    #[default]
    #[serde(other)]
    Other,
}

impl AgentKind {
    /// The label shown in the sidebar.
    pub fn label(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::Codex => "codex",
            AgentKind::Opencode => "opencode",
            AgentKind::Other => "agent",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractions_never_divide_by_zero() {
        let sys = SysInfo {
            host: String::new(),
            uptime_s: 0,
            load: [0.0; 3],
            mem_total: 0,
            mem_used: 0,
            disks: vec![],
            os: String::new(),
        };
        assert_eq!(sys.mem_fraction(), 0.0);
        assert_eq!(
            Disk {
                mount: "/".into(),
                total: 0,
                used: 5
            }
            .fraction(),
            0.0
        );
    }

    #[test]
    fn an_unprivileged_probe_reports_no_owning_process() {
        let anonymous = Port {
            proto: "tcp".into(),
            port: 443,
            pid: 0,
            cmd: String::new(),
        };
        assert_eq!(anonymous.process(), None);

        let owned = Port {
            proto: "tcp".into(),
            port: 443,
            pid: 812,
            cmd: "nginx".into(),
        };
        assert_eq!(owned.process(), Some((812, "nginx")));
    }

    #[test]
    fn session_slug_strips_the_namespace() {
        let s = Session {
            name: "berth/main".into(),
            created: 0,
            attached: false,
            windows: vec![],
        };
        assert_eq!(s.slug(), "main");
    }
}
