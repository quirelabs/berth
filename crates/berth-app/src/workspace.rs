//! The root view: sidebar, tabs, main area, status bar.

mod machine;
mod render;
mod saving;
mod server;
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
use crate::theme::{Theme, hsla};
use crate::{runtime, session};
use gpui::svg;

/// How long output may go undrawn before the window is taken to be hidden,
/// and how often that is checked.
const SEEN_EVERY: Duration = Duration::from_secs(1);

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

/// A sidebar row being dragged: what it names, and how it looks under the
/// pointer.
#[derive(Clone)]
struct RowDrag {
    target: RowTarget,
    name: gpui::SharedString,
    theme: Theme,
}

impl Render for RowDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(2.0))
            .rounded(px(3.0))
            .bg(hsla(self.theme.surface))
            .border_1()
            .border_color(hsla(self.theme.border))
            .text_xs()
            .text_color(hsla(self.theme.text))
            .child(self.name.clone())
    }
}

/// A split's divider being dragged: which tab, and which split in it.
#[derive(Clone)]
struct DividerDrag {
    tab: u64,
    path: crate::panes::Path,
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

use crate::tabs::Tab;

/// A session as a sidebar row names it: shown in a pane, or running with no
/// pane, on a host or on the local server.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowTarget {
    Pane(u64),
    Remote { alias: String, slug: String },
    Local(u64),
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
    /// Offering Berth's server to a host: exactly what would be installed
    /// and where, before anything is (D55).
    Server {
        alias: String,
        plan: Box<berth_core::remote::Plan>,
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
    /// Hosts running a server build other than the one this app carries: the
    /// build there, and the one on offer (D55).
    server_updates: std::collections::BTreeMap<String, (String, String)>,
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
    /// Whether the window is taken to be drawn, as the local server was last
    /// told (D50).
    window_seen: bool,
    /// The session whose finished or failed status the focused pane was
    /// showing at the last look: still so at the next, the user has seen it
    /// (D53).
    status_shown: Looked<StatusKey>,
    /// Panes whose Claude finished its turn, as its hooks said, and whose
    /// user has since looked: their `done` is no longer shown (D53).
    hook_done_seen: std::collections::HashSet<u64>,
    /// What runs in each remote session, by alias and slug: the foreground
    /// process's name from the same scan as `running`.
    programs: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    /// The divider being dragged, whose pill stays up while it is.
    dragging_divider: Option<(u64, crate::panes::Path)>,
    /// The glyphs' clock (D53): what every blink keeps time by, what each
    /// pane or session shows and since when, the panes needing the user
    /// that have been focused since (their heartbeat is steady), whether
    /// motion is off, and the one timer every glyph shares. The timer is
    /// only set while a tab's glyph on screen moves.
    glyph_epoch: std::time::Instant,
    glyph_since: std::collections::HashMap<StatusKey, (crate::glyph::State, std::time::Instant)>,
    glyph_steady: std::collections::HashSet<StatusKey>,
    glyph_still: bool,
    glyph_timer: Option<gpui::Task<()>>,
    /// The accents `berth.toml` gives programs, by name in lower case.
    accents: std::collections::BTreeMap<String, [u8; 3]>,
    /// The terminal's font, which the chrome is set in too (D53): found
    /// once, as a terminal finds it.
    chrome_font: Option<gpui::SharedString>,
    /// Where the local server's Claude reports and session lists go, once
    /// it is up.
    claude_reports: session::ClaudeReports,
    session_lists: session::SessionLists,
}

/// Seen means looked at twice running: whatever a look is showing is seen
/// when the next look, a second later, is still showing it. Used for the
/// finished and failed status a focused pane shows, which goes once its user
/// has had the pane in front of them for about a second (D53).
#[derive(Debug)]
struct Looked<K> {
    last: Option<K>,
}

impl<K> Default for Looked<K> {
    fn default() -> Self {
        Self { last: None }
    }
}

impl<K: PartialEq + Copy> Looked<K> {
    /// One look, showing `showing`. Returns what has now been seen.
    fn look(&mut self, showing: Option<K>) -> Option<K> {
        let seen = showing.filter(|key| self.last == Some(*key));
        // Seen once is enough: it starts over if it shows again.
        self.last = if seen.is_some() { None } else { showing };
        seen
    }
}

/// A tab's title, in order: what the program says it is doing (the message
/// of its status record, OSC 7501), the title it gave the terminal, and
/// failing both the program's name with the directory it is in, shortened.
/// `None` when none of them is known, and the tab keeps the name it had.
fn tab_title(
    message: Option<&str>,
    terminal_title: Option<&str>,
    program: Option<&str>,
    directory: Option<&str>,
    home: Option<&str>,
) -> Option<String> {
    fn said(text: Option<&str>) -> Option<&str> {
        text.map(str::trim).filter(|t| !t.is_empty())
    }
    if let Some(text) = said(message).or(said(terminal_title)) {
        return Some(text.to_string());
    }
    let name = crate::programs::display_name(said(program)?);
    if name.is_empty() {
        return None;
    }
    Some(
        match said(directory).map(|dir| short_directory(dir, home)) {
            Some(dir) => format!("{name} {dir}"),
            None => name,
        },
    )
}

/// The user's home directory, for writing it as `~`.
fn dirs_home() -> Option<String> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .filter(|home| !home.is_empty())
}

