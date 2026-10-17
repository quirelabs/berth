//! The sidebar tree: a folder per host, its sessions under it, and the
//! sessions that need the user pinned above them all (D53).
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
    /// The group of sessions that need the user, across hosts, at the top
    /// while there are any.
    NeedsYou,
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
        /// The tmux slug for a remote tmux session, the label for any other.
        name: String,
        /// The pane showing it, or `None` when it runs detached.
        pane: Option<u64>,
        /// Its id on its host's server, for a host's server session (D55).
        server: Option<u64>,
        /// A tmux session on a host that has moved to its server: marked so,
        /// as it stays on tmux until it ends.
        tmux: bool,
        agent: bool,
        /// In the needs-you group rather than under its folder.
        pinned: bool,
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
    /// Its id on the host's server, when it shows a server session there
    /// rather than a tmux one (D55).
    pub server: Option<u64>,
}

/// A session on a host's server, open or not, as its folder lists it.
pub struct ServerRow {
    pub id: u64,
    pub name: String,
    pub pane: Option<u64>,
    pub agent: bool,
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
    /// (With whether Claude runs in each.)
    pub local_detached: &'a [(String, bool)],
    /// Each host's server sessions, open or not, by alias (D55).
    pub host_servers: &'a BTreeMap<String, Vec<ServerRow>>,
    /// Hosts that have moved to their server: their tmux sessions are
    /// marked as such.
    pub on_server: &'a BTreeSet<String>,
    /// Whether a session needs the user, by its folder, name, pane and id
    /// on a host's server: it is pinned to the top then, whatever its
    /// folder.
    pub needs_you: &'a NeedsYou<'a>,
}

/// Whether a session needs the user: by its folder, name, pane, and id on
/// a host's server.
pub type NeedsYou<'a> = dyn Fn(&Folder, &str, Option<u64>, Option<u64>) -> bool + 'a;

/// A session as a folder holds it before it is a row.
struct Listed {
    name: String,
    pane: Option<u64>,
    server: Option<u64>,
    tmux: bool,
    agent: bool,
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

/// What runs in each of Berth's sessions on one host, by slug: the foreground
/// process's name, as tmux gives it. Of a session with several panes, the
/// agent's if one runs there, else the first pane's.
pub fn programs(scan: &[Running]) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for running in scan {
        let Some(slug) = running.session.strip_prefix(SESSION_PREFIX) else {
            continue;
        };
        if slug.is_empty() {
            continue;
        }
        match out.get(slug) {
            Some(_) if !running.is_agent() => {}
            _ => {
                out.insert(slug.to_string(), running.command.clone());
            }
        }
    }
    out
}

/// Build the rows, headings and folders in ssh config order, this machine last.
pub fn rows(input: &Input) -> Vec<Row> {
    let mut out = Vec::new();
    let mut pinned = Vec::new();
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
        for open in input
            .open
            .iter()
            .filter(|o| o.host == Some(host.alias) && o.server.is_none())
        {
            sessions.entry(open.slug.to_string()).or_default().0 = Some(open.pane);
        }

        let mut tmux: Vec<(String, Option<u64>, bool)> = sessions
            .into_iter()
            .map(|(slug, (pane, agent))| (slug, pane, agent))
            .collect();
        tmux.sort_by(|a, b| natural(&a.0).cmp(&natural(&b.0)));
        let marked = input.on_server.contains(host.alias);
        // The server's sessions first, then those still on tmux.
        let ordered: Vec<Listed> = input
            .host_servers
            .get(host.alias)
            .into_iter()
            .flatten()
            .map(|row| Listed {
                name: row.name.clone(),
                pane: row.pane,
                server: Some(row.id),
                tmux: false,
                agent: row.agent,
            })
            .chain(tmux.into_iter().map(|(name, pane, agent)| Listed {
                name,
                pane,
                server: None,
                tmux: marked,
                agent,
            }))
            .collect();

        push_folder(
            &mut out,
            &mut pinned,
            Folder::Host(host.alias.to_string()),
            ordered,
            input,
        );
    }

    let mut local: Vec<Listed> = input
        .open
        .iter()
        .filter(|o| o.host.is_none())
        .enumerate()
        .map(|(index, o)| Listed {
            name: local_name(input.local_label, index),
            pane: Some(o.pane),
            server: None,
            tmux: false,
            agent: false,
        })
        .collect();
    local.extend(input.local_detached.iter().map(|(name, agent)| Listed {
        name: name.clone(),
        pane: None,
        server: None,
        tmux: false,
        agent: *agent,
    }));
    push_folder(&mut out, &mut pinned, Folder::Local, local, input);

    if pinned.is_empty() {
        return out;
    }
    let mut top = vec![Row::NeedsYou];
    top.append(&mut pinned);
    top.append(&mut out);
    top
}

