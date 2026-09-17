//! The root view: sidebar, tabs, main area, status bar.

mod machine;
mod render;
mod trust;

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AppContext, AsyncApp, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, StatefulInteractiveElement, Styled, WeakEntity, Window,
    div, px,
};

use berth_core::config::SshConfig;
use berth_core::connection::ConnectionState;
use berth_core::connection::state::{Action, Event};
use berth_core::forward::Forwarder;
use berth_core::host::Host;
use berth_core::transport::Connection;

use crate::panes::{Axis, Node};
use crate::strings;
use crate::terminal_view::TerminalView;
use crate::text_field::{Outcome, TextField};
use crate::theme::{Appearance, Theme, hsla};
use crate::{runtime, session};
use gpui::svg;

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

/// One open session, and the view onto it.
///
/// Remote sessions are a tmux session on a host. Local ones are a shell on this
/// machine with no connection, no tmux and no reconnect machine (D36), so the
/// absence is in the type. A synthetic host would have been less code and would
/// collide the first time someone has an ssh config entry aliased `local`.
struct Pane {
    /// Stable across reordering and closing, unlike an index.
    id: u64,
    host: Option<Host>,
    slug: String,
    view: Entity<TerminalView>,
}

impl Pane {
    /// What the sidebar, tabs and status bar call this session.
    fn alias(&self) -> &str {
        match &self.host {
            Some(host) => &host.alias,
            None => strings::LOCAL_ALIAS,
        }
    }

    fn is_local(&self) -> bool {
        self.host.is_none()
    }

    fn is_prod(&self) -> bool {
        self.host.as_ref().is_some_and(|h| h.is_prod())
    }

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
    /// Renaming a session. The tmux name moves with the label, or a reattach
    /// would find the old one and start a second shell beside it.
    Rename {
        pane: u64,
        field: TextField,
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
    /// tmux session names with an agent running in them, by host. Presence
    /// only, never what the agent is doing (D34).
    agents: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
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
        let override_path = berth_core::env::ssh_config();
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
            agents: std::collections::BTreeMap::new(),
            statuses: std::collections::BTreeMap::new(),
            started: std::time::Instant::now(),
            cold_start_reported: false,
        };

