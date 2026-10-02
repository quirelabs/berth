//! The root view: sidebar, tabs, main area, status bar.

mod machine;
mod render;
mod saving;
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
use berth_core::settings::{DEFAULT_FONT_SIZE, FONT_SIZE_STEP};
use berth_core::transport::Connection;

use crate::panes::{Axis, Node};
use crate::strings;
use crate::terminal_view::TerminalView;
use crate::text_field::{Outcome, TextField};
use crate::theme::{Appearance, Theme, hsla};
use crate::{runtime, session};
use gpui::svg;

/// How often the local server's session list is read for the sidebar.
const LOCAL_LIST_EVERY: Duration = Duration::from_secs(3);

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
    /// A local shell's Claude, as its hooks report it (D48). The token is the
    /// pane's `BERTH_PANE`; signals carrying any other are not believed.
    claude: Option<ClaudePane>,
    /// A local session from a previous run whose tab has not been shown yet,
    /// so its shell has not started (D46).
    restored: Option<saving::Restored>,
    /// The local server's session this pane shows (D50).
    session: Option<u64>,
    /// Why this local pane runs a shell of its own instead, shown on it: it
    /// will not survive quitting (D50).
    direct: Option<String>,
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

/// What Berth knows about the Claude in one pane.
struct ClaudePane {
    token: berth_core::claude::PaneToken,
    tracker: berth_core::claude::Tracker,
}

impl ClaudePane {
    fn new(token: berth_core::claude::PaneToken) -> Self {
        Self {
            token,
            tracker: berth_core::claude::Tracker::default(),
        }
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
        /// The pane showing it, if it has one. A detached session has none.
        pane: Option<u64>,
        alias: String,
        slug: String,
        session: String,
    },
    /// Renaming a session. The tmux name moves with the label, or a reattach
    /// would find the old one and start a second shell beside it.
    Rename {
        pane: u64,
        field: TextField,
    },
    /// Naming a host. Kept in Berth's settings, never written to ssh config.
    RenameHost {
        alias: String,
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
    /// Berth's tmux sessions on each host, open or detached, with whether an
    /// agent is running in each. Presence only, never what it is doing (D34).
    running: std::collections::BTreeMap<String, std::collections::BTreeMap<String, bool>>,
    /// Sidebar folders opened or closed by hand.
    expanded: std::collections::BTreeMap<crate::sidebar::Folder, bool>,
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
    /// The right-click menu, what it is for and where it opened.
    menu: Option<(crate::menu::Target, gpui::Point<gpui::Pixels>)>,
    /// What Berth remembers between launches.
    settings: berth_core::settings::Settings,
    /// Where the Claude plugin and shim were put, or `None` if they could not
    /// be, in which case panes open without the shim (D48).
    claude_base: Option<std::path::PathBuf>,
    /// The local server, once started, or why local tabs run without it
    /// (D50).
    local_server: session::LocalServerCell,
    /// The local server's sessions, as last listed: what the sidebar shows
    /// under this machine besides the open tabs.
    local_sessions: Vec<berth_proto::v1::SessionInfo>,
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
            settings: berth_core::settings::Settings::load(),
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
            running: std::collections::BTreeMap::new(),
            expanded: std::collections::BTreeMap::new(),
            menu: None,
            claude_base: install_claude_support(),
            local_server: Default::default(),
            local_sessions: Vec::new(),
            statuses: std::collections::BTreeMap::new(),
            started: std::time::Instant::now(),
            cold_start_reported: false,
        };