/// A directory as a tab has room for: `~` for home, else its last part.
fn short_directory(directory: &str, home: Option<&str>) -> String {
    let directory = directory.trim_end_matches('/');
    if home.is_some_and(|home| home.trim_end_matches('/') == directory) {
        return "~".to_string();
    }
    match directory.rsplit('/').next() {
        Some(last) if !last.is_empty() => last.to_string(),
        _ => "/".to_string(),
    }
}

/// Whose status: a pane's, or a local session's that no pane shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum StatusKey {
    Pane(u64),
    Session(u64),
}

/// What a status shows and says.
struct StatusLook {
    state: crate::glyph::State,
    /// The program it is from, by the name it reports or runs under: its
    /// accent, and how it is named.
    program: Option<String>,
    /// The tooltip's words for it: the program's own, as a report.
    text: String,
    /// The program's own message, if it gave one.
    msg: Option<String>,
}

impl StatusLook {
    fn of_status(status: &berth_proto::v1::Status) -> Self {
        Self {
            state: crate::glyph::State::of_status(status.state),
            program: status.app.clone(),
            text: render::status_text(status),
            msg: status.msg.clone(),
        }
    }

    fn of_claude(state: berth_core::claude::State) -> Self {
        Self {
            state: crate::glyph::State::of_claude(state),
            program: Some("claude".to_string()),
            text: render::claude_text(state).into(),
            msg: None,
        }
    }
}