        this.warm(cx);
        this
    }

    // -- sessions ----------------------------------------------------------

    fn pane_mut(&mut self, id: u64) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

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
        self.panes.iter().find(|p| p.alias() == alias).map(|p| p.id)
    }

    /// The connection a pane's session runs on.
    fn conn_of(&self, id: u64) -> Option<&Conn> {
        self.pane(id).and_then(|p| self.conns.get(p.alias()))
    }

    /// How many sessions are still using a host's connection.
    ///
    /// The connection outlives any one of them, so it is only retired when the
    /// last one goes. Splits are what make this greater than one.
    fn sessions_on(&self, alias: &str) -> usize {
        self.panes.iter().filter(|p| p.alias() == alias).count()
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
        let taken: Vec<&str> = self
            .panes
            .iter()
            .filter(|p| p.alias() == alias)
            .map(|p| p.slug.as_str())
            .collect();
        free_slug(&Self::slug(), &taken)
    }

    fn slug() -> String {
        berth_core::env::session().unwrap_or_else(|| berth_core::session::DEFAULT_SLUG.to_string())
    }

    /// Take a freshly connected session and make it a tab.
    ///
    /// Everything that makes a session a session happens here, once, so a first
    /// connect and a warm one cannot drift into behaving differently.
    #[allow(clippy::too_many_arguments)]
    fn adopt(
        &mut self,
        host: Option<Host>,
        slug: String,
        connected: session::Connected,
        forwarders: Vec<Forwarder>,
        installed: Vec<String>,
        place: Place,
        cx: &mut Context<Self>,
    ) {
        let alias = host
            .as_ref()
            .map(|h| h.alias.clone())
            .unwrap_or_else(|| strings::LOCAL_ALIAS.to_string());

        let _ = self.place_pane(
            host,
            slug,
            connected.terminal.clone(),
            connected.pty.clone(),
            connected.events,
            installed,
            place,
            cx,
        );

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

        self.statuses.insert(
            alias.clone(),
            HostStatus::Connection(ConnectionState::Connected),
        );
        self.report_cold_start(&alias);
        // One driver and one measurement per connection, not per session.
        if fresh {
            self.drive(alias.clone(), cx);
            self.measure_rtt(alias.clone(), cx);
            self.watch_agents(alias, cx);
        }
        cx.notify();
    }

    /// Open a shell on this machine.
    ///
    /// No connection, no tmux, no reconnect machine, because there is nothing
    /// to reconnect to. The view, the pump and therefore the recording are the
    /// same ones a remote session uses (D36).
    fn open_local(&mut self, place: Place, cx: &mut Context<Self>) {
        tracing::info!(shell = %berth_core::local::login_shell(), "opening a local shell");
        cx.spawn(async move |this, cx| {
            let palette = crate::terminal_style::TerminalStyle::default().palette;
            let installed = cx.update(|cx| cx.text_system().all_font_names());
            let opened = runtime::spawn(session::open_local(80, 24, palette))
                .await
                .map_err(|_| strings::RUNTIME_DROPPED.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));

            match opened {
                Ok(local) => {
                    let _ = this.update(cx, |this, cx| {
                        this.place_pane(
                            None,
                            strings::LOCAL_ALIAS.to_string(),
                            local.terminal,
                            local.pty,
                            local.events,
                            installed,
                            place,
                            cx,
                        );
                        this.report_cold_start(strings::LOCAL_ALIAS);
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "could not open a local shell");
                    let _ = this.update(cx, |this, cx| {
                        this.warnings.push(error);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// Build the view for a session and put it where it belongs.
    ///
    /// The half of adopting a session that has nothing to do with transports,
    /// so a local shell and a remote one share it rather than drifting apart.
    #[allow(clippy::too_many_arguments)]
    fn place_pane(
        &mut self,
        host: Option<Host>,
        slug: String,
        terminal: crate::terminal_view::SharedTerminal,
        pty: berth_core::pty::Pty,
        events: tokio::sync::mpsc::Receiver<berth_core::pty::PtyEvent>,
        installed: Vec<String>,
        place: Place,
        cx: &mut Context<Self>,
    ) -> u64 {
        let view = cx.new(|cx| {
            TerminalView::new(
                terminal,
                Some(pty),
                {
                    let mut style = crate::terminal_style::TerminalStyle::default();
                    style.resolve_family(&installed);
                    style
                },
                cx,
            )
        });
        view.update(cx, |_, cx| {
            session::start_pump(&cx.entity(), events, cx);
        });

        self.next_pane_id += 1;
        let id = self.next_pane_id;
        self.panes.push(Pane {
            id,
            host,
            slug,
            view,
        });

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
                    // The pane being split went away while this was opening.
                    // The session is real, so it keeps its place in the list
                    // rather than being dropped on the floor.
                    return id;
                };
                let tab = &mut self.tabs[index];
                tab.root.split(pane, axis, id);
                // The new pane takes the keyboard, as it does in every other
                // splitting editor: you split in order to type in the new one.
                tab.focus = id;
                self.active = index;
            }
        }

        cx.notify();
        id
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

        // Nothing configured is not an error and should not be a wall. Open a
        // shell here so there is something to type into immediately, and let
        // the sidebar explain where hosts come from.
        if hosts.is_empty() {
            self.open_local(Place::Tab { show: true }, cx);
            return;
        }

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
                        this.adopt(
                            Some(host),
                            slug,
                            connected,
                            forwarders,
                            installed,
                            place,
                            cx,
                        );
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
        startup_alias(&self.hosts, berth_core::env::host().as_deref())
            .and_then(|alias| self.hosts.iter().find(|h| h.alias == alias).cloned())
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
        let from = pane.id;
        let place = Place::Beside { pane: from, axis };

        // Splitting a local pane opens another local shell. There is no
        // connection to ride and no tmux session to name.
        if pane.is_local() {
            self.open_local(place, cx);
            return;
        }

        let host = pane.host.clone();
        let alias = pane.alias().to_string();
        let Some(connection) = self.conns.get(&alias).map(|c| c.connection.clone()) else {
            return;
        };
        let slug = self.free_slug(&alias);
        tracing::info!(%alias, %slug, ?axis, "splitting");

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
        let alias = pane.alias().to_string();
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
        let Some(pane) = self.pane(id).filter(|p| !p.is_local()) else {
            return;
        };
        self.modal = Some(Modal::Kill {
            pane: id,
            alias: pane.alias().to_string(),
            session: pane.tmux_name(),
        });
        self.modal_focused = false;
        cx.notify();
    }

    /// Whether a session name is already in use on the same host.
    fn slug_taken(&self, alias: &str, slug: &str, except: u64) -> bool {
        self.panes
            .iter()
            .any(|p| p.id != except && p.alias() == alias && p.slug == slug)
    }

    fn ask_rename(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(pane) = self.pane(id) else {
            return;
        };
        self.modal = Some(Modal::Rename {
            pane: id,
            field: TextField::new(pane.slug.clone()),
        });
        self.modal_focused = false;
        cx.notify();
    }

    /// Rename a session, on the server and here, together.
    fn rename(&mut self, id: u64, to: String, cx: &mut Context<Self>) {
        let to = berth_core::session::Session::new(to, 80, 24).slug;
        let Some(pane) = self.pane(id) else {
            return;
        };
        let alias = pane.alias().to_string();
        let from = pane.slug.clone();
        if to.is_empty() || to == from {
            return;
        }
        if self.slug_taken(&alias, &to, id) {
            // Two sessions with one name on one host would attach to the same
            // tmux session and mirror each other.
            self.warnings.push(strings::rename_taken(&to, &alias));
            cx.notify();
            return;
        }

        // A local session has no tmux behind it, so its name is only a label
        // and there is nothing on a server to keep in step.
        let remote = self
            .pane(id)
            .filter(|p| !p.is_local())
            .and_then(|_| self.conns.get(&alias))
            .map(|c| c.connection.clone());

        if let Some(connection) = remote {
            let command = berth_core::session::Session::new(from.clone(), 80, 24)
                .rename_command(&berth_core::session::Session::new(to.clone(), 80, 24));
            let named = to.clone();
            cx.spawn(async move |_, _| {
                match runtime::spawn(async move { connection.exec(&command).await }).await {
                    Ok(Ok(out)) if out.succeeded() => {
                        tracing::info!(%from, to = %named, "session renamed")
                    }
                    Ok(Ok(out)) => {
                        tracing::warn!(%from, stderr = %out.stderr_string(), "rename failed")
                    }
                    Ok(Err(error)) => tracing::warn!(%from, %error, "rename failed"),
                    Err(_) => tracing::warn!(%from, "the runtime dropped the rename"),
                }
            })
            .detach();
        }

        if let Some(pane) = self.pane_mut(id) {
            pane.slug = to;
        }
        cx.notify();
    }

    /// End the tmux session and everything running in it, then close the tab.
    fn kill(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(pane) = self.pane(id) else {
            return;
        };
        let name = pane.tmux_name();
        let command = berth_core::session::Session::new(pane.slug.clone(), 80, 24).kill_command();
        tracing::warn!(alias = %pane.alias(), session = %name, "killing session");
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
}

/// The display refresh rate, which sets the floor on render latency.
/// Overridable for measurement on a machine whose rate we cannot query.
fn refresh_hz() -> f64 {
    berth_core::env::refresh_hz().unwrap_or(60.0)
}

/// The first session name on this host that nothing is using.
///
/// A slug names a tmux session on the server, so two panes on one host must
/// never agree on one: the second would attach to the first's shell instead of
/// opening its own.
fn free_slug(base: &str, taken: &[&str]) -> String {
    if !taken.contains(&base) {
        return base.to_string();
    }
    // At most `taken.len()` of these can collide, so one is always free.
    // Bounded rather than open-ended so the total is visible here and there is
    // nothing to unwrap.
    (2..=taken.len() + 2)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken.contains(&candidate.as_str()))
        .unwrap_or_else(|| base.to_string())
}

/// Which host to open at startup, if any.
///
/// One host means there is nothing to choose between, so Berth opens it. Two or
/// more and it waits, because guessing costs a connection to a machine nobody
/// asked about, and one of them could be production.
fn startup_alias<'a>(hosts: &'a [Host], requested: Option<&'a str>) -> Option<&'a str> {
    match requested {
        Some(alias) => Some(alias),
        None if hosts.len() == 1 => hosts.first().map(|h| h.alias.as_str()),
        None => None,
    }
}

/// Whether the set of sessions running an agent on one host has moved.
///
/// Nothing recorded and nothing found is not a change. Treating it as one made
/// every host log its arrival at startup, for a picture that had always been
/// empty.
fn presence_changed(
    existing: Option<&std::collections::BTreeSet<String>>,
    found: &std::collections::BTreeSet<String>,
) -> bool {
    match existing {
        Some(existing) => existing != found,
        None => !found.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_free_slug_is_used_as_it_is() {
        assert_eq!(free_slug("main", &[]), "main");
        assert_eq!(free_slug("main", &["other"]), "main");
    }

    /// Two panes on one host must not name the same tmux session, or the second
    /// attaches to the first's shell instead of opening its own.
    #[test]
    fn a_taken_slug_gets_the_first_free_number() {
        assert_eq!(free_slug("main", &["main"]), "main-2");
        assert_eq!(free_slug("main", &["main", "main-2"]), "main-3");
        // A gap is filled rather than skipped.
        assert_eq!(free_slug("main", &["main", "main-3"]), "main-2");
    }

    /// The search is bounded, so this is the case that would have panicked if
    /// the bound were ever wrong.
    #[test]
    fn a_long_run_of_taken_slugs_still_finds_one() {
        let taken: Vec<String> = std::iter::once("main".to_string())
            .chain((2..40).map(|n| format!("main-{n}")))
            .collect();
        let refs: Vec<&str> = taken.iter().map(String::as_str).collect();
        assert_eq!(free_slug("main", &refs), "main-40");
    }

    fn hosts(aliases: &[&str]) -> Vec<Host> {
        aliases
            .iter()
            .map(|a| Host::new(*a, "example.com"))
            .collect()
    }

    #[test]
    fn one_host_opens_itself() {
        assert_eq!(startup_alias(&hosts(&["only"]), None), Some("only"));
    }

    /// Opening a guess costs a connection to a machine nobody asked about, and
    /// one of them could be production.
    #[test]
    fn several_hosts_open_nothing() {
        assert_eq!(startup_alias(&hosts(&["a", "b"]), None), None);
        assert_eq!(startup_alias(&hosts(&[]), None), None);
    }

    #[test]
    fn a_requested_host_wins_over_the_only_one() {
        assert_eq!(startup_alias(&hosts(&["a", "b"]), Some("b")), Some("b"));
        assert_eq!(startup_alias(&hosts(&["a"]), Some("a")), Some("a"));
    }

    fn sessions(names: &[&str]) -> std::collections::BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// The startup case: nothing known and nothing running is not news.
    #[test]
    fn nothing_running_and_nothing_known_is_not_a_change() {
        assert!(!presence_changed(None, &sessions(&[])));
    }

    #[test]
    fn an_agent_appearing_or_leaving_is_a_change() {
        assert!(presence_changed(None, &sessions(&["berth/main"])));
        assert!(presence_changed(
            Some(&sessions(&["berth/main"])),
            &sessions(&[])
        ));
        assert!(presence_changed(
            Some(&sessions(&["berth/main"])),
            &sessions(&["berth/deploy"])
        ));
    }

    /// The same picture twice must not repaint, or this is a wakeup every five
    /// seconds forever.
    #[test]
    fn the_same_agents_are_not_a_change() {
        let running = sessions(&["berth/main", "berth/deploy"]);
        assert!(!presence_changed(Some(&running), &running.clone()));
    }
}