        this.restore_local(cx);
        this.warm(cx);
        this.watch_saves(cx);
        this.watch_local_sessions(cx);
        this
    }

    /// Why local tabs skip the local server, if they do: the dogfooding
    /// escape hatch, by environment or setting (D50).
    fn local_direct(&self) -> Option<String> {
        if berth_core::env::local_direct() {
            Some(strings::LOCAL_DIRECT_BY_ENV.to_string())
        } else if self.settings.local_direct {
            Some(strings::LOCAL_DIRECT_BY_SETTING.to_string())
        } else {
            None
        }
    }

    /// Keep the local server's session list current for the sidebar. Only
    /// once the server is up: this never starts it.
    fn watch_local_sessions(&self, cx: &mut Context<Self>) {
        let cell = self.local_server.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(LOCAL_LIST_EVERY).await;
                let Some(Ok(server)) = cell.get().cloned() else {
                    if this.update(cx, |_, _| ()).is_err() {
                        return;
                    }
                    continue;
                };
                let listed = runtime::spawn(async move { server.list().await })
                    .await
                    .ok()
                    .and_then(|r| r.ok());
                let alive = this.update(cx, |this, cx| {
                    if let Some(list) = listed
                        && list != this.local_sessions
                    {
                        this.local_sessions = list;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Local server sessions that no pane shows.
    fn detached_local(&self) -> Vec<&berth_proto::v1::SessionInfo> {
        self.local_sessions
            .iter()
            .filter(|s| !self.panes.iter().any(|p| p.session == Some(s.id)))
            .collect()
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
    ///
    /// Detached sessions count as taken too: `new-session -A` would otherwise
    /// attach a new terminal to one of them and mirror it.
    fn free_slug(&self, alias: &str) -> String {
        let open = self.open_sessions();
        let taken: Vec<&str> = crate::sidebar::taken(alias, &open, &self.running)
            .into_iter()
            .collect();
        free_slug(&Self::slug(), &taken)
    }

    /// The session a host reattaches to when nothing more specific was asked
    /// for: the default one, unless a pane here already shows it.
    ///
    /// Only open panes count. A detached `berth/main` is exactly what opening
    /// the host should land back in.
    fn first_slug(&self, alias: &str) -> String {
        let taken: Vec<&str> = self
            .panes
            .iter()
            .filter(|p| p.alias() == alias)
            .map(|p| p.slug.as_str())
            .collect();
        free_slug(&Self::slug(), &taken)
    }

    /// Every pane, as the sidebar sees it.
    fn open_sessions(&self) -> Vec<crate::sidebar::OpenSession<'_>> {
        self.panes
            .iter()
            .map(|p| crate::sidebar::OpenSession {
                pane: p.id,
                host: p.host.as_ref().map(|h| h.alias.as_str()),
                slug: p.slug.as_str(),
            })
            .collect()
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
            None,
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

    /// Open a shell on this machine: a new session on the local server
    /// (D50), which outlives the app, or, when the server is off or will not
    /// start, a shell of its own that says on the tab that it will not.
    fn open_local(&mut self, place: Place, cx: &mut Context<Self>) {
        tracing::info!(shell = %berth_core::local::login_shell(), "opening a local shell");
        let pane_env = self.pane_env();
        let token = Some(pane_env.token.clone());
        let cell = self.local_server.clone();
        let direct = self.local_direct();
        cx.spawn(async move |this, cx| {
            let palette = crate::terminal_style::TerminalStyle::default().palette;
            let installed = cx.update(|cx| cx.text_system().all_font_names());
            let opened = runtime::spawn(session::open_local_tab(
                cell,
                direct,
                (80, 24),
                palette,
                pane_env,
                None,
            ))
            .await
            .map_err(|_| strings::RUNTIME_DROPPED.to_string())
            .and_then(|r| r.map_err(|e| e.to_string()));

            match opened {
                Ok(tab) => {
                    let _ = this.update(cx, |this, cx| {
                        let id = this.place_pane(
                            None,
                            strings::LOCAL_ALIAS.to_string(),
                            token,
                            tab.local.terminal,
                            tab.local.pty,
                            tab.local.events,
                            installed,
                            place,
                            cx,
                        );
                        if let Some(pane) = this.pane_mut(id) {
                            pane.session = tab.session;
                            pane.direct = tab.direct;
                        }
                        this.report_cold_start(strings::LOCAL_ALIAS);
                        this.refresh_local_sessions(cx);
                    });
                    if crate::bench::Bench::from_env() == Some(crate::bench::Bench::LocalCat) {
                        Self::run_local_benchmark(&this, cx).await;
                    }
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

    /// Show a local server session that no tab shows, in a tab of its own.
    fn attach_local(&mut self, session_id: u64, cx: &mut Context<Self>) {
        if let Some(pane) = self.panes.iter().find(|p| p.session == Some(session_id)) {
            let id = pane.id;
            self.focus_pane(id, cx);
            return;
        }
        let Some(Ok(server)) = self.local_server.get().cloned() else {
            return;
        };
        tracing::info!(session = session_id, "attaching a local session");
        cx.spawn(async move |this, cx| {
            let palette = crate::terminal_style::TerminalStyle::default().palette;
            let installed = cx.update(|cx| cx.text_system().all_font_names());
            let attached = runtime::spawn(async move {
                session::attach_session(&server, session_id, 80, 24, palette).await
            })
            .await
            .map_err(|_| strings::RUNTIME_DROPPED.to_string())
            .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = this.update(cx, |this, cx| match attached {
                Ok(local) => {
                    // Claude status for a session opened in an earlier run
                    // waits for the worker to report it (step 2, piece 6): the
                    // pane's token is not known here.
                    let id = this.place_pane(
                        None,
                        strings::LOCAL_ALIAS.to_string(),
                        None,
                        local.terminal,
                        local.pty,
                        local.events,
                        installed,
                        Place::Tab { show: true },
                        cx,
                    );
                    if let Some(pane) = this.pane_mut(id) {
                        pane.session = Some(session_id);
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "could not attach a local session");
                    this.warnings.push(error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Read the local server's session list now, rather than at the next
    /// interval: after opening, closing or ending a session.
    fn refresh_local_sessions(&self, cx: &mut Context<Self>) {
        let Some(Ok(server)) = self.local_server.get().cloned() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let listed = runtime::spawn(async move { server.list().await })
                .await
                .ok()
                .and_then(|r| r.ok());
            if let Some(list) = listed {
                let _ = this.update(cx, |this, cx| {
                    this.local_sessions = list;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// A new token, and the shim for the front of PATH, for a local pane.
    fn pane_env(&self) -> berth_core::claude::PaneEnv {
        berth_core::claude::PaneEnv::new(self.claude_base.as_deref())
    }

    /// Hear the agent signals a pane's terminal picks out.
    fn watch_signals(&self, id: u64, view: &Entity<TerminalView>, cx: &mut Context<Self>) {
        cx.subscribe(
            view,
            move |this, _, signals: &crate::terminal_view::AgentSignals, cx| {
                this.on_agent_signals(id, &signals.0, cx)
            },
        )
        .detach();
    }

    /// Believe the signals that carry this pane's token, and fold them in.
    fn on_agent_signals(&mut self, id: u64, payloads: &[String], cx: &mut Context<Self>) {
        let Some(claude) = self.pane_mut(id).and_then(|p| p.claude.as_mut()) else {
            return;
        };
        let before = claude.tracker.state;
        for payload in payloads {
            match berth_core::claude::parse(payload, &claude.token) {
                Some(signal) => claude.tracker.apply(&signal),
                // Stray output that looks like a signal, or another pane's.
                None => tracing::debug!(pane = id, "agent signal not believed"),
            }
        }
        if claude.tracker.state != before {
            tracing::info!(pane = id, state = ?claude.tracker.state, "claude state");
            cx.notify();
        }
    }

    /// What a pane's Claude is doing, when its hooks have said.
    fn claude_state(&self, id: u64) -> Option<berth_core::claude::State> {
        self.pane(id)?.claude.as_ref()?.tracker.state
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
        token: Option<berth_core::claude::PaneToken>,
        terminal: crate::terminal_view::SharedTerminal,
        pty: berth_core::pty::Pty,
        events: tokio::sync::mpsc::Receiver<berth_core::pty::PtyEvent>,
        installed: Vec<String>,
        place: Place,
        cx: &mut Context<Self>,
    ) -> u64 {
        let font_size = self.settings.font_size;
        let view = cx.new(|cx| {
            TerminalView::new(
                terminal,
                Some(pty),
                {
                    let mut style = crate::terminal_style::TerminalStyle::default();
                    style.resolve_family(&installed);
                    style.font_size = font_size;
                    style
                },
                cx,
            )
        });
        view.update(cx, |view, _| session::start_pump(view, events));

        self.next_pane_id += 1;
        let id = self.next_pane_id;
        self.watch_signals(id, &view, cx);
        self.panes.push(Pane {
            id,
            host,
            slug,
            view,
            claude: token.map(ClaudePane::new),
            restored: None,
            session: None,
            direct: None,
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
            // Restored local tabs are already something to type into.
            if self.tabs.is_empty() {
                self.open_local(Place::Tab { show: true }, cx);
            }
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
            self.connect(host, None, asked_for, cx);
        }
    }

    /// Bring a host up in a tab of its own, shown or not.
    ///
    /// `slug` names the tmux session to attach, or `None` for the default one,
    /// which is what warming and a click on a host have always reattached to.
    fn connect(&mut self, host: Host, slug: Option<String>, show: bool, cx: &mut Context<Self>) {
        let alias = host.alias.clone();
        tracing::info!(%alias, "connecting");
        let slug = slug.unwrap_or_else(|| self.first_slug(&alias));
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
            self.wake_active(cx);
            cx.notify();
            return;
        }
        let Some(host) = self.hosts.iter().find(|h| h.alias == alias).cloned() else {
            return;
        };
        if berth_core::known_hosts::is_known(&host.hostname, host.port) {
            self.statuses
                .insert(alias, HostStatus::Connection(ConnectionState::Connecting));
            self.connect(host, None, true, cx);
        } else {
            self.verify(host, cx);
        }
    }

    /// Act on a click on a folder row.
    ///
    /// A host with no connection is opened, exactly as a host row always was:
    /// verified if it was never trusted, retried if it failed, reattached
    /// otherwise. Only a live host has a listing to show, so only a live host
    /// folds and unfolds.
    fn click_folder(
        &mut self,
        folder: crate::sidebar::Folder,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        if let crate::sidebar::Folder::Host(alias) = &folder
            && !self.conns.contains_key(alias)
        {
            self.expanded.insert(folder.clone(), true);
            self.open(alias.clone(), cx);
            return;
        }
        self.expanded.insert(folder, !expanded);
        cx.notify();
    }

    /// Act on a click on a session row: show it if it is open, reattach it in a
    /// tab of its own if it is running detached.
    fn open_session(&mut self, alias: String, slug: String, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            return;
        }
        if let Some(pane) = self
            .panes
            .iter()
            .find(|p| p.host.as_ref().is_some_and(|h| h.alias == alias) && p.slug == slug)
            .map(|p| p.id)
        {
            self.focus_pane(pane, cx);
            return;
        }
        self.attach_new(alias, slug, cx);
    }

    /// A fresh terminal in a folder, in a tab of its own.
    fn new_terminal(&mut self, folder: crate::sidebar::Folder, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            return;
        }
        match folder {
            crate::sidebar::Folder::Local => self.open_local(Place::Tab { show: true }, cx),
            crate::sidebar::Folder::Host(alias) => {
                let slug = self.free_slug(&alias);
                self.attach_new(alias, slug, cx);
            }
        }
    }

    /// Attach a named session in a new tab, on the host's connection if it has
    /// one and through a normal connect if it does not.
    fn attach_new(&mut self, alias: String, slug: String, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.iter().find(|h| h.alias == alias).cloned() else {
            return;
        };
        let Some(connection) = self.conns.get(&alias).map(|c| c.connection.clone()) else {
            // Nothing to ride: the same path as opening the host, so trust is
            // still asked for before anything connects.
            if berth_core::known_hosts::is_known(&host.hostname, host.port) {
                self.statuses
                    .insert(alias, HostStatus::Connection(ConnectionState::Connecting));
                self.connect(host, Some(slug), true, cx);
            } else {
                self.verify(host, cx);
            }
            return;
        };
        tracing::info!(%alias, %slug, "attaching from the sidebar");
        let place = Place::Tab { show: true };
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
                        this.adopt(
                            Some(host),
                            slug,
                            connected,
                            Vec::new(),
                            installed,
                            place,
                            cx,
                        );
                    });
                }
                Err(error) => {
                    tracing::warn!(%alias, %error, "attach failed");
                    let _ = this.update(cx, |this, cx| {
                        this.warnings.push(error);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn activate(&mut self, tab: u64, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.iter().position(|t| t.id == tab) {
            self.active = index;
            self.wake_active(cx);
            cx.notify();
        }
    }

    /// Give a pane the keyboard, and show the tab holding it.
    fn focus_pane(&mut self, pane: u64, cx: &mut Context<Self>) {
        if let Some(index) = self.tab_of(pane) {
            self.active = index;
            self.wake_active(cx);
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
            Command::FontBigger => self.nudge_font(FONT_SIZE_STEP, cx),
            Command::FontSmaller => self.nudge_font(-FONT_SIZE_STEP, cx),
            Command::FontReset => self.set_font_size(DEFAULT_FONT_SIZE, cx),
            // There was no way to quit but closing the window. Quitting runs
            // the quit hooks, which is where local tabs are saved.
            Command::Quit => cx.quit(),
        }
    }

    fn nudge_font(&mut self, by: f32, cx: &mut Context<Self>) {
        self.set_font_size(self.settings.font_size + by, cx);
    }

    /// Resize the text everywhere and remember it.
    ///
    /// Every pane, not the focused one: a split showing two sizes of the same
    /// font is not a feature anybody asked for. Saved on each press rather than
    /// on quit, because the one time it matters is the launch after a crash.
    fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        if !self.settings.set_font_size(size) {
            return;
        }
        let size = self.settings.font_size;
        for pane in &self.panes {
            pane.view.update(cx, |view, cx| {
                view.set_font_size(size);
                cx.notify();
            });
        }
        self.settings.save();
        cx.notify();
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
        let local = pane.is_local();
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
            // With no connection nothing keeps the listing true, and a stale
            // row would reattach to a session killed from somewhere else.
            self.running.remove(&alias);
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
        // A closed local shell has ended, so it must not come back at the next
        // launch. Saved now rather than at the next minute or at quit, which a
        // crash would skip.
        if local {
            self.save_local(cx);
        }
        // Closing can put a restored tab on screen.
        self.wake_active(cx);
        cx.notify();
    }

    /// Ask before ending a session. Never kill directly from a click.
    fn ask_kill(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(session) = self.pane(id).and_then(|p| p.session) {
            self.ask_kill_local(session, Some(id), cx);
            return;
        }
        let Some(pane) = self.pane(id).filter(|p| !p.is_local()) else {
            return;
        };
        self.modal = Some(Modal::Kill {
            pane: Some(id),
            alias: pane.alias().to_string(),
            slug: pane.slug.clone(),
            session: pane.tmux_name(),
        });
        self.modal_focused = false;
        cx.notify();
    }

    /// Ask before ending a local server session (D50), shown or not.
    fn ask_kill_local(&mut self, session: u64, pane: Option<u64>, cx: &mut Context<Self>) {
        self.modal = Some(Modal::Kill {
            pane,
            alias: strings::LOCAL_ALIAS.to_string(),
            slug: session.to_string(),
            session: strings::local_detached_name(&render::local_label(), session),
        });
        self.modal_focused = false;
        cx.notify();
    }

    /// Ask before ending a session that has no tab.
    fn ask_kill_detached(&mut self, alias: String, slug: String, cx: &mut Context<Self>) {
        let session = berth_core::session::Session::new(slug.clone(), 80, 24).tmux_name();
        self.modal = Some(Modal::Kill {
            pane: None,
            alias,
            slug,
            session,
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
            let from = from.clone();
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

        // Moved here as well, or the tree shows the old name as a detached
        // session until the next poll.
        if let Some(listed) = self.running.get_mut(&alias)
            && let Some(agent) = listed.remove(&from)
        {
            listed.insert(to.clone(), agent);
        }
        if let Some(pane) = self.pane_mut(id) {
            pane.slug = to;
        }
        cx.notify();
    }

    /// End a tmux session and everything running in it, and close its pane if
    /// it has one.
    fn kill(&mut self, alias: String, slug: String, pane: Option<u64>, cx: &mut Context<Self>) {
        if alias == strings::LOCAL_ALIAS {
            self.kill_local(&slug, pane, cx);
            return;
        }
        let session = berth_core::session::Session::new(slug.clone(), 80, 24);
        let name = session.tmux_name();
        let command = session.kill_command();
        tracing::warn!(%alias, session = %name, "killing session");
        if let Some(listed) = self.running.get_mut(&alias) {
            listed.remove(&slug);
        }
        let Some(connection) = self.conns.get(&alias).map(|c| c.connection.clone()) else {
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

        if let Some(pane) = pane {
            self.close(pane, cx);
        }
        cx.notify();
    }

    /// End a local server session: its shell and everything running in it.
    fn kill_local(&mut self, session: &str, pane: Option<u64>, cx: &mut Context<Self>) {
        let (Ok(session), Some(Ok(server))) =
            (session.parse::<u64>(), self.local_server.get().cloned())
        else {
            return;
        };
        tracing::warn!(session, "ending a local session");
        self.local_sessions.retain(|s| s.id != session);
        cx.spawn(async move |this, cx| {
            let _ = runtime::spawn(async move { server.kill(session).await }).await;
            let _ = this.update(cx, |this, cx| this.refresh_local_sessions(cx));
        })
        .detach();
        if let Some(pane) = pane {
            self.close(pane, cx);
        }
        cx.notify();
    }

    /// What a host is called on screen: the name it was given, or its alias.
    fn host_label(&self, alias: &str) -> String {
        self.settings.host_name(alias).unwrap_or(alias).to_string()
    }

    fn ask_rename_host(&mut self, alias: String, cx: &mut Context<Self>) {
        let current = self.settings.host_name(&alias).unwrap_or("").to_string();
        self.modal = Some(Modal::RenameHost {
            alias,
            field: TextField::new(current),
        });
        self.modal_focused = false;
        cx.notify();
    }

    /// Name a host, or clear its name with an empty one.
    fn rename_host(&mut self, alias: &str, name: &str, cx: &mut Context<Self>) {
        if self.settings.set_host_name(alias, name) {
            self.settings.save();
            cx.notify();
        }
    }

    // -- the right-click menu ----------------------------------------------

    fn open_menu(
        &mut self,
        target: crate::menu::Target,
        at: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.modal.is_some() {
            return;
        }
        self.menu = Some((target, at));
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    /// What the open menu offers, given what is true now.
    fn menu_actions(&self) -> Vec<crate::menu::Action> {
        use crate::menu::{Facts, Target};
        let Some((target, _)) = &self.menu else {
            return Vec::new();
        };
        let facts = match target {
            Target::Pane(id) => Facts {
                local: self
                    .pane(*id)
                    .is_some_and(|p| p.is_local() && p.session.is_none()),
                connected: false,
            },
            Target::Host(alias) => Facts {
                local: false,
                connected: self.conns.contains_key(alias),
            },
            Target::Detached { .. } | Target::LocalDetached { .. } | Target::Local => {
                Facts::default()
            }
        };
        crate::menu::actions(target, facts)
    }

    fn run_menu(&mut self, action: crate::menu::Action, cx: &mut Context<Self>) {
        use crate::menu::{Action, Target};
        let Some((target, _)) = self.menu.take() else {
            return;
        };
        match (action, target) {
            (Action::Rename, Target::Pane(id)) => self.ask_rename(id, cx),
            (Action::Detach | Action::Close, Target::Pane(id)) => self.close(id, cx),
            (Action::EndSession, Target::Pane(id)) => self.ask_kill(id, cx),
            (Action::EndSession, Target::Detached { alias, slug }) => {
                self.ask_kill_detached(alias, slug, cx)
            }
            (Action::Open, Target::Detached { alias, slug }) => self.open_session(alias, slug, cx),
            (Action::Open, Target::LocalDetached { session }) => self.attach_local(session, cx),
            (Action::EndSession, Target::LocalDetached { session }) => {
                self.ask_kill_local(session, None, cx)
            }
            (Action::NewTerminal, Target::Local) => {
                self.new_terminal(crate::sidebar::Folder::Local, cx)
            }
            (Action::NewTerminal, Target::Host(alias)) => {
                self.new_terminal(crate::sidebar::Folder::Host(alias), cx)
            }
            (Action::Connect, Target::Host(alias)) => self.open(alias, cx),
            (Action::RenameHost, Target::Host(alias)) => self.ask_rename_host(alias, cx),
            // The menu only lists what `actions` allows, so anything else is a
            // menu built from one target and run against another.
            (action, target) => tracing::debug!(?action, ?target, "menu action does not apply"),
        }
        cx.notify();
    }
}

/// Put the Claude plugin and shim in Berth's data directory (D48).
///
/// Best effort: without them a pane still opens, it just cannot report
/// Claude's state. Only files that changed are rewritten, so this is cheap on
/// every launch.
fn install_claude_support() -> Option<std::path::PathBuf> {
    let base = berth_core::claude::base_dir()?;
    match berth_core::claude::install(&base) {
        Ok(()) => Some(base),
        Err(error) => {
            tracing::warn!(%error, "could not put the Claude plugin in place, no Claude status");
            None
        }
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

/// Whether a tab carries a state dot.
///
/// Only when something is wrong. Connected is the normal case and the sidebar
/// folder already says it, so a dot on every tab was colour that meant nothing.
/// A local shell has no connection at all, and used to show one as
/// disconnected.
fn shows_state(local: bool, state: ConnectionState) -> bool {
    !local && state != ConnectionState::Connected
}

/// Whether a host's sessions, or the agents in them, have moved.
///
/// Nothing recorded and nothing found is not a change. Treating it as one made
/// every host log its arrival at startup, for a picture that had always been
/// empty.
fn presence_changed(
    existing: Option<&std::collections::BTreeMap<String, bool>>,
    found: &std::collections::BTreeMap<String, bool>,
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

    fn sessions(pairs: &[(&str, bool)]) -> std::collections::BTreeMap<String, bool> {
        pairs.iter().map(|(n, a)| (n.to_string(), *a)).collect()
    }

    /// The startup case: nothing known and nothing running is not news.
    #[test]
    fn nothing_running_and_nothing_known_is_not_a_change() {
        assert!(!presence_changed(None, &sessions(&[])));
    }

    #[test]
    fn an_agent_appearing_or_leaving_is_a_change() {
        assert!(presence_changed(
            Some(&sessions(&[("main", false)])),
            &sessions(&[("main", true)])
        ));
        assert!(presence_changed(
            Some(&sessions(&[("main", true)])),
            &sessions(&[("main", false)])
        ));
    }

    /// The tree lists detached sessions, so one appearing or going is news
    /// even with no agent anywhere.
    #[test]
    fn a_session_appearing_or_leaving_is_a_change() {
        assert!(presence_changed(None, &sessions(&[("main", false)])));
        assert!(presence_changed(
            Some(&sessions(&[("main", false)])),
            &sessions(&[])
        ));
        assert!(presence_changed(
            Some(&sessions(&[("main", false)])),
            &sessions(&[("deploy", false)])
        ));
    }

    #[test]
    fn a_healthy_tab_carries_no_state_dot() {
        assert!(!shows_state(false, ConnectionState::Connected));
        for state in [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::Degraded,
            ConnectionState::Reconnecting,
        ] {
            assert!(shows_state(false, state), "{state:?}");
        }
    }

    /// A local shell has no connection, so there is no state to report.
    #[test]
    fn a_local_tab_never_carries_a_state_dot() {
        assert!(!shows_state(true, ConnectionState::Disconnected));
        assert!(!shows_state(true, ConnectionState::Connected));
    }

    /// The same picture twice must not repaint, or this is a wakeup every five
    /// seconds forever.
    #[test]
    fn the_same_listing_is_not_a_change() {
        let running = sessions(&[("main", true), ("deploy", false)]);
        assert!(!presence_changed(Some(&running), &running.clone()));
    }
}