/// The accents `berth.toml` gives programs (D53). Read once, at start; a
/// file that does not parse is said in the log and changes nothing.
fn load_accents() -> std::collections::BTreeMap<String, [u8; 3]> {
    use berth_core::config::BerthConfig;
    let Some(path) = BerthConfig::path() else {
        return Default::default();
    };
    match BerthConfig::load_or_default(&path) {
        Ok(config) => config
            .accents
            .keys()
            .filter_map(|name| Some((name.to_ascii_lowercase(), config.accent(name)?)))
            .collect(),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "berth.toml was not read");
            Default::default()
        }
    }
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
            theme: Theme::of_terminal(&crate::terminal_style::TerminalStyle::default().palette),
            empty: Empty::FirstRun,
            settings: berth_core::settings::Settings::load(),
            server_updates: std::collections::BTreeMap::new(),
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
            window_seen: true,
            status_shown: Looked::default(),
            hook_done_seen: std::collections::HashSet::new(),
            programs: std::collections::BTreeMap::new(),
            dragging_divider: None,
            glyph_epoch: std::time::Instant::now(),
            glyph_since: std::collections::HashMap::new(),
            glyph_steady: std::collections::HashSet::new(),
            glyph_still: false,
            glyph_timer: None,
            accents: load_accents(),
            chrome_font: None,
            claude_reports: {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                Self::watch_claude_reports(rx, cx);
                tx
            },
            session_lists: {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                Self::watch_session_lists(rx, cx);
                tx
            },
            statuses: std::collections::BTreeMap::new(),
            started: std::time::Instant::now(),
            cold_start_reported: false,
        };

        this.restore_local(cx);
        this.warm(cx);
        this.watch_saves(cx);
        this.watch_window_seen(cx);
        this
    }

    /// Tell the local server when the window is not being drawn, so its
    /// workers send nothing meanwhile (D50).
    ///
    /// gpui says whether the window is focused, not whether it is visible or
    /// minimized, and focus is the wrong question: a window in the background
    /// still shows its output. So it is read from what is drawn: output
    /// arriving with no frame painted for a second means macOS is not drawing
    /// the window. The next frame painted, or the window becoming active,
    /// means it is again. A wrong guess undoes itself at the next paint, key
    /// or click.
    fn watch_window_seen(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SEEN_EVERY).await;
                let changed = this.update(cx, |this, cx| this.check_window_seen(cx));
                if changed.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    fn check_window_seen(&mut self, cx: &mut Context<Self>) {
        self.check_status_seen(cx);
        let Some(Ok(server)) = self.local_server.get().cloned() else {
            return;
        };
        let now = std::time::Instant::now();
        let recent = |at: Option<std::time::Instant>| at.is_some_and(|at| now - at < SEEN_EVERY);
        let local = || self.panes.iter().filter(|p| p.session.is_some());
        let painted = local().any(|p| recent(p.view.read(cx).last_frame_at));
        let busy = local().any(|p| recent(p.view.read(cx).intake.last_output()));
        let seen = if self.window_seen {
            painted || !busy
        } else {
            painted
        };
        if seen != self.window_seen {
            self.set_window_seen(seen, server.clone(), cx);
        }
        // A pane saying its input was not delivered is drawn again each
        // look, so the notice comes and goes on time.
        if self
            .panes
            .iter()
            .any(|p| p.view.read(cx).intake.input_dropped_recently())
        {
            cx.notify();
        }
    }

    /// A finished or failed status stays on a pane's icons until the user
    /// has seen it: the pane focused, in a window that is drawn, across two
    /// looks, so for a second or more (D53). For a status a program reported
    /// (OSC 7501) the worker is then told and drops the record, for every
    /// app and the session list alike; for a turn Claude's hooks said is
    /// finished, the pane is noted as looked at.
    fn check_status_seen(&mut self, cx: &mut Context<Self>) {
        use crate::glyph::State;
        use berth_proto::v1::StatusState;
        let showing = self
            .focused()
            .filter(|_| self.window_seen)
            .and_then(|pane| {
                let reported = pane.session.filter(|session| {
                    self.session_status(*session)
                        .is_some_and(|s| matches!(s.state, StatusState::Done | StatusState::Error))
                });
                match reported {
                    Some(session) => Some(StatusKey::Session(session)),
                    None => (self.pane_status(pane.id).is_none()
                        && self.pane_look(pane.id).map(|look| look.state) == Some(State::Done))
                    .then_some(StatusKey::Pane(pane.id)),
                }
            });
        match self.status_shown.look(showing) {
            Some(StatusKey::Session(session)) => {
                let Some(Ok(server)) = self.local_server.get().cloned() else {
                    return;
                };
                cx.background_executor()
                    .spawn(async move {
                        let _ =
                            runtime::spawn(async move { server.status_seen(session).await }).await;
                    })
                    .detach();
            }
            Some(StatusKey::Pane(pane)) => {
                self.hook_done_seen.insert(pane);
                cx.notify();
            }
            None => {}
        }
    }

    fn set_window_seen(
        &mut self,
        seen: bool,
        server: berth_core::server::local::LocalServer,
        cx: &mut Context<Self>,
    ) {
        self.window_seen = seen;
        tracing::info!(seen, "the window is drawn: told the local server");
        cx.background_executor()
            .spawn(async move {
                let _ = runtime::spawn(async move { server.set_visible(seen).await }).await;
            })
            .detach();
    }

    /// The window became active: it is certainly drawn.
    pub fn window_activated(&mut self, cx: &mut Context<Self>) {
        if !self.window_seen
            && let Some(Ok(server)) = self.local_server.get().cloned()
        {
            self.set_window_seen(true, server, cx);
        }
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

    /// Fold what workers report about Claude into the panes that show their
    /// sessions, and the session list (D50). The worker checked the token.
    fn watch_claude_reports(
        mut rx: tokio::sync::mpsc::UnboundedReceiver<(u64, berth_proto::v1::Claude)>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some((session, claude)) = rx.recv().await {
                let alive = this.update(cx, |this, cx| {
                    for pane in this.panes.iter_mut().filter(|p| p.session == Some(session)) {
                        let token = pane
                            .claude
                            .as_ref()
                            .map(|c| c.token.clone())
                            .unwrap_or_default();
                        let mut known = ClaudePane::new(token);
                        known.tracker = berth_core::claude::from_wire(&claude);
                        pane.claude = Some(known);
                    }
                    if let Some(info) = this.local_sessions.iter_mut().find(|s| s.id == session) {
                        info.claude = Some(claude.clone());
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Hand the local server's Claude reports and session lists to this
    /// workspace. Once per server; harmless to repeat.
    fn listen_for_claude(&self) {
        if let Some(Ok(server)) = self.local_server.get() {
            let tx = self.claude_reports.clone();
            server.on_claude(move |session, claude| {
                let _ = tx.send((session, claude));
            });
            let lists = self.session_lists.clone();
            server.on_sessions(move |list| {
                let _ = lists.send(list);
            });
        }
    }

    /// Keep the session list as the supervisor pushes it (D50): no timer.
    fn watch_session_lists(
        mut rx: tokio::sync::mpsc::UnboundedReceiver<Vec<berth_proto::v1::SessionInfo>>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some(list) = rx.recv().await {
                let alive = this.update(cx, |this, cx| {
                    if list != this.local_sessions {
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

    /// Local server sessions that no pane shows, with the names the tree
    /// gives them.
    fn detached_local(&self) -> Vec<(u64, String)> {
        let sessions: Vec<crate::sidebar::LocalSession> = self
            .local_sessions
            .iter()
            .filter(|s| !self.panes.iter().any(|p| p.session == Some(s.id)))
            .map(|s| crate::sidebar::LocalSession {
                id: s.id,
                program: s.program.as_deref(),
                cwd: s.cwd.as_deref(),
            })
            .collect();
        let home = std::env::home_dir().map(|h| h.to_string_lossy().to_string());
        crate::sidebar::local_session_names(&sessions, &render::local_label(), home.as_deref())
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
        crate::tabs::tab_of(&self.tabs, pane)
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

        let place_shown = match &place {
            Place::Tab { show } => Some(*show),
            Place::Beside { .. } => None,
        };
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
            self.watch_agents(alias.clone(), cx);
            // Only a host the user asked for is asked anything, and a
            // benchmark run is never interrupted by a question.
            let asking = matches!(place_shown, Some(true));
            if alias != strings::LOCAL_ALIAS && crate::bench::Bench::from_env().is_none() {
                self.check_server(alias, asking, false, cx);
            }
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
                        this.listen_for_claude();
                        this.report_cold_start(strings::LOCAL_ALIAS);
                        this.refresh_local_sessions(cx);
                    });
                    if crate::bench::Bench::from_env() == Some(crate::bench::Bench::LocalCat) {
                        Self::run_local_benchmark(&this, cx).await;
                    }
                    Self::run_resize_benchmark(&this, cx).await;
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

    /// Show a local server session that no tab shows, where `place` says.
    fn attach_local(&mut self, session_id: u64, place: Place, cx: &mut Context<Self>) {
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
                        place,
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

    /// What the programs in a local session report of themselves (OSC 7501,
    /// D53): the record the session is shown by. For display only.
    fn session_status(&self, session: u64) -> Option<&berth_proto::v1::Status> {
        self.local_sessions
            .iter()
            .find(|info| info.id == session)?
            .status
            .as_ref()
    }

    /// The same for the session a pane shows.
    fn pane_status(&self, id: u64) -> Option<&berth_proto::v1::Status> {
        self.session_status(self.pane(id)?.session?)
    }

    /// What a pane's status shows: what its session's programs report (OSC
    /// 7501, D53) first, Claude's hooks for a Claude too old to report that
    /// way, and nothing if neither has said anything. A report that names
    /// no program is taken to be from what runs in the pane.
    fn pane_look(&self, id: u64) -> Option<StatusLook> {
        if let Some(status) = self.pane_status(id) {
            let mut look = StatusLook::of_status(status);
            if look.program.is_none() {
                look.program = self
                    .pane(id)
                    .and_then(|pane| self.pane_program(pane))
                    .map(str::to_string);
            }
            return Some(look);
        }
        let mut look = self.claude_state(id).map(StatusLook::of_claude)?;
        // A finished turn the user has looked at since is no longer news.
        if look.state == crate::glyph::State::Done && self.hook_done_seen.contains(&id) {
            look.state = crate::glyph::State::Idle;
        }
        Some(look)
    }

    /// What runs in a pane's session, by its process name: the local
    /// server's word for a session of its own, the host's scan for a remote
    /// one. Unknown for a shell Berth runs directly.
    fn pane_program(&self, pane: &Pane) -> Option<&str> {
        match (&pane.host, pane.session) {
            (None, Some(session)) => self.session_program(session),
            (Some(host), _) => self.remote_program(&host.alias, &pane.slug),
            (None, None) => None,
        }
    }

    fn session_program(&self, session: u64) -> Option<&str> {
        self.local_sessions
            .iter()
            .find(|info| info.id == session)?
            .program
            .as_deref()
    }

    fn remote_program(&self, alias: &str, slug: &str) -> Option<&str> {
        self.programs.get(alias)?.get(slug).map(String::as_str)
    }

    /// A pane's title from what its program says (D53), if anything is
    /// known: see [`tab_title`]. The directory is only known for a session
    /// on the local server.
    fn pane_title(&self, pane: &Pane) -> Option<String> {
        let info = pane
            .session
            .and_then(|session| self.local_sessions.iter().find(|info| info.id == session));
        let home = dirs_home();
        tab_title(
            self.pane_status(pane.id).and_then(|s| s.msg.as_deref()),
            info.and_then(|info| info.title.as_deref()),
            self.pane_program(pane),
            info.and_then(|info| info.cwd.as_deref()),
            home.as_deref(),
        )
    }

    /// The same for a local session no pane shows.
    fn session_look(&self, session: u64) -> Option<StatusLook> {
        let mut look = StatusLook::of_status(self.session_status(session)?);
        if look.program.is_none() {
            look.program = self.session_program(session).map(str::to_string);
        }
        Some(look)
    }

    /// How long a status has been showing as it is.
    fn status_since(&self, key: StatusKey) -> std::time::Duration {
        self.glyph_since
            .get(&key)
            .map(|(_, since)| since.elapsed())
            .unwrap_or_default()
    }

    /// The colours a status is drawn in, for its program.
    fn glyph_colours(&self, program: Option<&str>) -> crate::glyph::Colours {
        let theme = self.theme;
        let accent = program.and_then(|program| crate::programs::accent(program, &self.accents));
        crate::glyph::Colours {
            accent: theme.accent_for(accent),
            gold: theme.gold,
            rose: theme.rose,
            ink: theme.ink,
            muted: theme.text_muted,
        }
    }

    /// A status's glyph now: on a tab it moves, in the sidebar (`still`)
    /// it never does (D53).
    fn glyph_of(&self, key: StatusKey, look: &StatusLook, still: bool) -> crate::glyph::Glyph {
        crate::glyph::glyph(
            look.state,
            self.glyph_colours(look.program.as_deref()),
            crate::glyph::Moment {
                clock: self.glyph_epoch.elapsed(),
                since: self.status_since(key),
                steady: self.glyph_steady.contains(&key),
                still: still || self.glyph_still,
            },
        )
    }

    /// A pane's glyph on a tab, idle when nothing has been said.
    fn pane_glyph(&self, id: u64) -> crate::glyph::Glyph {
        let key = StatusKey::Pane(id);
        match self.pane_look(id) {
            Some(look) => self.glyph_of(key, &look, false),
            None => {
                let program = self.pane(id).and_then(|pane| self.pane_program(pane));
                crate::glyph::glyph(
                    crate::glyph::State::Idle,
                    self.glyph_colours(program),
                    crate::glyph::Moment {
                        clock: std::time::Duration::ZERO,
                        since: std::time::Duration::ZERO,
                        steady: true,
                        still: true,
                    },
                )
            }
        }
    }

    /// Before drawing: note what each status on screen shows and since when,
    /// make a heartbeat steady once its pane is focused in a drawn window,
    /// and set the one timer for the next change any tab glyph needs.
    /// Sidebar glyphs never move. Nothing moving, motion reduced or the
    /// window not drawn: no timer, and no wakeups.
    fn tend_glyphs(&mut self, cx: &mut Context<Self>) {
        use crate::glyph::State;
        self.glyph_still = cx.reduce_motion();
        let finished: std::collections::HashSet<u64> = self
            .panes
            .iter()
            .filter(|pane| self.claude_state(pane.id).map(State::of_claude) == Some(State::Done))
            .map(|pane| pane.id)
            .collect();
        self.hook_done_seen.retain(|pane| finished.contains(pane));
        let shown: Vec<(StatusKey, State)> = self
            .panes
            .iter()
            .filter_map(|pane| Some((StatusKey::Pane(pane.id), self.pane_look(pane.id)?.state)))
            .chain(self.local_sessions.iter().filter_map(|info| {
                Some((
                    StatusKey::Session(info.id),
                    self.session_look(info.id)?.state,
                ))
            }))
            .collect();
        let now = std::time::Instant::now();
        self.glyph_since
            .retain(|key, _| shown.iter().any(|(shown, _)| shown == key));
        for (key, state) in &shown {
            match self.glyph_since.get(key) {
                Some((was, _)) if was == state => {}
                _ => {
                    self.glyph_since.insert(*key, (*state, now));
                }
            }
        }
        self.glyph_steady
            .retain(|key| shown.contains(&(*key, State::NeedsYou)));
        if self.window_seen
            && let Some(focused) = self.focused().map(|pane| StatusKey::Pane(pane.id))
            && shown.contains(&(focused, State::NeedsYou))
        {
            self.glyph_steady.insert(focused);
        }
        // Only the panes of tabs move: the tab strip shows each tab's
        // focused pane's glyph, or its pane map.
        let moving: Vec<(State, std::time::Duration, bool)> = shown
            .iter()
            .filter(|(key, _)| matches!(key, StatusKey::Pane(_)))
            .map(|(key, state)| {
                (
                    *state,
                    self.status_since(*key),
                    self.glyph_steady.contains(key),
                )
            })
            .collect();
        let next = crate::glyph::next_wake(
            &moving,
            self.glyph_epoch.elapsed(),
            self.glyph_still,
            self.window_seen,
        );
        self.glyph_timer = next.map(|wait| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                let _ = this.update(cx, |_, cx| cx.notify());
            })
        });
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

        // The first session to land is shown whether or not it was asked for
        // by name. Sitting on the first-run screen with a live session behind
        // it would be wrong, and whichever host answered first is the one the
        // user can type into soonest. A split takes the keyboard, as in every
        // other splitting editor: you split in order to type in the new one.
        // If the pane being split went away while this was opening, the
        // session is real, so it gets a tab of its own rather than being
        // dropped on the floor.
        let beside = match place {
            Place::Beside { pane, axis } => {
                crate::tabs::put_beside(&mut self.tabs, &mut self.active, id, pane, axis)
            }
            Place::Tab { .. } => false,
        };
        if !beside {
            let show = match place {
                Place::Tab { show } => show,
                Place::Beside { .. } => true,
            };
            self.next_tab_id += 1;
            crate::tabs::open_tab(&mut self.tabs, &mut self.active, self.next_tab_id, id, show);
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
        self.attach_new(alias, slug, Place::Tab { show: true }, cx);
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
                self.attach_new(alias, slug, Place::Tab { show: true }, cx);
            }
        }
    }

    /// Attach a named session where `place` says, on the host's connection if
    /// it has one and through a normal connect, in a tab of its own, if it
    /// does not.
    fn attach_new(&mut self, alias: String, slug: String, place: Place, cx: &mut Context<Self>) {
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

    /// A new tab with a fresh session on the focused pane's host, or on this
    /// machine when nothing is focused or it is local.
    fn new_tab(&mut self, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            return;
        }
        let folder = match self.focused().and_then(|p| p.host.as_ref()) {
            Some(host) => crate::sidebar::Folder::Host(host.alias.clone()),
            None => crate::sidebar::Folder::Local,
        };
        self.new_terminal(folder, cx);
    }

    /// Go to tab N (⌘N).
    fn select_tab(&mut self, n: usize, cx: &mut Context<Self>) {
        if let Some(index) = crate::tabs::numbered(&self.tabs, n) {
            self.active = index;
            self.wake_active(cx);
            cx.notify();
        }
    }

    /// The pane a row's session is shown in, if any.
    fn shown_in(&self, target: &RowTarget) -> Option<u64> {
        match target {
            RowTarget::Pane(id) => self.pane(*id).map(|p| p.id),
            RowTarget::Remote { alias, slug } => self
                .panes
                .iter()
                .find(|p| p.host.as_ref().is_some_and(|h| &h.alias == alias) && &p.slug == slug)
                .map(|p| p.id),
            RowTarget::Local(session) => self
                .panes
                .iter()
                .find(|p| p.session == Some(*session))
                .map(|p| p.id),
        }
    }

    /// A click on a row: its tab and pane, or a new tab if no pane shows it.
    fn click_row(&mut self, target: RowTarget, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            return;
        }
        if let Some(pane) = self.shown_in(&target) {
            self.focus_pane(pane, cx);
            return;
        }
        let place = Place::Tab { show: true };
        match target {
            RowTarget::Pane(_) => {}
            RowTarget::Remote { alias, slug } => self.attach_new(alias, slug, place, cx),
            RowTarget::Local(session) => self.attach_local(session, place, cx),
        }
    }

    /// ⌥-click on a row, or a row dropped on a tab or a pane: split its
    /// session in beside `beside` (the active tab's focused pane when none is
    /// given), moving it if another pane shows it. A session is only ever in
    /// one pane, since a terminal has one size.
    fn split_in(&mut self, target: RowTarget, beside: Option<u64>, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            return;
        }
        let Some(beside) = beside.or_else(|| self.tab().map(|t| t.focus)) else {
            return self.click_row(target, cx);
        };
        let axis = Axis::Horizontal;
        match self.shown_in(&target) {
            Some(pane) if pane == beside => self.focus_pane(pane, cx),
            Some(pane) => {
                if crate::tabs::put_beside(&mut self.tabs, &mut self.active, pane, beside, axis) {
                    tracing::info!(pane, beside, "moved a session into a tab");
                    self.wake_active(cx);
                    cx.notify();
                }
            }
            None => {
                let place = Place::Beside { pane: beside, axis };
                match target {
                    RowTarget::Pane(_) => {}
                    RowTarget::Remote { alias, slug } => self.attach_new(alias, slug, place, cx),
                    RowTarget::Local(session) => self.attach_local(session, place, cx),
                }
            }
        }
    }

    /// Whether an agent may be working in a session: its status says so, or
    /// an agent is there and nothing says what it is doing.
    fn agent_working(&self, target: &RowTarget) -> bool {
        use crate::glyph::State;
        let pane = self.shown_in(target);
        let look = match (pane, target) {
            (Some(pane), _) => self.pane_look(pane),
            (None, RowTarget::Local(session)) => self.session_look(*session),
            (None, _) => None,
        };
        if let Some(look) = look {
            return look.state == State::Working;
        }
        let remote = match (pane.and_then(|p| self.pane(p)), target) {
            (Some(pane), _) => pane
                .host
                .as_ref()
                .map(|h| (h.alias.clone(), pane.slug.clone())),
            (None, RowTarget::Remote { alias, slug }) => Some((alias.clone(), slug.clone())),
            _ => None,
        };
        let local = match (pane.and_then(|p| self.pane(p)), target) {
            (Some(pane), _) => pane.session,
            (None, RowTarget::Local(session)) => Some(*session),
            _ => None,
        };
        if let Some((alias, slug)) = remote {
            return self
                .running
                .get(&alias)
                .and_then(|listed| listed.get(&slug))
                .copied()
                .unwrap_or(false);
        }
        local
            .and_then(|session| self.local_sessions.iter().find(|s| s.id == session))
            .and_then(|info| info.claude.as_ref())
            .is_some_and(|c| c.state == berth_proto::v1::ClaudeState::Working)
    }

    /// End a session: its own action, never a close. Asked first while an
    /// agent is working in it; otherwise ended at once.
    fn end_session(&mut self, target: RowTarget, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            return;
        }
        let pane = self.shown_in(&target);
        let ask = self.agent_working(&target);
        let local = match (pane.and_then(|p| self.pane(p)), &target) {
            (Some(p), _) => p.session,
            (None, RowTarget::Local(session)) => Some(*session),
            _ => None,
        };
        if let Some(session) = local {
            if ask {
                self.ask_kill_local(session, pane, cx);
            } else {
                self.kill_local(&session.to_string(), pane, cx);
            }
            return;
        }
        let named = match (pane.and_then(|p| self.pane(p)), &target) {
            (Some(p), _) => p.host.as_ref().map(|h| (h.alias.clone(), p.slug.clone())),
            (None, RowTarget::Remote { alias, slug }) => Some((alias.clone(), slug.clone())),
            _ => None,
        };
        let Some((alias, slug)) = named else {
            // A shell of its own with no session behind it: closing it ends
            // it, and there is nothing else to end.
            if let Some(pane) = pane {
                self.close(pane, cx);
            }
            return;
        };
        if ask {
            let session = berth_core::session::Session::new(slug.clone(), 80, 24).tmux_name();
            self.modal = Some(Modal::Kill {
                pane,
                alias,
                slug,
                session,
            });
            self.modal_focused = false;
            cx.notify();
        } else {
            self.kill(alias, slug, pane, cx);
        }
    }

    /// Move a split's divider to where the pointer is, as a share of the
    /// split. Redrawn at once; nothing else waits on it.
    fn drag_divider(&mut self, tab: u64, path: &[bool], share: f32, cx: &mut Context<Self>) {
        let share = (share.clamp(0.0, 1.0) * 1000.0).round() as u16;
        if let Some(found) = self.tabs.iter_mut().find(|t| t.id == tab)
            && found.root.set_share(path, share)
        {
            self.dragging_divider = Some((tab, path.to_vec()));
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
            Command::NewTab => self.new_tab(cx),
            Command::SelectTab(n) => self.select_tab(n, cx),
            Command::EndSession => {
                if let Some(pane) = self.focused().map(|p| p.id) {
                    self.end_session(RowTarget::Pane(pane), cx);
                }
            }
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
        crate::tabs::remove_pane(&mut self.tabs, &mut self.active, id);

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

    /// Ask before ending a local server session (D50), shown or not.
    fn ask_kill_local(&mut self, session: u64, pane: Option<u64>, cx: &mut Context<Self>) {
        self.modal = Some(Modal::Kill {
            pane,
            alias: strings::LOCAL_ALIAS.to_string(),
            slug: session.to_string(),
            session: self
                .detached_local()
                .into_iter()
                .find(|(id, _)| *id == session)
                .map(|(_, name)| name)
                .unwrap_or_else(|| format!("{} #{session}", render::local_label())),
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
            (Action::EndSession, Target::Pane(id)) => self.end_session(RowTarget::Pane(id), cx),
            (Action::EndSession, Target::Detached { alias, slug }) => {
                self.end_session(RowTarget::Remote { alias, slug }, cx)
            }
            (Action::Open, Target::Detached { alias, slug }) => self.open_session(alias, slug, cx),
            (Action::Open, Target::LocalDetached { session }) => {
                self.attach_local(session, Place::Tab { show: true }, cx)
            }
            (Action::EndSession, Target::LocalDetached { session }) => {
                self.end_session(RowTarget::Local(session), cx)
            }
            (Action::NewTerminal, Target::Local) => {
                self.new_terminal(crate::sidebar::Folder::Local, cx)
            }
            (Action::NewTerminal, Target::Host(alias)) => {
                self.new_terminal(crate::sidebar::Folder::Host(alias), cx)
            }
            (Action::Connect, Target::Host(alias)) => self.open(alias, cx),
            (Action::RenameHost, Target::Host(alias)) => self.ask_rename_host(alias, cx),
            (Action::Server, Target::Host(alias)) => self.ask_server(alias, cx),
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

    /// A tab's title: the program's status message first, then the title it
    /// gave the terminal, then its name and directory, then nothing.
    #[test]
    fn a_tab_title_falls_back_in_order() {
        let home = Some("/Users/j");
        let title = |message, terminal, program, directory| {
            tab_title(message, terminal, program, directory, home)
        };
        assert_eq!(
            title(
                Some("Running tests"),
                Some("vim"),
                Some("claude"),
                Some("/Users/j/work")
            ),
            Some("Running tests".into())
        );
        assert_eq!(
            title(
                None,
                Some("vim: notes.md"),
                Some("nvim"),
                Some("/Users/j/work")
            ),
            Some("vim: notes.md".into())
        );
        assert_eq!(
            title(
                None,
                None,
                Some("/usr/bin/python3.12"),
                Some("/Users/j/work/api")
            ),
            Some("python api".into())
        );
        assert_eq!(
            title(None, None, Some("-zsh"), Some("/Users/j")),
            Some("zsh ~".into())
        );
        assert_eq!(title(None, None, Some("htop"), None), Some("htop".into()));
        // Empty or blank is the same as not set.
        assert_eq!(
            title(Some("  "), Some(""), Some("git"), Some("/")),
            Some("git /".into())
        );
        assert_eq!(title(None, None, None, Some("/Users/j")), None);
        assert_eq!(title(None, None, Some(" "), None), None);
    }

    /// A title is words on a tab and nothing more. The icon and the accent
    /// come from the process name alone, whatever a title claims.
    #[test]
    fn a_title_never_chooses_the_icon() {
        let claimed = tab_title(None, Some("claude"), Some("vim"), None, None);
        assert_eq!(claimed.as_deref(), Some("claude"), "shown as written");
        let program = crate::programs::lookup("vim");
        assert_eq!(program.icon, crate::icons::VIM);
        assert_eq!(program.accent, None);
    }

    #[test]
    fn a_directory_is_shortened_to_its_last_part() {
        assert_eq!(short_directory("/Users/j", Some("/Users/j")), "~");
        assert_eq!(short_directory("/Users/j/", Some("/Users/j")), "~");
        assert_eq!(
            short_directory("/Users/j/work/api", Some("/Users/j")),
            "api"
        );
        assert_eq!(short_directory("/", None), "/");
        assert_eq!(short_directory("/tmp", None), "tmp");
    }

    /// The done dot clears once its pane has been in front of the user for
    /// about a second: shown on two looks running. One look is not enough,
    /// looking away starts over, and each showing is seen once.
    #[test]
    fn a_status_is_seen_after_two_looks_running() {
        let mut looked = Looked::default();
        let pane = StatusKey::Pane(3);
        assert_eq!(looked.look(Some(pane)), None, "one look");
        assert_eq!(
            looked.look(Some(pane)),
            Some(pane),
            "still there a second on"
        );
        assert_eq!(
            looked.look(Some(pane)),
            None,
            "seen once, not again at once"
        );

        // Focus moved away in between: the count starts over.
        let mut looked = Looked::default();
        assert_eq!(looked.look(Some(pane)), None);
        assert_eq!(looked.look(None), None);
        assert_eq!(looked.look(Some(pane)), None);
        assert_eq!(looked.look(Some(pane)), Some(pane));

        // A different pane in front is a different thing to see.
        let mut looked = Looked::default();
        assert_eq!(looked.look(Some(pane)), None);
        assert_eq!(looked.look(Some(StatusKey::Session(3))), None);
        assert_eq!(
            looked.look(Some(StatusKey::Session(3))),
            Some(StatusKey::Session(3))
        );
    }

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
