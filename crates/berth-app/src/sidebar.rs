//! The sidebar tree: a folder per host, its sessions under it.
//!
//! Pure structure, like `panes`: what is configured, open and running goes in,
//! a flat list of rows comes out, and no gpui is involved. The things that go
//! wrong here are about which session lands under which folder and in what
//! order, and none of that needs a window to test.

use std::collections::{BTreeMap, BTreeSet};

use berth_core::agents::Running;
use berth_core::session::SESSION_PREFIX;

/// Which folder a row belongs to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Folder {
    /// Shells on this machine (D36).
    Local,
    /// A host from ssh config, by alias.
    Host(String),
}

/// One line of the sidebar, in the order it is drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// An ssh config comment heading (D37).
    Heading(String),
    /// A host, or this machine. `agent` is only set while collapsed, so an
    /// agent hidden inside a closed folder still shows on the folder.
    Folder {
        folder: Folder,
        expanded: bool,
        agent: bool,
    },
    Session {
        folder: Folder,
        /// The tmux slug for a remote session, the label for a local one.
        name: String,
        /// The pane showing it, or `None` when it runs detached.
        pane: Option<u64>,
        agent: bool,
    },
    NewTerminal(Folder),
}

/// A host as the sidebar needs it: its alias and its heading.
pub struct HostEntry<'a> {
    pub alias: &'a str,
    pub group: Option<&'a str>,
}

/// A pane on screen, and the session it shows.
pub struct OpenSession<'a> {
    pub pane: u64,
    /// `None` for a local shell.
    pub host: Option<&'a str>,
    pub slug: &'a str,
}

/// Everything the tree is built from.
pub struct Input<'a> {
    pub hosts: &'a [HostEntry<'a>],
    pub open: &'a [OpenSession<'a>],
    /// What each host's tmux reported, by alias: slug to agent presence.
    pub running: &'a BTreeMap<String, BTreeMap<String, bool>>,
    /// Folders the user opened or closed by hand. Anything absent is open when
    /// it has sessions and closed when it has none.
    pub expanded: &'a BTreeMap<Folder, bool>,
    /// What a local shell is called, before numbering.
    pub local_label: &'a str,
    /// Local server sessions no pane shows, by the name the tree gives them
    /// (D50).
    pub local_detached: &'a [String],
}

/// Berth's own sessions from one host's scan, with whether an agent is running
/// in each.
///
/// Sessions outside the `berth/` namespace are the user's own tmux and are not
/// Berth's to list. A session with several panes is an agent if any is.
pub fn listing(scan: &[Running]) -> BTreeMap<String, bool> {
    let mut out: BTreeMap<String, bool> = BTreeMap::new();
    for running in scan {
        let Some(slug) = running.session.strip_prefix(SESSION_PREFIX) else {
            continue;
        };
        if slug.is_empty() {
            continue;
        }
        *out.entry(slug.to_string()).or_default() |= running.is_agent();
    }
    out
}

/// Build the rows, headings and folders in ssh config order, this machine last.
pub fn rows(input: &Input) -> Vec<Row> {
    let mut out = Vec::new();
    let empty = BTreeMap::new();

    // File order, heading printed when it changes. Sorting would reorder the
    // file, and the order someone put their own config in is information.
    let mut heading: Option<&str> = None;
    for host in input.hosts {
        if host.group != heading {
            heading = host.group;
            if let Some(name) = heading {
                out.push(Row::Heading(name.to_string()));
            }
        }

        let running = input.running.get(host.alias).unwrap_or(&empty);
        let mut sessions: BTreeMap<String, (Option<u64>, bool)> = running
            .iter()
            .map(|(slug, agent)| (slug.clone(), (None, *agent)))
            .collect();
        // An open pane counts before its first scan has come back, so a new
        // session never blinks out of the tree for five seconds.
        for open in input.open.iter().filter(|o| o.host == Some(host.alias)) {
            sessions.entry(open.slug.to_string()).or_default().0 = Some(open.pane);
        }

        let mut ordered: Vec<(String, Option<u64>, bool)> = sessions
            .into_iter()
            .map(|(slug, (pane, agent))| (slug, pane, agent))
            .collect();
        ordered.sort_by(|a, b| natural(&a.0).cmp(&natural(&b.0)));

        push_folder(
            &mut out,
            Folder::Host(host.alias.to_string()),
            ordered,
            input.expanded,
        );
    }

    let mut local: Vec<(String, Option<u64>, bool)> = input
        .open
        .iter()
        .filter(|o| o.host.is_none())
        .enumerate()
        .map(|(index, o)| (local_name(input.local_label, index), Some(o.pane), false))
        .collect();
    local.extend(
        input
            .local_detached
            .iter()
            .map(|name| (name.clone(), None, false)),
    );
    push_folder(&mut out, Folder::Local, local, input.expanded);

    out
}

