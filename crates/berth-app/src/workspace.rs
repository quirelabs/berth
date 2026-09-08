//! The root view: sidebar, tabs, main area, status bar.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AppContext, AsyncApp, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, StatefulInteractiveElement, Styled, WeakEntity, Window,
    div, px,
};

use std::path::PathBuf;

use berth_core::config::SshConfig;
use berth_core::connection::ConnectionState;
use berth_core::connection::state::{Action, Event};
use berth_core::forward::Forwarder;
use berth_core::host::Host;
use berth_core::transport::Connection;

use crate::panes::{Axis, Node};
use crate::strings;
use crate::terminal_view::TerminalView;
use crate::theme::{Appearance, Theme, hsla};
use crate::{runtime, session};

/// A tooltip body. gpui wants a view, and this is the whole of it.
struct Note {
    text: gpui::SharedString,
    theme: Theme,
}

impl Render for Note {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(320.0))
            .p(px(8.0))
            .rounded(px(4.0))
            .bg(hsla(self.theme.surface))
            .border_1()
            .border_color(hsla(self.theme.border))
            .text_xs()
            .text_color(hsla(self.theme.text))
            .child(self.text.clone())
    }
}

/// What the sidebar shows for one host.
#[derive(Debug, Clone)]
pub enum HostStatus {
    /// Nothing recorded in known_hosts, so warming it would raise a
    /// trust-on-first-use prompt. Ten dialogs on launch is a bad first run, so
    /// this host waits to be asked for and shows its fingerprint then.
    NeedsVerify,
    /// The probe is in flight. Berth is looking at the host's key and has
    /// authenticated to nothing.
    Verifying,
    /// Whatever the connection is doing, as that session's machine sees it.
    Connection(ConnectionState),
    /// Warming failed. Recorded per host so one bad host cannot stop the rest.
    Failed(String),
}

impl HostStatus {
    /// The dot colour a host gets, with `NeedsVerify` deliberately not
    /// borrowing Disconnected's grey: it is a thing to act on, not an absence.
    pub fn state(&self) -> ConnectionState {
        match self {
            HostStatus::Connection(state) => *state,
            HostStatus::Verifying => ConnectionState::Connecting,
            HostStatus::NeedsVerify | HostStatus::Failed(_) => ConnectionState::Disconnected,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            HostStatus::NeedsVerify => strings::STATE_VERIFY,
            HostStatus::Verifying => strings::STATE_VERIFYING,
            HostStatus::Failed(_) => strings::STATE_FAILED,
            HostStatus::Connection(state) => strings::connection_state(*state),
        }
    }
}

/// Everything that belongs to a host rather than to one of its sessions.
///
/// One connection per host, with a channel per session on it. Splitting a host
/// four ways must not present sshd with four handshakes, and the thing that
/// drops and comes back is the connection, so the reconnect machine lives here
/// and every session on it returns together.
///
/// One machine per connection, never one per workspace: a machine per workspace
/// could only watch whatever was on screen, so a background host that dropped
/// went unnoticed until someone looked at it.
struct Conn {
    connection: Arc<Connection>,
    /// Dropping one closes its listener, which is how retiring a host's last
    /// session retires the forwards that belonged to it.
    forwarders: Vec<Forwarder>,
    reconnect: crate::reconnect::Driver,
    rtt: Option<Duration>,
}

/// One open session: a tmux session on a host, and the view onto it.
struct Pane {
    /// Stable across reordering and closing, unlike an index.
    id: u64,
    host: Host,
    slug: String,
    view: Entity<TerminalView>,
}

impl Pane {
    fn tmux_name(&self) -> String {
        berth_core::session::Session::new(self.slug.clone(), 80, 24).tmux_name()
    }
}

/// Where a newly opened session goes.
#[derive(Clone, Copy)]
enum Place {
    /// A tab of its own. `show` when the user asked for this host by name.
    Tab { show: bool },
    /// Beside an existing pane, splitting it.
    Beside { pane: u64, axis: Axis },
}

/// A tab: a tree of panes, and which one has the keyboard.
///
/// The spec called for one tab per session. With splits that is the same thing
/// read a level up: a tab holds a tree, an unsplit tab is one session, and the
/// tab's label follows whichever pane is focused.
struct Tab {
    id: u64,
    root: Node,
    focus: u64,
}

/// A host key the user is being asked about.
///
/// Holds the key itself, not just its fingerprint, so accepting records exactly
/// what was displayed rather than asking the server again and recording
/// whatever comes back the second time.
struct Prompt {
    alias: String,
    endpoint: String,
    fingerprint: String,
    key: Box<berth_core::known_hosts::PublicKey>,
}

/// A question in front of everything else.
///
/// One at a time, and both of them are decisions that are awkward to undo: one
/// trusts a machine, the other ends a session and everything running in it.
enum Modal {
    Verify(Prompt),
    /// Killing is not closing. This exists so the destructive one has to be
    /// asked for twice and can never be reached by meaning the other.
    Kill {
        pane: u64,
        alias: String,
        session: String,
    },
}

/// What the main area shows when there is no session to show.
enum Empty {
    FirstRun,
    Connecting(String),
    Failed(String),
}

pub struct Workspace {
    hosts: Vec<Host>,
    /// One per host, shared by every session on it.
    conns: std::collections::BTreeMap<String, Conn>,
    /// Every open session, flat. The trees hold ids into this.
    panes: Vec<Pane>,
    tabs: Vec<Tab>,
    active: usize,
    next_pane_id: u64,
    next_tab_id: u64,
    modal: Option<Modal>,
    /// The modal takes the keyboard so Escape and Enter reach it rather than
    /// the terminal underneath.
    modal_focus: FocusHandle,
    modal_focused: bool,
    /// Which pane currently holds the keyboard, so switching moves it. Without
    /// this the user looks at one session and types into another.
    focused_pane: Option<u64>,
    /// Per-host status, so the sidebar can show hosts landing independently.
    statuses: std::collections::BTreeMap<String, HostStatus>,
    /// When the process started, for the cold-start measurement.
    started: std::time::Instant,
    /// Reported once, when the first host reaches a prompt.
    cold_start_reported: bool,
    warnings: Vec<String>,
    ssh_config_path: Option<String>,
    theme: Theme,
    empty: Empty,
}

impl Workspace {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Overridable so a test can point at a fixture instead of the user's
        // own ssh config, which Berth has no business writing to or depending
        // on the contents of.
        let override_path = std::env::var("BERTH_SSH_CONFIG").ok().map(PathBuf::from);
        let path = override_path
            .clone()
            .or_else(SshConfig::default_path)
            .map(|p| p.display().to_string());
        let loaded = match &override_path {
            Some(p) => SshConfig::load(p),
            None => SshConfig::load_default(),
        };
        let (hosts, warnings) = match loaded {
            Ok(config) => (config.hosts, config.warnings),
            Err(error) => {
                tracing::warn!(%error, "could not read ssh config");
                (Vec::new(), vec![error.to_string()])
            }
        };
        tracing::info!(hosts = hosts.len(), "loaded ssh config");