fn push_folder(
    out: &mut Vec<Row>,
    pinned: &mut Vec<Row>,
    folder: Folder,
    sessions: Vec<Listed>,
    input: &Input,
) {
    // Those needing the user go to the top, open folder or closed.
    let (needing, sessions): (Vec<_>, Vec<_>) = sessions
        .into_iter()
        .partition(|s| (input.needs_you)(&folder, &s.name, s.pane, s.server));
    for s in needing {
        pinned.push(Row::Session {
            folder: folder.clone(),
            name: s.name,
            pane: s.pane,
            server: s.server,
            tmux: s.tmux,
            agent: s.agent,
            pinned: true,
        });
    }
    let open = input
        .expanded
        .get(&folder)
        .copied()
        .unwrap_or(!sessions.is_empty());
    let agent = !open && sessions.iter().any(|s| s.agent);
    out.push(Row::Folder {
        folder: folder.clone(),
        expanded: open,
        agent,
    });
    if !open {
        return;
    }
    for s in sessions {
        out.push(Row::Session {
            folder: folder.clone(),
            name: s.name,
            pane: s.pane,
            server: s.server,
            tmux: s.tmux,
            agent: s.agent,
            pinned: false,
        });
    }
    out.push(Row::NewTerminal(folder));
}

/// A local server session no tab shows, as the tree names it.
pub struct LocalSession<'a> {
    pub id: u64,
    /// What has its terminal now.
    pub program: Option<&'a str>,
    pub cwd: Option<&'a str>,
}