fn push_folder(
    out: &mut Vec<Row>,
    folder: Folder,
    sessions: Vec<(String, Option<u64>, bool)>,
    expanded: &BTreeMap<Folder, bool>,
) {
    let open = expanded
        .get(&folder)
        .copied()
        .unwrap_or(!sessions.is_empty());
    let agent = !open && sessions.iter().any(|s| s.2);
    out.push(Row::Folder {
        folder: folder.clone(),
        expanded: open,
        agent,
    });
    if !open {
        return;
    }
    for (name, pane, agent) in sessions {
        out.push(Row::Session {
            folder: folder.clone(),
            name,
            pane,
            agent,
        });
    }
    out.push(Row::NewTerminal(folder));
}

/// `zsh`, then `zsh 2`, `zsh 3`, in the order the panes were opened.
fn local_name(label: &str, index: usize) -> String {
    if index == 0 {
        label.to_string()
    } else {
        format!("{label} {}", index + 1)
    }
}

/// Sort key that puts `main-10` after `main-9`, and `main` before `main-2`.
///
/// Splits name their sessions `main`, `main-2`, `main-3`, so plain string order
/// would file the tenth split between the first and the second.
fn natural(slug: &str) -> (&str, u64) {
    match slug.rsplit_once('-') {
        Some((base, n)) if !base.is_empty() => match n.parse::<u64>() {
            Ok(n) => (base, n),
            Err(_) => (slug, 1),
        },
        _ => (slug, 1),
    }
}