        let mut this = Self {
            hosts,
            warnings,
            ssh_config_path: path,
            theme: Theme::for_appearance(Appearance::Dark),
            empty: Empty::FirstRun,
            conns: std::collections::BTreeMap::new(),
            panes: Vec::new(),
            tabs: Vec::new(),
            active: 0,
            next_pane_id: 0,
            next_tab_id: 0,
            modal: None,
            modal_focus: cx.focus_handle(),
            modal_focused: false,
            focused_pane: None,
            statuses: std::collections::BTreeMap::new(),
            started: std::time::Instant::now(),
            cold_start_reported: false,
        };

        this.warm(cx);
        this
    }

    // -- sessions ----------------------------------------------------------

    fn pane(&self, id: u64) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    fn tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    /// The pane with the keyboard, which is what most commands act on.
    fn focused(&self) -> Option<&Pane> {
        self.pane(self.tab()?.focus)
    }

    /// The tab holding a pane, by index.
    fn tab_of(&self, pane: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.root.contains(pane))
    }

    /// A pane already showing this host, if there is one.
    fn pane_for(&self, alias: &str) -> Option<u64> {
        self.panes
            .iter()
            .find(|p| p.host.alias == alias)
            .map(|p| p.id)
    }

    /// The connection a pane's session runs on.
    fn conn_of(&self, id: u64) -> Option<&Conn> {
        self.pane(id).and_then(|p| self.conns.get(&p.host.alias))
    }

    /// How many sessions are still using a host's connection.
    ///
    /// The connection outlives any one of them, so it is only retired when the
    /// last one goes. Splits are what make this greater than one.
    fn sessions_on(&self, alias: &str) -> usize {
        self.panes.iter().filter(|p| p.host.alias == alias).count()
    }

    fn state_of(&self, id: u64) -> ConnectionState {
        self.conn_of(id)
            .map(|c| c.reconnect.state())
            .unwrap_or(ConnectionState::Disconnected)
    }

    /// A session name not already in use on this host.
    ///
    /// Splitting a host opens a second shell on it, which needs its own tmux
    /// session or both panes would attach to the same one and mirror each
    /// other. The base name is kept for the first, so an unsplit host still
    /// lands in `berth/main` and nothing about the common case changes.
    fn free_slug(&self, alias: &str) -> String {
        let base = Self::slug();
        let taken: Vec<&str> = self
            .panes
            .iter()
            .filter(|p| p.host.alias == alias)
            .map(|p| p.slug.as_str())
            .collect();
        if !taken.contains(&base.as_str()) {
            return base;
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|candidate| !taken.contains(&candidate.as_str()))
            .expect("the range is unbounded")
    }

    fn slug() -> String {
        std::env::var("BERTH_SESSION")
            .unwrap_or_else(|_| berth_core::session::DEFAULT_SLUG.to_string())
    }

    /// Take a freshly connected session and make it a tab.
    ///
    /// Everything that makes a session a session happens here, once, so a first
    /// connect and a warm one cannot drift into behaving differently.
    #[allow(clippy::too_many_arguments)]
    fn adopt(
        &mut self,
        host: Host,
        slug: String,
        connected: session::Connected,
        forwarders: Vec<Forwarder>,
        installed: Vec<String>,
        place: Place,
        cx: &mut Context<Self>,
    ) {
        let alias = host.alias.clone();
        let view = cx.new(|cx| {
            TerminalView::new(
                connected.terminal.clone(),
                Some(connected.pty.clone()),
                {
                    let mut style = crate::terminal_style::TerminalStyle::default();
                    style.resolve_family(&installed);
                    style
                },
                cx,
            )
        });
        view.update(cx, |_, cx| {
            session::start_pump(&cx.entity(), connected.events, cx);
        });

        self.next_pane_id += 1;
        let id = self.next_pane_id;

        // A second session on a host joins the connection that is already
        // there rather than opening another one.
        let fresh = !self.conns.contains_key(&alias);
        if fresh {
            let mut reconnect = crate::reconnect::Driver::new();
            reconnect.handle(Event::Open);
            reconnect.handle(Event::ConnectSucceeded);
            self.conns.insert(
                alias.clone(),
                Conn {
                    connection: connected.connection.clone(),
                    forwarders,
                    reconnect,
                    rtt: None,
                },
            );
        }

        self.panes.push(Pane {
            id,
            host,
            slug,
            view,
        });
        self.statuses.insert(
            alias.clone(),
            HostStatus::Connection(ConnectionState::Connected),
        );

        match place {
            Place::Tab { show } => {
                self.next_tab_id += 1;
                self.tabs.push(Tab {
                    id: self.next_tab_id,
                    root: Node::leaf(id),
                    focus: id,
                });
                // The first session to land is shown whether or not it was
                // asked for by name. Sitting on the first-run screen with a
                // live session behind it would be wrong, and whichever host
                // answered first is the one the user can type into soonest.
                if show || self.tabs.len() == 1 {
                    self.active = self.tabs.len() - 1;
                }
            }
            Place::Beside { pane, axis } => {
                let Some(index) = self.tab_of(pane) else {
                    return;
                };
                let tab = &mut self.tabs[index];
                tab.root.split(pane, axis, id);
                // The new pane takes the keyboard, as it does in every other
                // splitting editor: you split in order to type in the new one.
                tab.focus = id;
                self.active = index;
            }
        }

        self.report_cold_start(&alias);
        // One driver and one measurement per connection, not per session.
        if fresh {
            self.drive(alias.clone(), cx);
            self.measure_rtt(alias, cx);
        }
        cx.notify();
    }

    /// Open every host that is already trusted, in parallel, at startup.
    ///
    /// Warming is deliberately not a connection attempt for every host in the
    /// file. A host with nothing in known_hosts would raise a trust-on-first-use
    /// prompt, and a first run that opens ten dialogs is worse than one that
    /// opens none: those hosts are marked `NeedsVerify` and wait to be asked
    /// for, so the fingerprint is shown when the user is looking at that host
    /// rather than at a pile of modals.
    ///
    /// Each host gets its own task. One that is slow or refuses cannot hold up
    /// the others, which is the whole point of doing this at startup.
    fn warm(&mut self, cx: &mut Context<Self>) {
        let hosts = self.hosts.clone();
        let named = self.startup_host().map(|h| h.alias);

        for host in hosts {
            let asked_for = named.as_deref() == Some(host.alias.as_str());

            if !berth_core::known_hosts::is_known(&host.hostname, host.port) {
                self.statuses
                    .insert(host.alias.clone(), HostStatus::NeedsVerify);
                // The one host the user asked for by name is not warming: it is
                // the same request a click makes, so it gets the same answer,
                // a fingerprint to look at. Showing an empty pane instead would
                // be a launch that silently did nothing.
                if asked_for {
                    self.verify(host, cx);
                } else {
                    tracing::info!(
                        alias = %host.alias,
                        "not warming: no known_hosts entry, waiting for the user to verify"
                    );
                }
                continue;
            }

            self.statuses.insert(
                host.alias.clone(),
                HostStatus::Connection(ConnectionState::Connecting),
            );
            if asked_for {
                self.empty = Empty::Connecting(host.alias.clone());
            }
            self.connect(host, asked_for, cx);
        }
    }

    /// Bring a host up in a tab of its own, shown or not.
    fn connect(&mut self, host: Host, show: bool, cx: &mut Context<Self>) {
        let alias = host.alias.clone();
        tracing::info!(%alias, "connecting");
        let slug = self.free_slug(&alias);
        let forwards = host.forwards.clone();
        let place = Place::Tab { show };

        cx.spawn(async move |this, cx| {
            let palette = crate::terminal_style::TerminalStyle::default().palette;
            // Resolved once here, against what the platform actually has, so
            // the grid is not built on a proportional substitute.
            let installed = cx.update(|cx| cx.text_system().all_font_names());
            let result = runtime::spawn(session::connect(
                host.clone(),
                slug.clone(),
                80,
                24,
                palette,
            ))
            .await
            .map_err(|_| strings::RUNTIME_DROPPED.to_string())
            .and_then(|r| r.map_err(|e| e.to_string()));

            match result {
                Ok(connected) => {
                    // Same ordering as a reconnect: ports before the state.
                    let forwarders =
                        session::start_forwards(connected.connection.clone(), &forwards).await;
                    let _ = this.update(cx, |this, cx| {
                        this.adopt(host, slug, connected, forwarders, installed, place, cx);
                    });

                    // Benchmarks drive the real render loop, then quit.
                    if crate::bench::Bench::from_env().is_some() {
                        Self::run_benchmark(&this, cx).await;
                    }
                }
                Err(error) => {
                    // Recorded against this host only. A host that refuses must
                    // not take the others down with it.
                    tracing::warn!(%alias, %error, "connect failed");
                    let _ = this.update(cx, |this, cx| {
                        this.statuses
                            .insert(alias.clone(), HostStatus::Failed(error.clone()));
                        if this.tabs.is_empty() {
                            this.empty = Empty::Failed(error);
                        }
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// Report the first host to come up, once.
    ///
    /// The number that matters is the fastest host, not the average: what a
    /// user waits for at launch is something to type into.
    fn report_cold_start(&mut self, alias: &str) {
        if self.cold_start_reported {
            return;
        }
        self.cold_start_reported = true;
        let elapsed = self.started.elapsed();
        tracing::info!(
            alias = %alias,
            ms = elapsed.as_millis(),
            "cold start to a live session"
        );
        println!("COLD_START {} ms via {alias}", elapsed.as_millis());
    }

    fn startup_host(&self) -> Option<Host> {
        if let Ok(alias) = std::env::var("BERTH_HOST") {
            return self.hosts.iter().find(|h| h.alias == alias).cloned();
        }
        if self.hosts.len() == 1 {
            return self.hosts.first().cloned();
        }
        None
    }

    // -- opening, closing, killing ----------------------------------------

    /// Act on a click on a host row.
    ///
    /// What happens is decided from what is true now rather than from the
    /// status the sidebar last drew: a host with a session shows it, a host
    /// with no recorded key is verified first, anything else is connected. So a
    /// failed row is retried by clicking it, and a row that failed because it
    /// was never trusted asks the question instead of retrying the failure.
    fn open(&mut self, alias: String, cx: &mut Context<Self>) {
        // Never a connection started behind a dialog that is asking whether to
        // make one.
        if self.modal.is_some() {
            return;
        }
        if matches!(self.statuses.get(&alias), Some(HostStatus::Verifying)) {
            return;
        }
        // Already open somewhere: go to it rather than opening a second one.
        if let Some(pane) = self.pane_for(&alias)
            && let Some(index) = self.tab_of(pane)
        {
            self.active = index;
            self.tabs[index].focus = pane;
            cx.notify();
            return;
        }
        let Some(host) = self.hosts.iter().find(|h| h.alias == alias).cloned() else {
            return;
        };
        if berth_core::known_hosts::is_known(&host.hostname, host.port) {
            self.statuses
                .insert(alias, HostStatus::Connection(ConnectionState::Connecting));
            self.connect(host, true, cx);
        } else {
            self.verify(host, cx);
        }
    }

    fn activate(&mut self, tab: u64, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.iter().position(|t| t.id == tab) {
            self.active = index;
            cx.notify();
        }
    }

    /// Give a pane the keyboard, and show the tab holding it.
    fn focus_pane(&mut self, pane: u64, cx: &mut Context<Self>) {
        if let Some(index) = self.tab_of(pane) {
            self.active = index;
            self.tabs[index].focus = pane;
            cx.notify();
        }
    }

    /// Act on a workspace key chord.
    fn run(&mut self, command: crate::input::Command, cx: &mut Context<Self>) {
        use crate::input::Command;
        match command {
            Command::SplitRight => self.split(Axis::Horizontal, cx),
            Command::SplitDown => self.split(Axis::Vertical, cx),
            Command::ClosePane => {
                if let Some(pane) = self.focused().map(|p| p.id) {
                    self.close(pane, cx);
                }
            }
            Command::NextPane => {
                if let Some(next) = self.tab().and_then(|t| t.root.next_after(t.focus)) {
                    self.focus_pane(next, cx);
                }
            }
        }
    }

    /// Split the focused pane, opening a second session on the same host.
    ///
    /// The new session rides the connection that is already open, so splitting
    /// a host four ways is four channels and one handshake. It gets its own
    /// tmux session, because two panes attached to one would mirror each other
    /// rather than being two shells.
    fn split(&mut self, axis: Axis, cx: &mut Context<Self>) {
        let Some(pane) = self.focused() else {
            return;
        };
        let host = pane.host.clone();
        let alias = host.alias.clone();
        let from = pane.id;
        let Some(connection) = self.conns.get(&alias).map(|c| c.connection.clone()) else {
            return;
        };
        let slug = self.free_slug(&alias);
        tracing::info!(%alias, %slug, ?axis, "splitting");
        let place = Place::Beside { pane: from, axis };

        cx.spawn(async move |this, cx| {
            let palette = crate::terminal_style::TerminalStyle::default().palette;
            let installed = cx.update(|cx| cx.text_system().all_font_names());
            let result =
                runtime::spawn(session::open_on(connection, slug.clone(), 80, 24, palette))
                    .await
                    .map_err(|_| strings::RUNTIME_DROPPED.to_string())
                    .and_then(|r| r.map_err(|e| e.to_string()));

            match result {
                Ok(connected) => {
                    let _ = this.update(cx, |this, cx| {
                        this.adopt(host, slug, connected, Vec::new(), installed, place, cx);
                    });
                }
                Err(error) => {
                    // The existing panes are untouched. A split that could not
                    // open is one missing pane, not a broken tab.
                    tracing::warn!(%alias, %error, "split failed");
                    let _ = this.update(cx, |this, cx| {
                        this.warnings.push(error);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// Close a pane. This detaches and never kills.
    ///
    /// The connection goes and the forwards go with it, because both belong to
    /// this client. The tmux session on the server does not: it keeps running,
    /// and opening the host again re-attaches to the same shell with its
    /// scrollback and its running command intact. That is the whole reason
    /// sessions are tmux-backed, and it is why this is the cheap action.
    ///
    /// The connection is only retired with the host's last session, since every
    /// session on a host shares it, and the tab closes with its last pane.
    fn close(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(index) = self.panes.iter().position(|p| p.id == id) else {
            return;
        };
        let pane = self.panes.remove(index);
        let alias = pane.host.alias.clone();
        tracing::info!(%alias, session = %pane.tmux_name(), "detached");
        drop(pane);

        // Take the pane out of its tab, collapsing the split it was half of.
        // A tab with nothing left goes with it.
        if let Some(at) = self.tabs.iter().position(|t| t.root.contains(id)) {
            let tab = self.tabs.remove(at);
            if let Some(root) = tab.root.without(id) {
                let focus = if tab.focus == id {
                    root.panes().first().copied().unwrap_or(tab.focus)
                } else {
                    tab.focus
                };
                self.tabs.insert(at, Tab { root, focus, ..tab });
            } else if self.active > at {
                self.active -= 1;
            }
        }

        if self.sessions_on(&alias) == 0
            && let Some(mut conn) = self.conns.remove(&alias)
        {
            // Tell the machine rather than dropping it mid-backoff. Closing is
            // the one action that releases a grid, and this is the one place
            // that reaches the state which does.
            conn.reconnect.handle(Event::Close);
            debug_assert!(!conn.reconnect.holds_grid());
            self.statuses
                .insert(alias, HostStatus::Connection(ConnectionState::Disconnected));
            // Politeness, not correctness: dropping the handle would end it
            // anyway, but a clean disconnect saves the server a timeout.
            let connection = conn.connection.clone();
            drop(conn);
            cx.background_executor()
                .spawn(async move {
                    let _ = connection.disconnect().await;
                })
                .detach();
        }

        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        if self.tabs.is_empty() {
            self.empty = Empty::FirstRun;
        }
        cx.notify();
    }

    /// Ask before ending a session. Never kill directly from a click.
    fn ask_kill(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(pane) = self.pane(id) else {
            return;
        };
        self.modal = Some(Modal::Kill {
            pane: id,
            alias: pane.host.alias.clone(),
            session: pane.tmux_name(),
        });
        self.modal_focused = false;
        cx.notify();
    }

    /// End the tmux session and everything running in it, then close the tab.
    fn kill(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(pane) = self.pane(id) else {
            return;
        };
        let name = pane.tmux_name();
        let command = berth_core::session::Session::new(pane.slug.clone(), 80, 24).kill_command();
        tracing::warn!(alias = %pane.host.alias, session = %name, "killing session");
        let Some(connection) = self.conn_of(id).map(|c| c.connection.clone()) else {
            return;
        };

        cx.spawn(async move |_, _| {
            match runtime::spawn(async move { connection.exec(&command).await }).await {
                Ok(Ok(out)) if out.succeeded() => tracing::info!(%name, "session killed"),
                Ok(Ok(out)) => tracing::warn!(%name, stderr = %out.stderr_string(), "kill failed"),
                Ok(Err(error)) => tracing::warn!(%name, %error, "kill failed"),
                Err(_) => tracing::warn!(%name, "the runtime dropped the kill"),
            }
        })
        .detach();

        self.close(id, cx);
    }

    // -- trust -------------------------------------------------------------

    /// Ask a host for its key and put the fingerprint in front of the user.
    fn verify(&mut self, host: Host, cx: &mut Context<Self>) {
        let alias = host.alias.clone();
        let endpoint = format!("{}:{}", host.hostname, host.port);
        tracing::info!(%alias, "asking the host for its key");
        self.statuses.insert(alias.clone(), HostStatus::Verifying);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let offer = runtime::spawn(session::scout(host)).await;
            let _ = this.update(cx, |this, cx| {
                match offer {
                    Ok(Ok(session::Offer::Unknown { key, fingerprint })) => {
                        this.statuses.insert(alias.clone(), HostStatus::NeedsVerify);
                        this.modal = Some(Modal::Verify(Prompt {
                            alias,
                            endpoint,
                            fingerprint,
                            key,
                        }));
                        this.modal_focused = false;
                    }
                    // Recorded between startup and the click, so there is
                    // nothing to ask and the click meant "open this".
                    Ok(Ok(session::Offer::Trusted)) => {
                        if let Some(host) = this.hosts.iter().find(|h| h.alias == alias).cloned() {
                            this.statuses
                                .insert(alias, HostStatus::Connection(ConnectionState::Connecting));
                            this.connect(host, true, cx);
                        }
                    }
                    // Never offered as a question. There is nothing here a user
                    // can usefully be asked to accept.
                    Ok(Ok(session::Offer::Changed { line, fingerprint })) => {
                        tracing::error!(%alias, %line, %fingerprint, "host key has changed");
                        this.statuses.insert(
                            alias,
                            HostStatus::Failed(strings::changed_key(line, &fingerprint)),
                        );
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%alias, %error, "could not reach the host to see its key");
                        this.statuses
                            .insert(alias, HostStatus::Failed(error.to_string()));
                    }
                    Err(_) => {
                        this.statuses.insert(
                            alias,
                            HostStatus::Failed(strings::RUNTIME_DROPPED.to_string()),
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Say yes to whatever is being asked.
    fn confirm(&mut self, cx: &mut Context<Self>) {
        match self.modal.take() {
            Some(Modal::Verify(prompt)) => self.accept(prompt, cx),
            Some(Modal::Kill { pane, .. }) => self.kill(pane, cx),
            None => {}
        }
        cx.notify();
    }

    /// Say no. Nothing is written, nothing is ended.
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        match self.modal.take() {
            Some(Modal::Verify(prompt)) => {
                tracing::info!(alias = %prompt.alias, "host key declined");
                self.statuses.insert(prompt.alias, HostStatus::NeedsVerify);
            }
            Some(Modal::Kill { alias, .. }) => {
                tracing::info!(%alias, "kill cancelled");
            }
            None => {}
        }
        cx.notify();
    }

    /// Record the key the user accepted, then open the host.
    fn accept(&mut self, prompt: Prompt, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.iter().find(|h| h.alias == prompt.alias).cloned() else {
            return;
        };
        match session::trust(&host, &prompt.key) {
            Ok(()) => {
                tracing::info!(
                    alias = %prompt.alias,
                    fingerprint = %prompt.fingerprint,
                    "host key accepted and recorded"
                );
                self.statuses.insert(
                    prompt.alias,
                    HostStatus::Connection(ConnectionState::Connecting),
                );
                self.connect(host, true, cx);
            }
            Err(error) => {
                // Nothing was written, so the host stays a question. Connecting
                // anyway would be trusting a key that was never recorded, and
                // would ask again on the next launch with no explanation.
                tracing::error!(%error, alias = %prompt.alias, "could not record the accepted key");
                self.warnings.push(error.to_string());
                self.statuses.insert(prompt.alias, HostStatus::NeedsVerify);
            }
        }
    }

    // -- the connection machine, one per host ------------------------------

    /// Run one connection's clock: keepalives while connected, the slide into
    /// Degraded when they stop being answered, and the backoff after that.
    ///
    /// The loop sleeps until that machine's own next deadline rather than
    /// polling, and exits when the machine rests or when the connection is
    /// gone. The alias is what ends it: a retired connection stops resolving,
    /// so closing a host's last session ends its loop with no separate
    /// bookkeeping and two loops can never tick one machine.
    fn drive(&mut self, alias: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(Some(wait)) = this.read_with(cx, |this, _| {
                    this.conns.get(&alias).and_then(|c| c.reconnect.next_wait())
                }) else {
                    return;
                };
                cx.background_executor().timer(wait).await;

                let Ok(Some(step)) = this.update(cx, |this, cx| {
                    let step = this
                        .conns
                        .get_mut(&alias)
                        .map(|c| c.reconnect.handle(Event::Tick))?;
                    if step.changed {
                        tracing::info!(%alias, state = ?step.state, "connection state");
                        this.statuses
                            .insert(alias.clone(), HostStatus::Connection(step.state));
                        cx.notify();
                    }
                    Some(step)
                }) else {
                    return;
                };

                for action in step.actions {
                    match action {
                        Action::SendKeepalive => {
                            let Ok(Some(connection)) = this.read_with(cx, |this, _| {
                                this.conns.get(&alias).map(|c| c.connection.clone())
                            }) else {
                                return;
                            };
                            // Deliberately not awaited inline: a keepalive on a
                            // dead link blocks until its own timeout, and the
                            // machine has to keep ticking through that or it
                            // can never reach Degraded.
                            tracing::debug!(%alias, "keepalive");
                            let alive = this.clone();
                            let alias = alias.clone();
                            cx.spawn(async move |cx| {
                                if runtime::spawn(crate::reconnect::keepalive(connection))
                                    .await
                                    .unwrap_or(false)
                                {
                                    let _ = alive.update(cx, |this, cx| {
                                        let changed = this
                                            .conns
                                            .get_mut(&alias)
                                            .map(|c| {
                                                c.reconnect.handle(Event::KeepaliveAck).changed
                                            })
                                            .unwrap_or(false);
                                        if changed {
                                            cx.notify();
                                        }
                                    });
                                }
                            })
                            .detach();
                        }
                        Action::Connect => Self::reattach(&this, &alias, cx).await,
                        // Emitted after the machine is already Connected. The
                        // work it names is done before that, in `reattach`, so
                        // the state never claims a session that is not ready.
                        Action::Reattach => {}
                        Action::Teardown => return,
                    }
                }
            }
        })
        .detach();
    }

    /// Rebuild a host's transport underneath its live views.
    ///
    /// Every session on the host comes back together, because they share the
    /// connection that dropped. Each one re-attaches to its own tmux session at
    /// the size its grid already is, so tmux repaints into the same shape and
    /// nothing reflows on the way back.
    ///
    /// Everything the sessions need is done before `ConnectSucceeded` is fed
    /// in: the connection, the attaches, the pty swaps, the pumps, and the port
    /// forwards. The machine only returns to Connected once the host is
    /// genuinely usable, rather than announcing it and then catching up.
    async fn reattach(this: &WeakEntity<Self>, alias: &str, cx: &mut AsyncApp) {
        let Ok(Some(host)) = this.read_with(cx, |this, _| {
            this.panes
                .iter()
                .find(|p| p.host.alias == alias)
                .map(|p| p.host.clone())
        }) else {
            return;
        };
        let Ok(sessions) = this.read_with(cx, |this, _| {
            this.panes
                .iter()
                .filter(|p| p.host.alias == alias)
                .map(|p| (p.id, p.slug.clone(), p.view.clone()))
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        let forwards = host.forwards.clone();

        // The first session rebuilds the connection; the rest join it.
        let mut restored: Vec<(u64, session::Attached)> = Vec::new();
        let mut connection = None;
        for (id, slug, view) in sessions {
            let size = view.read_with(cx, |view, _| view.grid_size());
            let attached = match &connection {
                None => {
                    runtime::spawn(session::attach(host.clone(), slug, size.cols, size.rows)).await
                }
                Some(existing) => {
                    runtime::spawn(session::rejoin(
                        Arc::clone(existing),
                        slug,
                        size.cols,
                        size.rows,
                    ))
                    .await
                }
            };
            match attached {
                Ok(Ok(attached)) => {
                    connection.get_or_insert_with(|| attached.connection.clone());
                    restored.push((id, attached));
                }
                failed => {
                    if let Ok(Err(error)) = failed {
                        tracing::warn!(%alias, %error, "reattach failed");
                    }
                    let _ = this.update(cx, |this, cx| {
                        if let Some(conn) = this.conns.get_mut(alias) {
                            conn.reconnect.handle(Event::ConnectFailed);
                        }
                        cx.notify();
                    });
                    return;
                }
            }
        }
        let Some(connection) = connection else {
            return;
        };

        // Forwards come back before the state does, so nothing ever sees a host
        // reported usable with its ports dead. The old forwarders are dropped
        // first: their listeners belong to a connection that no longer exists,
        // and holding them would keep the local ports bound to nothing.
        let _ = this.update(cx, |this, _| {
            if let Some(conn) = this.conns.get_mut(alias) {
                conn.forwarders.clear();
            }
        });
        let forwarders = session::start_forwards(connection.clone(), &forwards).await;

        let _ = this.update(cx, |this, cx| {
            for (id, attached) in restored {
                let Some(view) = this.pane(id).map(|p| p.view.clone()) else {
                    continue;
                };
                view.update(cx, |view, cx| {
                    // The grid is untouched. tmux repaints over it in place.
                    view.swap_pty(attached.pty.clone());
                    session::start_pump(&cx.entity(), attached.events, cx);
                });
            }
            let Some(conn) = this.conns.get_mut(alias) else {
                return;
            };
            conn.connection = connection;
            conn.forwarders = forwarders;
            let step = conn.reconnect.handle(Event::ConnectSucceeded);
            let forwards = conn.forwarders.len();
            tracing::info!(%alias, state = ?step.state, forwards, "reattached");
            this.statuses
                .insert(alias.to_string(), HostStatus::Connection(step.state));
            cx.notify();
        });
    }

    /// A first RTT sample for the status bar.
    fn measure_rtt(&self, alias: String, cx: &mut Context<Self>) {
        let Some(connection) = self.conns.get(&alias).map(|c| c.connection.clone()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Ok(Some(rtt)) = runtime::spawn(session::round_trip(connection)).await {
                let _ = this.update(cx, |this, cx| {
                    if let Some(conn) = this.conns.get_mut(&alias) {
                        conn.rtt = Some(rtt);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    /// Drive the real render loop for a benchmark, then quit.
    async fn run_benchmark(this: &WeakEntity<Self>, cx: &mut AsyncApp) {
        let Some(mode) = crate::bench::Bench::from_env() else {
            return;
        };
        let Ok(Some((view, pty, connection))) = this.read_with(cx, |this, _| {
            let pane = this.focused()?;
            Some((
                pane.view.downgrade(),
                pane.view.read_with(cx, |v, _| v.pty())?,
                this.conn_of(pane.id)?.connection.clone(),
            ))
        }) else {
            return;
        };

        let report = match mode {
            crate::bench::Bench::Cat => {
                let prepared = runtime::spawn({
                    let connection = connection.clone();
                    async move {
                        connection
                            .exec(&crate::bench::create_file_command())
                            .await
                            .map(|o| o.stdout_string().trim().to_string())
                    }
                })
                .await;
                println!("benchmark file bytes: {prepared:?}");
                crate::bench::run_cat(
                    view,
                    pty,
                    std::time::Duration::from_secs(
                        std::env::var("BERTH_BENCH_SETTLE")
                            .ok()
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(20),
                    ),
                    cx,
                )
                .await
            }
            crate::bench::Bench::Keys => {
                crate::bench::run_keys(view, pty, 40, refresh_hz(), cx).await
            }
        };
        println!("\n{}", report.text);
        let _ = runtime::spawn(async move {
            let _ = connection.exec(&crate::bench::remove_file_command()).await;
        })
        .await;
        if !report.passed {
            // Exit non-zero so a regression is a failure, not a number someone
            // has to notice.
            eprintln!("benchmark did not meet its criteria");
            std::process::exit(1);
        }
        cx.update(|cx| cx.quit());
    }

    // -- rendering ---------------------------------------------------------

    fn warnings(&self) -> impl IntoElement {
        let theme = self.theme;
        let mut list = div().flex().flex_col().gap(px(2.0)).px(px(10.0));
        for warning in &self.warnings {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(hsla(theme.warning))
                    .child(warning.clone()),
            );
        }
        list
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let mut list = div().flex().flex_col().gap(px(2.0));
        let showing = self.focused().map(|p| p.host.alias.clone());

        for (index, host) in self.hosts.iter().enumerate() {
            let status = self
                .statuses
                .get(&host.alias)
                .cloned()
                .unwrap_or(HostStatus::Connection(ConnectionState::Disconnected));
            let needs_verify = matches!(status, HostStatus::NeedsVerify);
            let dot = if needs_verify {
                theme.warning.into()
            } else {
                hsla(theme.state_colour(status.state()))
            };

            let alias = host.alias.clone();
            let on_screen = showing.as_deref() == Some(alias.as_str());

            let mut row = div()
                .id(("host", index))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.open(alias.clone(), cx)))
                .hover(|row| row.bg(hsla(theme.border)))
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(10.0))
                .py(px(4.0))
                // Never shrink: flex was collapsing this to nothing to make
                // room for a long label, which took the state indicator away
                // from exactly the host that most needed one.
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(6.0))
                        .h(px(6.0))
                        .rounded(px(3.0))
                        .bg(dot),
                )
                .child(div().text_color(hsla(theme.text)).child(host.alias.clone()));

            // The row on screen is marked, so a sidebar of connected hosts
            // still says which one is being looked at.
            if on_screen {
                row = row.bg(hsla(theme.border));
            }

            // Only the states that need explaining carry a label. Connected is
            // the dot's job; "waiting for you" and "this one broke" are not.
            if needs_verify || matches!(status, HostStatus::Verifying | HostStatus::Failed(_)) {
                row = row.child(
                    div()
                        .text_xs()
                        .text_color(hsla(theme.text_muted))
                        .child(status.label()),
                );
            }

            // Why it failed, or why it was left alone, goes on the row: for
            // the same reason the forward note does, this is where someone is
            // looking when they ask.
            let note: Option<gpui::SharedString> = match &status {
                HostStatus::Failed(reason) => Some(reason.clone().into()),
                HostStatus::NeedsVerify => Some(strings::VERIFY_TOOLTIP.into()),
                HostStatus::Verifying | HostStatus::Connection(_) => None,
            };
            if let Some(note) = note {
                row = row.tooltip(move |_, cx| {
                    cx.new(|_| Note {
                        text: note.clone(),
                        theme,
                    })
                    .into()
                });
            }
            list = list.child(row);
        }

        let forwards = self
            .focused()
            .and_then(|p| self.conns.get(&p.host.alias))
            .map(|c| c.forwarders.iter().map(|f| f.forward()).collect::<Vec<_>>())
            .unwrap_or_default();
        for (index, forward) in forwards.into_iter().enumerate() {
            list = list.child(
                div()
                    .id(("forward", index))
                    .px(px(10.0))
                    .py(px(2.0))
                    .text_xs()
                    .text_color(hsla(theme.text_muted))
                    .child(forward.to_string())
                    // The bind clash is confusing exactly here and nowhere
                    // else, so the explanation lives on the row rather than in
                    // a document nobody has open at the time.
                    .tooltip(move |_, cx| {
                        cx.new(|_| Note {
                            text: strings::FORWARD_BIND_TOOLTIP.into(),
                            theme,
                        })
                        .into()
                    }),
            );
        }

        div()
            .w(px(220.0))
            .h_full()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .bg(hsla(theme.surface))
            .border_r(px(1.0))
            .border_color(hsla(theme.border))
            .py(px(10.0))
            .child(
                div()
                    .px(px(10.0))
                    .text_xs()
                    .text_color(hsla(theme.text_muted))
                    .child(strings::SIDEBAR_HOSTS),
            )
            .child(list)
            .child(self.warnings())
    }

    /// One tab per open session.
    ///
    /// Each carries the host's own colour and its connection state, so a row of
    /// tabs answers "which host" and "is it up" without being read word by
    /// word. The close and kill actions are on the active tab only: an inactive
    /// tab is a place to go, and putting a destructive control on it invites
    /// hitting one while aiming for the other.
    fn tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let mut strip = div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .h(px(30.0))
            .px(px(6.0))
            .bg(hsla(theme.surface))
            .border_b(px(1.0))
            .border_color(hsla(theme.border));

        for (index, tab) in self.tabs.iter().enumerate() {
            // A tab is named by its focused pane, so a split tab still says
            // which host you are looking at rather than which one it started
            // as.
            let Some(pane) = self.pane(tab.focus) else {
                continue;
            };
            let id = pane.id;
            let tab_id = tab.id;
            let active = index == self.active;
            let colour = theme.host_colour(&pane.host.alias, pane.host.is_prod());
            let split_count = tab.root.panes().len();

            let mut row = div()
                .id(("tab", tab_id as usize))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.activate(tab_id, cx)))
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(8.0))
                .py(px(4.0))
                .rounded(px(4.0))
                .text_xs()
                .hover(|t| t.bg(hsla(theme.border)))
                // The host's colour, as a bar rather than a dot, so it is not
                // mistaken for the state indicator beside it.
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(3.0))
                        .h(px(12.0))
                        .rounded(px(2.0))
                        .bg(colour),
                )
                .child(
                    div()
                        .text_color(hsla(if active { theme.text } else { theme.text_muted }))
                        .child(pane.host.alias.clone()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(6.0))
                        .h(px(6.0))
                        .rounded(px(3.0))
                        .bg(hsla(theme.state_colour(self.state_of(id)))),
                );

            // Split tabs say how many panes they hold, since only one of them
            // can be named.
            if split_count > 1 {
                row = row.child(
                    div()
                        .text_color(hsla(theme.text_muted))
                        .child(format!("{split_count}")),
                );
            }

            if active {
                row = row
                    .bg(hsla(theme.background))
                    .child(
                        div()
                            .id(("detach", tab_id as usize))
                            .cursor_pointer()
                            .px(px(4.0))
                            .text_color(hsla(theme.text_muted))
                            .hover(|b| b.text_color(hsla(theme.text)))
                            .on_click(cx.listener(move |this, _, _, cx| this.close(id, cx)))
                            .tooltip(move |_, cx| {
                                cx.new(|_| Note {
                                    text: strings::TAB_DETACH_TOOLTIP.into(),
                                    theme,
                                })
                                .into()
                            })
                            .child(strings::TAB_DETACH),
                    )
                    .child(
                        div()
                            .id(("kill", tab_id as usize))
                            .cursor_pointer()
                            .px(px(4.0))
                            .text_color(hsla(theme.text_muted))
                            .hover(|b| b.text_color(hsla(theme.warning)))
                            .on_click(cx.listener(move |this, _, _, cx| this.ask_kill(id, cx)))
                            .tooltip(move |_, cx| {
                                cx.new(|_| Note {
                                    text: strings::TAB_KILL_TOOLTIP.into(),
                                    theme,
                                })
                                .into()
                            })
                            .child(strings::TAB_KILL),
                    );
            }
            strip = strip.child(row);
        }
        strip
    }

    fn first_run(&self) -> impl IntoElement {
        let theme = self.theme;
        let path = self
            .ssh_config_path
            .clone()
            .unwrap_or_else(|| strings::FIRST_RUN_PATH_MISSING.to_string());

        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .max_w(px(460.0))
            .child(
                div()
                    .text_color(hsla(theme.text))
                    .child(strings::FIRST_RUN_TITLE),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(hsla(theme.text_muted))
                    .child(strings::FIRST_RUN_BODY),
            )
            .child(
                div()
                    .flex()
                    .gap(px(6.0))
                    .text_xs()
                    .text_color(hsla(theme.text_muted))
                    .child(strings::FIRST_RUN_PATH_LABEL)
                    .child(div().text_color(hsla(theme.text)).child(path)),
            )
    }

    /// A modal, over everything else.
    ///
    /// Modal on purpose: both questions it asks are ones where a wrong click
    /// costs something that is awkward to undo, so neither sits in a corner
    /// competing with a terminal for attention. Escape is always no and Enter
    /// is always yes, which is why the two share this shell.
    fn modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let mut body = div().flex().flex_col().gap(px(10.0));
        let (title, accept, reject, danger) = match self.modal.as_ref() {
            Some(Modal::Verify(prompt)) => {
                body = body
                    .child(
                        div()
                            .text_sm()
                            .text_color(hsla(theme.text_muted))
                            .child(strings::verify_body(&prompt.alias, &prompt.endpoint)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .items_center()
                            .p(px(8.0))
                            .rounded(px(4.0))
                            .bg(hsla(theme.background))
                            .border_1()
                            .border_color(hsla(theme.border))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(hsla(theme.text_muted))
                                    .child(strings::VERIFY_FINGERPRINT_LABEL),
                            )
                            .child(
                                div()
                                    .text_color(hsla(theme.text))
                                    .child(prompt.fingerprint.clone()),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(hsla(theme.text_muted))
                            .child(strings::VERIFY_HINT),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(hsla(theme.text_muted))
                            .child(strings::VERIFY_RECORDS),
                    );
                (
                    strings::VERIFY_TITLE,
                    strings::VERIFY_ACCEPT,
                    strings::VERIFY_REJECT,
                    false,
                )
            }
            Some(Modal::Kill { alias, session, .. }) => {
                body = body
                    .child(
                        div()
                            .text_sm()
                            .text_color(hsla(theme.text_muted))
                            .child(strings::kill_body(alias, session)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(hsla(theme.text_muted))
                            .child(strings::KILL_ALTERNATIVE),
                    );
                (
                    strings::KILL_TITLE,
                    strings::KILL_ACCEPT,
                    strings::KILL_REJECT,
                    true,
                )
            }
            None => return div(),
        };

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .track_focus(&self.modal_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => this.dismiss(cx),
                    "enter" => this.confirm(cx),
                    _ => {}
                }
            }))
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::hsla(0.0, 0.0, 0.0, 0.55))
            .child(
                div()
                    .w(px(480.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .p(px(16.0))
                    .rounded(px(4.0))
                    .bg(hsla(theme.surface))
                    .border_1()
                    .border_color(hsla(if danger { theme.warning } else { theme.border }))
                    .child(div().text_color(hsla(theme.text)).child(title))
                    .child(body)
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .justify_end()
                            .child(
                                div()
                                    .id("modal-reject")
                                    .cursor_pointer()
                                    .px(px(12.0))
                                    .py(px(6.0))
                                    .rounded(px(4.0))
                                    .border_1()
                                    .border_color(hsla(theme.border))
                                    .text_color(hsla(theme.text))
                                    .hover(|b| b.bg(hsla(theme.border)))
                                    .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx)))
                                    .child(reject),
                            )
                            .child(
                                div()
                                    .id("modal-accept")
                                    .cursor_pointer()
                                    .px(px(12.0))
                                    .py(px(6.0))
                                    .rounded(px(4.0))
                                    .bg(hsla(if danger { theme.warning } else { theme.accent }))
                                    .text_color(hsla(theme.background))
                                    .on_click(cx.listener(|this, _, _, cx| this.confirm(cx)))
                                    .child(accept),
                            ),
                    ),
            )
    }

    /// Lay the pane tree out, splits and all.
    ///
    /// The focused pane carries a border in the accent colour. With more than
    /// one terminal on screen and no cursor blinking to give it away, there has
    /// to be something that says where the keys are going.
    fn render_node(&self, node: &Node, focus: u64, cx: &mut Context<Self>) -> gpui::AnyElement {
        match node {
            Node::Leaf(id) => {
                let theme = self.theme;
                let focused = *id == focus;
                let Some(pane) = self.pane(*id) else {
                    return div().flex().flex_1().into_any_element();
                };
                let pane_id = *id;
                div()
                    .id(("pane", pane_id as usize))
                    // Clicking a pane focuses it, which is the only thing that
                    // makes several terminals on screen usable with a mouse.
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.focus_pane(pane_id, cx)),
                    )
                    .flex()
                    .flex_1()
                    .overflow_hidden()
                    .border_1()
                    .border_color(hsla(if focused { theme.accent } else { theme.border }))
                    .child(pane.view.clone())
                    .into_any_element()
            }
            Node::Split {
                axis,
                first,
                second,
            } => {
                let row = div().flex().flex_1().min_w_0().min_h_0();
                let row = match axis {
                    Axis::Horizontal => row.flex_row(),
                    Axis::Vertical => row.flex_col(),
                };
                row.child(
                    div()
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .child(self.render_node(first, focus, cx)),
                )
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .child(self.render_node(second, focus, cx)),
                )
                .into_any_element()
            }
        }
    }

    fn status_bar(&self) -> impl IntoElement {
        let theme = self.theme;
        let pane = self.focused();
        let rtt = match pane.and_then(|p| self.conns.get(&p.host.alias)?.rtt) {
            Some(rtt) => format!("{} ms", rtt.as_millis()),
            None => "-".to_string(),
        };
        let mut bar = div()
            .h(px(24.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .px(px(10.0))
            .bg(hsla(theme.surface))
            .border_t(px(1.0))
            .border_color(hsla(theme.border))
            .text_xs()
            .text_color(hsla(theme.text_muted))
            // The host actually on screen, which with warming is whichever
            // answered first and not necessarily the first in the file.
            .child(
                pane.map(|p| p.host.alias.clone())
                    .unwrap_or_else(|| strings::STATUS_NO_HOST.to_string()),
            )
            .child(strings::connection_state(
                pane.map(|p| self.state_of(p.id))
                    .unwrap_or(ConnectionState::Disconnected),
            ))
            .child(rtt)
            .child(strings::STATUS_NOT_RECORDING);

        // Only while there is nothing to see. A keyboard feature nobody can
        // find does not exist, and one that keeps explaining itself after you
        // have used it is worse.
        if self.tab().is_some_and(|t| t.root.panes().len() == 1) {
            bar = bar.child(
                div()
                    .ml_auto()
                    .text_color(hsla(theme.border))
                    .child(strings::SPLIT_HINT),
            );
        }
        bar
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Light and dark follow the OS.
        self.theme = Theme::for_appearance(window.appearance().into());
        let theme = self.theme;

        // A modal takes the keyboard, so Escape and Enter reach it rather than
        // the terminal underneath.
        if self.modal.is_some() {
            if !self.modal_focused {
                window.focus(&self.modal_focus, cx);
                self.modal_focused = true;
                self.focused_pane = None;
            }
        } else if let Some(pane) = self.focused()
            && self.focused_pane != Some(pane.id)
        {
            // Without this the terminal never receives a keystroke, and after a
            // switch or a split it would be the wrong terminal receiving them.
            let handle = pane.view.focus_handle(cx);
            self.focused_pane = Some(pane.id);
            window.focus(&handle, cx);
        }

        let showing = self.tab().map(|t| (t.root.clone(), t.focus));
        let main = match showing {
            Some((root, focus)) => div()
                .flex()
                .flex_1()
                .child(self.render_node(&root, focus, cx)),
            None => match &self.empty {
                Empty::Connecting(alias) => centred(
                    div()
                        .text_color(hsla(theme.text_muted))
                        .child(format!("{} {alias}", strings::STATE_CONNECTING)),
                ),
                Empty::Failed(error) => centred(
                    div()
                        .max_w(px(560.0))
                        .text_color(hsla(theme.warning))
                        .child(error.clone()),
                ),
                Empty::FirstRun => centred(self.first_run()),
            },
        };

        let mut pane = div().flex().flex_col().flex_1();
        if !self.tabs.is_empty() {
            pane = pane.child(self.tabs(cx));
        }
        pane = pane.child(main);

        let mut root = div()
            .size_full()
            .relative()
            // Workspace chords are handled here rather than in the terminal,
            // because they are about the window and not about the shell. Every
            // one is a Cmd chord, which berth-term already refuses to send to
            // the remote, so the shell loses nothing.
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if this.modal.is_some() {
                    return;
                }
                if let Some(command) = crate::input::command(&event.keystroke) {
                    this.run(command, cx);
                }
            }))
            .flex()
            .flex_col()
            .bg(hsla(theme.background))
            .text_sm()
            .child(div().flex().flex_1().child(self.sidebar(cx)).child(pane))
            .child(self.status_bar());

        if self.modal.is_some() {
            root = root.child(self.modal(cx));
        }
        root
    }
}

fn centred(child: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .child(child)
}

/// The display refresh rate, which sets the floor on render latency.
/// Overridable for measurement on a machine whose rate we cannot query.
fn refresh_hz() -> f64 {
    std::env::var("BERTH_REFRESH_HZ")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60.0)
}
