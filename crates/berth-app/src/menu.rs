//! What a right-click offers, by what was clicked.
//!
//! Pure, like `sidebar`: which actions a row gets is where the rules live, and
//! the rules are the ones that must not drift. Detach and End session are
//! different actions with different costs and never share a label (D32), a
//! local shell has no session to end, and ending is always last so it is never
//! the item a hurried click lands on.

/// What was right-clicked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A session open in a pane, from its tab or its sidebar row.
    Pane(u64),
    /// A remote session running with no tab.
    Detached { alias: String, slug: String },
    /// A local server session running with no tab (D50).
    LocalDetached { session: u64 },
    /// A host folder.
    Host(String),
    /// The "This machine" folder.
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Rename a session.
    Rename,
    /// Close the tab and leave the tmux session running.
    Detach,
    /// Close a local shell, which ends it.
    Close,
    /// Kill the session, after asking.
    EndSession,
    /// Reattach a detached session in a tab of its own.
    Open,
    NewTerminal,
    /// Connect a host that is not connected: verify, retry or reattach.
    Connect,
    /// Give a host a name Berth shows instead of its alias.
    RenameHost,
    /// Look at a host's Berth server again: offer it, or say how it stands.
    Server,
}

/// What the menu needs to know beyond the target itself.
#[derive(Debug, Clone, Copy, Default)]
pub struct Facts {
    /// The pane is a local shell of its own, which closing ends. A local
    /// session on the local server detaches like a remote one (D50).
    pub local: bool,
    /// The host has a live connection.
    pub connected: bool,
}

/// The actions for a target, in the order they are listed.
pub fn actions(target: &Target, facts: Facts) -> Vec<Action> {
    match target {
        Target::Pane(_) if facts.local => vec![Action::Rename, Action::Close],
        Target::Pane(_) => vec![Action::Rename, Action::Detach, Action::EndSession],
        Target::Detached { .. } | Target::LocalDetached { .. } => {
            vec![Action::Open, Action::EndSession]
        }
        Target::Host(_) if facts.connected => {
            vec![Action::NewTerminal, Action::RenameHost, Action::Server]
        }
        Target::Host(_) => vec![Action::Connect, Action::RenameHost],
        Target::Local => vec![Action::NewTerminal],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_target() -> Vec<(Target, Facts)> {
        let mut out = Vec::new();
        for local in [false, true] {
            for connected in [false, true] {
                let facts = Facts { local, connected };
                out.push((Target::Pane(1), facts));
                out.push((
                    Target::Detached {
                        alias: "web".into(),
                        slug: "main".into(),
                    },
                    facts,
                ));
                out.push((Target::Host("web".into()), facts));
                out.push((Target::Local, facts));
            }
        }
        out
    }

    /// The destructive item is last, below everything else, so it is never
    /// the one a hurried click lands on.
    #[test]
    fn ending_is_always_last() {
        for (target, facts) in every_target() {
            let actions = actions(&target, facts);
            if let Some(at) = actions.iter().position(|a| *a == Action::EndSession) {
                assert_eq!(at, actions.len() - 1, "{target:?}");
            }
        }
    }

    /// A local shell has no tmux session behind it, so there is nothing to end
    /// beyond closing it, and nothing to detach from.
    #[test]
    fn a_local_shell_closes_and_is_never_ended_or_detached() {
        let actions = actions(
            &Target::Pane(1),
            Facts {
                local: true,
                connected: false,
            },
        );
        assert!(actions.contains(&Action::Close));
        assert!(!actions.contains(&Action::EndSession));
        assert!(!actions.contains(&Action::Detach));
    }

    /// A local session on the local server detaches like a remote one, and
    /// ending it is its own, confirmed action (D50).
    #[test]
    fn a_local_server_session_detaches_and_ends_like_a_remote_one() {
        assert_eq!(
            actions(&Target::Pane(1), Facts::default()),
            [Action::Rename, Action::Detach, Action::EndSession]
        );
        assert_eq!(
            actions(&Target::LocalDetached { session: 3 }, Facts::default()),
            [Action::Open, Action::EndSession]
        );
    }

    #[test]
    fn a_remote_pane_offers_detach_and_end_separately() {
        let actions = actions(&Target::Pane(1), Facts::default());
        assert_eq!(
            actions,
            [Action::Rename, Action::Detach, Action::EndSession]
        );
    }

    #[test]
    fn a_host_connects_or_opens_a_terminal_by_its_state() {
        let host = Target::Host("web".into());
        let up = Facts {
            local: false,
            connected: true,
        };
        // The server is asked about over the connection, so only then.
        assert_eq!(
            actions(&host, up),
            [Action::NewTerminal, Action::RenameHost, Action::Server]
        );
        assert_eq!(
            actions(&host, Facts::default()),
            [Action::Connect, Action::RenameHost]
        );
    }

    #[test]
    fn a_detached_session_can_be_opened_or_ended() {
        let detached = Target::Detached {
            alias: "web".into(),
            slug: "deploy".into(),
        };
        assert_eq!(
            actions(&detached, Facts::default()),
            [Action::Open, Action::EndSession]
        );
    }
}