/// Slugs in use on a host, open or detached, so a new terminal never lands in
/// a session that is already running and mirrors it.
pub fn taken<'a>(
    alias: &str,
    open: &'a [OpenSession<'a>],
    running: &'a BTreeMap<String, BTreeMap<String, bool>>,
) -> BTreeSet<&'a str> {
    let mut out: BTreeSet<&str> = open
        .iter()
        .filter(|o| o.host == Some(alias))
        .map(|o| o.slug)
        .collect();
    if let Some(listed) = running.get(alias) {
        out.extend(listed.keys().map(String::as_str));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(pairs: &[(&str, &str)]) -> Vec<Running> {
        pairs
            .iter()
            .map(|(session, command)| Running {
                session: session.to_string(),
                command: command.to_string(),
            })
            .collect()
    }

    fn host<'a>(alias: &'a str, group: Option<&'a str>) -> HostEntry<'a> {
        HostEntry { alias, group }
    }

    fn build(
        hosts: &[HostEntry],
        open: &[OpenSession],
        running: &BTreeMap<String, BTreeMap<String, bool>>,
        expanded: &BTreeMap<Folder, bool>,
    ) -> Vec<Row> {
        rows(&Input {
            hosts,
            open,
            running,
            expanded,
            local_label: "zsh",
            local_detached: &[],
        })
    }

    fn session_names(rows: &[Row], of: &Folder) -> Vec<String> {
        rows.iter()
            .filter_map(|r| match r {
                Row::Session { folder, name, .. } if folder == of => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    fn web() -> Folder {
        Folder::Host("web".into())
    }

    #[test]
    fn only_berth_sessions_are_listed() {
        let scan = running(&[
            ("berth/main", "zsh"),
            ("scratch", "vim"),
            ("berth/", "zsh"),
            ("berth/deploy", "bash"),
        ]);
        let listed = listing(&scan);
        assert_eq!(listed.keys().collect::<Vec<_>>(), ["deploy", "main"]);
    }

    /// tmux reports one line per pane. A session with an agent in one pane
    /// and a shell in another is a session with an agent.
    #[test]
    fn a_session_is_an_agent_if_any_pane_is() {
        let scan = running(&[
            ("berth/main", "claude"),
            ("berth/main", "zsh"),
            ("berth/logs", "zsh"),
        ]);
        let listed = listing(&scan);
        assert_eq!(listed.get("main"), Some(&true));
        assert_eq!(listed.get("logs"), Some(&false));
    }

    /// Detach is not kill (D32), so a session with no tab is still a row.
    #[test]
    fn detached_sessions_are_listed_beside_open_ones() {
        let hosts = [host("web", None)];
        let open = [OpenSession {
            pane: 7,
            host: Some("web"),
            slug: "main",
        }];
        let running = BTreeMap::from([(
            "web".to_string(),
            BTreeMap::from([("main".to_string(), false), ("deploy".to_string(), true)]),
        )]);
        let rows = build(&hosts, &open, &running, &BTreeMap::new());

        assert!(rows.contains(&Row::Session {
            folder: web(),
            name: "main".into(),
            pane: Some(7),
            agent: false,
        }));
        assert!(rows.contains(&Row::Session {
            folder: web(),
            name: "deploy".into(),
            pane: None,
            agent: true,
        }));
    }

    /// A pane that opened a moment ago has not been scanned yet. It must not
    /// be missing from the tree until the next poll.
    #[test]
    fn an_open_pane_shows_before_its_first_scan() {
        let hosts = [host("web", None)];
        let open = [OpenSession {
            pane: 1,
            host: Some("web"),
            slug: "main",
        }];
        let rows = build(&hosts, &open, &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(session_names(&rows, &web()), ["main"]);
    }

    #[test]
    fn splits_sort_by_number_not_by_text() {
        let hosts = [host("web", None)];
        let running = BTreeMap::from([(
            "web".to_string(),
            ["main-10", "main-2", "main", "deploy", "main-9"]
                .into_iter()
                .map(|s| (s.to_string(), false))
                .collect(),
        )]);
        let rows = build(&hosts, &[], &running, &BTreeMap::new());
        assert_eq!(
            session_names(&rows, &web()),
            ["deploy", "main", "main-2", "main-9", "main-10"]
        );
    }

    /// Headings in file order, printed once per run of hosts sharing one.
    #[test]
    fn headings_follow_the_file() {
        let hosts = [
            host("a", Some("Work")),
            host("b", Some("Work")),
            host("c", Some("Home")),
            host("d", None),
        ];
        let rows = build(&hosts, &[], &BTreeMap::new(), &BTreeMap::new());
        let order: Vec<String> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Heading(h) => Some(format!("#{h}")),
                Row::Folder {
                    folder: Folder::Host(a),
                    ..
                } => Some(a.clone()),
                Row::Folder {
                    folder: Folder::Local,
                    ..
                } => Some("local".into()),
                _ => None,
            })
            .collect();
        assert_eq!(order, ["#Work", "a", "b", "#Home", "c", "d", "local"]);
    }

    /// A host with nothing running would otherwise be a folder holding only
    /// "new terminal", repeated for every host in the file.
    #[test]
    fn a_folder_with_sessions_opens_and_one_without_stays_closed() {
        let hosts = [host("web", None), host("db", None)];
        let running = BTreeMap::from([(
            "web".to_string(),
            BTreeMap::from([("main".to_string(), false)]),
        )]);
        let rows = build(&hosts, &[], &running, &BTreeMap::new());
        assert!(rows.contains(&Row::Folder {
            folder: web(),
            expanded: true,
            agent: false,
        }));
        assert!(rows.contains(&Row::Folder {
            folder: Folder::Host("db".into()),
            expanded: false,
            agent: false,
        }));
        assert!(rows.contains(&Row::NewTerminal(web())));
        assert!(!rows.contains(&Row::NewTerminal(Folder::Host("db".into()))));
    }

    /// Closing a folder must not hide that an agent is running inside it.
    #[test]
    fn a_closed_folder_carries_its_sessions_agent() {
        let hosts = [host("web", None)];
        let running = BTreeMap::from([(
            "web".to_string(),
            BTreeMap::from([("main".to_string(), true)]),
        )]);
        let closed = BTreeMap::from([(web(), false)]);
        let rows = build(&hosts, &[], &running, &closed);
        assert_eq!(
            rows,
            [
                Row::Folder {
                    folder: web(),
                    expanded: false,
                    agent: true,
                },
                Row::Folder {
                    folder: Folder::Local,
                    expanded: false,
                    agent: false,
                },
            ]
        );
    }

    /// Open, the agent is on the session row and not repeated on the folder.
    #[test]
    fn an_open_folder_leaves_the_agent_on_the_session() {
        let hosts = [host("web", None)];
        let running = BTreeMap::from([(
            "web".to_string(),
            BTreeMap::from([("main".to_string(), true)]),
        )]);
        let rows = build(&hosts, &[], &running, &BTreeMap::new());
        assert!(rows.contains(&Row::Folder {
            folder: web(),
            expanded: true,
            agent: false,
        }));
    }

    #[test]
    fn an_empty_folder_can_be_opened_by_hand() {
        let hosts = [host("db", None)];
        let opened = BTreeMap::from([(Folder::Host("db".into()), true)]);
        let rows = build(&hosts, &[], &BTreeMap::new(), &opened);
        assert!(rows.contains(&Row::NewTerminal(Folder::Host("db".into()))));
    }

    #[test]
    fn local_shells_are_numbered_in_opening_order() {
        let open = [
            OpenSession {
                pane: 3,
                host: None,
                slug: "local",
            },
            OpenSession {
                pane: 5,
                host: None,
                slug: "local",
            },
        ];
        let rows = build(&[], &open, &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(session_names(&rows, &Folder::Local), ["zsh", "zsh 2"]);
        assert!(rows.contains(&Row::Session {
            folder: Folder::Local,
            name: "zsh 2".into(),
            pane: Some(5),
            agent: false,
        }));
    }

    /// A local shell is not a session on a host that happens to share a name.
    #[test]
    fn a_host_aliased_local_does_not_collect_local_shells() {
        let hosts = [host("local", None)];
        let open = [OpenSession {
            pane: 1,
            host: None,
            slug: "local",
        }];
        let rows = build(&hosts, &open, &BTreeMap::new(), &BTreeMap::new());
        assert!(session_names(&rows, &Folder::Host("local".into())).is_empty());
        assert_eq!(session_names(&rows, &Folder::Local), ["zsh"]);
    }

    /// Local server sessions no tab shows sit under this machine, after the
    /// open ones, with no pane (D50).
    #[test]
    fn detached_local_sessions_are_listed_under_this_machine() {
        let open = [OpenSession {
            pane: 1,
            host: None,
            slug: "local",
        }];
        let detached = ["zsh #4".to_string()];
        let rows = rows(&Input {
            hosts: &[],
            open: &open,
            running: &BTreeMap::new(),
            expanded: &BTreeMap::new(),
            local_label: "zsh",
            local_detached: &detached,
        });
        assert_eq!(session_names(&rows, &Folder::Local), ["zsh", "zsh #4"]);
        assert!(rows.contains(&Row::Session {
            folder: Folder::Local,
            name: "zsh #4".into(),
            pane: None,
            agent: false,
        }));
    }

    #[test]
    fn taken_counts_open_and_detached_sessions() {
        let open = [OpenSession {
            pane: 1,
            host: Some("web"),
            slug: "main",
        }];
        let running = BTreeMap::from([(
            "web".to_string(),
            BTreeMap::from([("main-2".to_string(), false)]),
        )]);
        let taken = taken("web", &open, &running);
        assert_eq!(taken.into_iter().collect::<Vec<_>>(), ["main", "main-2"]);
    }
}
