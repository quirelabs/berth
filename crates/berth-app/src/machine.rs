//! Which machine a server session runs on, and every server's session list
//! (D55).
//!
//! A session's id is only unique on its own machine's server: this machine's
//! session 3 and a host's session 3 are two sessions. So a server session is
//! always named by its machine and its id together, and every list, status
//! and row reaches it that way. Pure, like `sidebar`: no gpui.

use std::collections::BTreeMap;

use berth_proto::v1::SessionInfo;

/// Where a server session runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Machine {
    /// This machine's server (D50).
    Local,
    /// A host's server, by its ssh config alias (D55).
    Host(String),
}

impl Machine {
    /// The machine of a pane on `host`, `None` being this one.
    pub fn of(host: Option<&str>) -> Self {
        match host {
            Some(alias) => Machine::Host(alias.to_string()),
            None => Machine::Local,
        }
    }
}

/// A server session: its machine and its id there.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionRef {
    pub machine: Machine,
    pub id: u64,
}

impl SessionRef {
    pub fn local(id: u64) -> Self {
        Self {
            machine: Machine::Local,
            id,
        }
    }

    pub fn on(host: Option<&str>, id: u64) -> Self {
        Self {
            machine: Machine::of(host),
            id,
        }
    }
}

/// Each server's sessions, as it last listed them.
#[derive(Debug, Default)]
pub struct Sessions {
    lists: BTreeMap<Machine, Vec<SessionInfo>>,
}

impl Sessions {
    /// A machine's list as its server pushed it. Whether anything changed.
    pub fn set(&mut self, machine: Machine, list: Vec<SessionInfo>) -> bool {
        if self.lists.get(&machine) == Some(&list) {
            return false;
        }
        if list.is_empty() {
            self.lists.remove(&machine);
        } else {
            self.lists.insert(machine, list);
        }
        true
    }

    pub fn info(&self, session: &SessionRef) -> Option<&SessionInfo> {
        self.lists
            .get(&session.machine)?
            .iter()
            .find(|info| info.id == session.id)
    }

    pub fn info_mut(&mut self, session: &SessionRef) -> Option<&mut SessionInfo> {
        self.lists
            .get_mut(&session.machine)?
            .iter_mut()
            .find(|info| info.id == session.id)
    }

    /// Take a session off its list at once, ahead of its server saying so.
    pub fn remove(&mut self, session: &SessionRef) {
        if let Some(list) = self.lists.get_mut(&session.machine) {
            list.retain(|info| info.id != session.id);
        }
    }

    /// One machine's sessions.
    pub fn of(&self, machine: &Machine) -> &[SessionInfo] {
        self.lists.get(machine).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Every session on every machine.
    pub fn all(&self) -> impl Iterator<Item = (SessionRef, &SessionInfo)> {
        self.lists.iter().flat_map(|(machine, list)| {
            list.iter().map(move |info| {
                (
                    SessionRef {
                        machine: machine.clone(),
                        id: info.id,
                    },
                    info,
                )
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: u64, title: &str) -> SessionInfo {
        SessionInfo {
            id,
            name: String::new(),
            cwd: None,
            program: None,
            cols: 80,
            rows: 24,
            claude: None,
            pid: None,
            status: None,
            title: Some(title.to_string()),
            exit: None,
        }
    }

    /// The same id on two machines is two sessions: each found, changed
    /// and ended on its own, and never taken for the other.
    #[test]
    fn two_machines_with_the_same_session_id_never_collide() {
        let mut sessions = Sessions::default();
        let here = SessionRef::local(1);
        let there = SessionRef::on(Some("box"), 1);
        let elsewhere = SessionRef::on(Some("other"), 1);
        assert_ne!(here, there);
        assert_ne!(there, elsewhere);

        sessions.set(Machine::Local, vec![info(1, "here")]);
        sessions.set(Machine::Host("box".into()), vec![info(1, "there")]);
        assert_eq!(sessions.info(&here).unwrap().title.as_deref(), Some("here"));
        assert_eq!(
            sessions.info(&there).unwrap().title.as_deref(),
            Some("there")
        );
        assert!(sessions.info(&elsewhere).is_none());

        sessions.info_mut(&there).unwrap().title = Some("renamed".into());
        assert_eq!(sessions.info(&here).unwrap().title.as_deref(), Some("here"));

        sessions.remove(&here);
        assert!(sessions.info(&here).is_none());
        assert_eq!(
            sessions.info(&there).unwrap().title.as_deref(),
            Some("renamed")
        );

        let all: Vec<SessionRef> = sessions.all().map(|(r, _)| r).collect();
        assert_eq!(all, vec![there]);
    }

    #[test]
    fn a_list_unchanged_is_not_a_change_and_an_empty_one_is_forgotten() {
        let mut sessions = Sessions::default();
        assert!(sessions.set(Machine::Local, vec![info(2, "a")]));
        assert!(!sessions.set(Machine::Local, vec![info(2, "a")]));
        assert!(sessions.set(Machine::Local, Vec::new()));
        assert!(sessions.of(&Machine::Local).is_empty());
        assert_eq!(sessions.all().count(), 0);
    }
}