/// Names for local server sessions no tab shows (D50): what runs and where,
/// `claude · ~/berth`, so several can be told apart. The shell's label and
/// the session's number only when neither is known, and the number added
/// wherever two would otherwise read the same.
pub fn local_session_names(
    sessions: &[LocalSession],
    label: &str,
    home: Option<&str>,
) -> Vec<(u64, String)> {
    let short = |dir: &str| -> String {
        match home {
            Some(home) if dir == home => "~".to_string(),
            Some(home) => match dir.strip_prefix(home).and_then(|r| r.strip_prefix('/')) {
                Some(rest) => format!("~/{rest}"),
                None => dir.to_string(),
            },
            None => dir.to_string(),
        }
    };
    let named: Vec<(u64, String)> = sessions
        .iter()
        .map(|s| {
            let name = match (s.program, s.cwd) {
                (Some(program), Some(dir)) => format!("{program} \u{b7} {}", short(dir)),
                (Some(program), None) => program.to_string(),
                (None, Some(dir)) => format!("{label} \u{b7} {}", short(dir)),
                (None, None) => format!("{label} #{}", s.id),
            };
            (s.id, name)
        })
        .collect();
    named
        .iter()
        .map(|(id, name)| {
            let twice = named.iter().filter(|(_, other)| other == name).count() > 1;
            if twice && !name.ends_with(&format!("#{id}")) {
                (*id, format!("{name} #{id}"))
            } else {
                (*id, name.clone())
            }
        })
        .collect()
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
        .filter(|o| o.host == Some(alias) && o.server.is_none())
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

    #[test]
    fn a_sessions_program_is_its_agent_or_its_first_pane() {
        let scan = running(&[
            ("berth/api", "zsh"),
            ("berth/api", "claude"),
            ("berth/api", "vim"),
            ("berth/logs", "tail"),
            ("berth/logs", "zsh"),
            ("mine", "claude"),
            ("berth/", "zsh"),
        ]);
        let programs = programs(&scan);
        assert_eq!(programs.get("api").map(String::as_str), Some("claude"));
        assert_eq!(programs.get("logs").map(String::as_str), Some("tail"));
        assert_eq!(programs.len(), 2, "only Berth's own sessions");
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
            host_servers: &BTreeMap::new(),
            on_server: &BTreeSet::new(),
            needs_you: &|_, _, _, _| false,
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

    /// A session that needs the user is pinned to the top, across hosts,
    /// out of its folder even when that is closed, and the group is there
    /// only while it has something in it.
    #[test]
    fn sessions_that_need_you_are_pinned_to_the_top() {
        let hosts = [host("web", None), host("db", None)];
        let mut running = BTreeMap::new();
        running.insert(
            "web".to_string(),
            BTreeMap::from([("main".to_string(), true), ("logs".to_string(), false)]),
        );
        running.insert(
            "db".to_string(),
            BTreeMap::from([("main".to_string(), true)]),
        );
        let mut expanded = BTreeMap::new();
        expanded.insert(Folder::Host("db".into()), false);
        let needs = |folder: &Folder, name: &str, _: Option<u64>, _: Option<u64>| {
            name == "main" && *folder != Folder::Local
        };
        let rows = rows(&Input {
            hosts: &hosts,
            open: &[],
            running: &running,
            expanded: &expanded,
            local_label: "zsh",
            local_detached: &[],
            host_servers: &BTreeMap::new(),
            on_server: &BTreeSet::new(),
            needs_you: &needs,
        });
        assert_eq!(rows[0], Row::NeedsYou);
        let pinned: Vec<(Folder, String)> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Session {
                    folder,
                    name,
                    pinned: true,
                    ..
                } => Some((folder.clone(), name.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            pinned,
            [
                (Folder::Host("web".into()), "main".to_string()),
                (Folder::Host("db".into()), "main".to_string())
            ]
        );
        assert_eq!(
            session_names(&rows, &web()),
            ["main", "logs"],
            "pinned and under"
        );
        let under: Vec<&Row> = rows
            .iter()
            .filter(|r| matches!(r, Row::Session { pinned: false, .. }))
            .collect();
        assert_eq!(under.len(), 1, "only logs is left under its folder");
        let quiet = build(&hosts, &[], &running, &expanded);
        assert!(!quiet.contains(&Row::NeedsYou));
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
            server: None,
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
            server: None,
            tmux: false,
            agent: false,
            pinned: false,
        }));
        assert!(rows.contains(&Row::Session {
            folder: web(),
            name: "deploy".into(),
            pane: None,
            server: None,
            tmux: false,
            agent: true,
            pinned: false,
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
            server: None,
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
                server: None,
            },
            OpenSession {
                pane: 5,
                host: None,
                slug: "local",
                server: None,
            },
        ];
        let rows = build(&[], &open, &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(session_names(&rows, &Folder::Local), ["zsh", "zsh 2"]);
        assert!(rows.contains(&Row::Session {
            folder: Folder::Local,
            name: "zsh 2".into(),
            pane: Some(5),
            server: None,
            tmux: false,
            agent: false,
            pinned: false,
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
            server: None,
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
            server: None,
        }];
        let detached = [("zsh #4".to_string(), true)];
        let rows = rows(&Input {
            hosts: &[],
            open: &open,
            running: &BTreeMap::new(),
            expanded: &BTreeMap::new(),
            local_label: "zsh",
            local_detached: &detached,
            host_servers: &BTreeMap::new(),
            on_server: &BTreeSet::new(),
            needs_you: &|_, _, _, _| false,
        });
        assert_eq!(session_names(&rows, &Folder::Local), ["zsh", "zsh #4"]);
        assert!(rows.contains(&Row::Session {
            folder: Folder::Local,
            name: "zsh #4".into(),
            pane: None,
            server: None,
            tmux: false,
            agent: true,
            pinned: false,
        }));
    }

    fn session(
        id: u64,
        program: Option<&'static str>,
        cwd: Option<&'static str>,
    ) -> LocalSession<'static> {
        LocalSession { id, program, cwd }
    }

    #[test]
    fn a_detached_local_session_is_named_by_what_runs_and_where() {
        let names = local_session_names(
            &[
                session(1, Some("claude"), Some("/Users/j/berth")),
                session(2, Some("zsh"), Some("/Users/j/work/api")),
                session(3, Some("zsh"), Some("/Users/j")),
                session(4, Some("top"), Some("/opt/x")),
            ],
            "zsh",
            Some("/Users/j"),
        );
        let names: Vec<&str> = names.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "claude \u{b7} ~/berth",
                "zsh \u{b7} ~/work/api",
                "zsh \u{b7} ~",
                "top \u{b7} /opt/x"
            ]
        );
    }

    #[test]
    fn the_number_is_used_only_when_nothing_else_tells_them_apart() {
        let names = local_session_names(
            &[
                session(5, None, None),
                session(6, Some("vim"), None),
                session(7, None, Some("/srv")),
                session(8, Some("claude"), Some("/a")),
                session(9, Some("claude"), Some("/a")),
            ],
            "zsh",
            None,
        );
        let names: Vec<&str> = names.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "zsh #5",
                "vim",
                "zsh \u{b7} /srv",
                "claude \u{b7} /a #8",
                "claude \u{b7} /a #9"
            ]
        );
    }

    #[test]
    fn taken_counts_open_and_detached_sessions() {
        let open = [OpenSession {
            pane: 1,
            host: Some("web"),
            slug: "main",
            server: None,
        }];
        let running = BTreeMap::from([(
            "web".to_string(),
            BTreeMap::from([("main-2".to_string(), false)]),
        )]);
        let taken = taken("web", &open, &running);
        assert_eq!(taken.into_iter().collect::<Vec<_>>(), ["main", "main-2"]);
    }

    /// A host that has moved to its server lists the server's sessions
    /// first, open or not, each with its id, and then the tmux sessions it
    /// still has, marked. An open server pane is never also a tmux row,
    /// and a host still on tmux marks nothing.
    #[test]
    fn a_host_on_its_server_lists_server_sessions_then_marked_tmux_ones() {
        let hosts = [host("web", None), host("db", None)];
        let open = [OpenSession {
            pane: 9,
            host: Some("web"),
            slug: "#3",
            server: Some(3),
        }];
        let mut running = BTreeMap::new();
        running.insert(
            "web".to_string(),
            BTreeMap::from([("main".to_string(), false)]),
        );
        running.insert(
            "db".to_string(),
            BTreeMap::from([("main".to_string(), false)]),
        );
        let mut host_servers = BTreeMap::new();
        host_servers.insert(
            "web".to_string(),
            vec![
                ServerRow {
                    id: 3,
                    name: "claude \u{b7} ~/berth".into(),
                    pane: Some(9),
                    agent: true,
                },
                ServerRow {
                    id: 5,
                    name: "zsh \u{b7} ~".into(),
                    pane: None,
                    agent: false,
                },
            ],
        );
        let on_server = BTreeSet::from(["web".to_string()]);
        let rows = rows(&Input {
            hosts: &hosts,
            open: &open,
            running: &running,
            expanded: &BTreeMap::new(),
            local_label: "zsh",
            local_detached: &[],
            host_servers: &host_servers,
            on_server: &on_server,
            needs_you: &|_, _, _, _| false,
        });
        let web: Vec<(String, Option<u64>, Option<u64>, bool)> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Session {
                    folder,
                    name,
                    pane,
                    server,
                    tmux,
                    ..
                } if *folder == Folder::Host("web".into()) => {
                    Some((name.clone(), *pane, *server, *tmux))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            web,
            [
                ("claude \u{b7} ~/berth".to_string(), Some(9), Some(3), false),
                ("zsh \u{b7} ~".to_string(), None, Some(5), false),
                ("main".to_string(), None, None, true),
            ]
        );
        assert!(rows.contains(&Row::Session {
            folder: Folder::Host("db".into()),
            name: "main".into(),
            pane: None,
            server: None,
            tmux: false,
            agent: false,
            pinned: false,
        }));
        let taken = taken("web", &open, &running);
        assert_eq!(taken.into_iter().collect::<Vec<_>>(), ["main"]);
    }
}
