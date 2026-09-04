//! The root view: sidebar, main area, status bar.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AppContext, AsyncApp, Context, Entity, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement, Styled, WeakEntity, Window, div, px,
};

use std::path::PathBuf;

use berth_core::config::SshConfig;
use berth_core::connection::ConnectionState;
use berth_core::connection::state::{Action, Event};
use berth_core::forward::Forwarder;
use berth_core::host::Host;
use berth_core::transport::Connection;

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
            .rounded(px(6.0))
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
    /// Whatever the connection is doing, as the reconnect machine sees it.
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

/// A session that is up but not on screen.
///
/// The connection travels with the view because switching hosts has to carry
/// the transport too, and the forwards travel with it because dropping a
/// `Forwarder` closes its listener: a forward whose port is in use must not go
/// down just because its host is no longer the one being looked at.
struct Warm {
    view: Entity<TerminalView>,
    connection: Arc<Connection>,
    forwarders: Vec<Forwarder>,
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

/// What the main area is showing.
enum Pane {
    FirstRun,
    Connecting(String),
    Failed(String),
    Session(Entity<TerminalView>),
}

pub struct Workspace {
    hosts: Vec<Host>,
    /// What a reconnect re-opens. Kept so the driver does not have to guess,
    /// and so the same slug lands in the same tmux session every time.
    session_host: Option<Host>,
    session_slug: String,
    /// Live forwards for the current session. Dropping one closes its
    /// listener, which is how a reconnect retires the forwards belonging to
    /// the connection that died.
    forwarders: Vec<Forwarder>,
    /// Sessions brought up at startup but not on screen, so switching to one
    /// is instant rather than a fresh connect and attach.
    warmed: std::collections::BTreeMap<String, Warm>,
    /// The host key question currently in front of the user, if any.
    prompt: Option<Prompt>,
    /// Per-host status, so the sidebar can show hosts landing independently.
    statuses: std::collections::BTreeMap<String, HostStatus>,
    /// When the process started, for the cold-start measurement.
    started: std::time::Instant,
    /// Reported once, when the first host reaches a prompt.
    cold_start_reported: bool,
    warnings: Vec<String>,
    ssh_config_path: Option<String>,
    theme: Theme,
    pane: Pane,
    /// The reconnect machine and its clock. The state shown in the UI is this
    /// machine's, never a value set by hand at a call site, so the two cannot
    /// drift apart.
    reconnect: crate::reconnect::Driver,
    connection: Option<Arc<Connection>>,
    rtt: Option<Duration>,
    /// The terminal takes focus once, when it first appears.
    focused: bool,
    /// Which host the keepalive loop belongs to. Switching hosts starts a new
    /// loop, and the old one exits when it sees a number that is not its own,
    /// so two loops can never tick the same machine.
    driving: u64,
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
            pane: Pane::FirstRun,
            session_host: None,
            session_slug: String::new(),
            forwarders: Vec::new(),
            warmed: std::collections::BTreeMap::new(),
            prompt: None,
            statuses: std::collections::BTreeMap::new(),
            started: std::time::Instant::now(),
            cold_start_reported: false,
            reconnect: crate::reconnect::Driver::new(),
            connection: None,
            rtt: None,
            focused: false,
            driving: 0,
        };

        this.warm(cx);
        this
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
        let only = self.startup_host().map(|h| h.alias);

        for host in hosts {
            // BERTH_HOST, or a single-host config, still picks what the pane
            // shows. Warming decides what is connected, not what is visible.
            let visible = only.as_deref() == Some(host.alias.as_str());

            if !berth_core::known_hosts::is_known(&host.hostname, host.port) {
                self.statuses
                    .insert(host.alias.clone(), HostStatus::NeedsVerify);
                // The one host the user asked for by name is not warming: it is
                // the same request a click makes, so it gets the same answer,
                // a fingerprint to look at. Showing an empty pane instead would
                // be a launch that silently did nothing.
                if visible {
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
            if visible {
                self.connect(host, cx);
            } else {
                self.warm_one(host, cx);
            }
        }
    }

    /// Bring a host up without showing it, so switching to it later is instant.
    fn warm_one(&mut self, host: Host, cx: &mut Context<Self>) {
        let alias = host.alias.clone();
        let slug = std::env::var("BERTH_SESSION").unwrap_or_else(|_| "main".to_string());
        let forwards = host.forwards.clone();
        cx.spawn(async move |this, cx| {
            let palette = crate::terminal_style::TerminalStyle::default().palette;
            let installed = cx.update(|cx| cx.text_system().all_font_names());
            let result = runtime::spawn(session::connect(host, slug, 80, 24, palette))
                .await
                .map_err(|_| "the runtime dropped the connection task".to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));

            match result {
                Ok(connected) => {
                    // A warmed host's forwards come up with it and stay up. The
                    // port is wanted by whatever is using it, not by whichever
                    // host happens to be on screen.
                    let forwarders =
                        session::start_forwards(connected.connection.clone(), &forwards).await;
                    let _ = this.update(cx, |this, cx| {
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
                        this.statuses.insert(
                            alias.clone(),
                            HostStatus::Connection(ConnectionState::Connected),
                        );
                        // Nothing on screen yet, so the first host to land gets
                        // the pane. Showing the first-run screen while several
                        // hosts are connected behind it is simply wrong, and
                        // whichever host answered first is the one the user can
                        // start typing into soonest.
                        if matches!(this.pane, Pane::FirstRun | Pane::Connecting(_)) {
                            this.session_host =
                                this.hosts.iter().find(|h| h.alias == alias).cloned();
                            this.session_slug = std::env::var("BERTH_SESSION")
                                .unwrap_or_else(|_| "main".to_string());
                            this.connection = Some(connected.connection.clone());
                            this.forwarders = forwarders;
                            this.reconnect.handle(Event::Open);
                            this.reconnect.handle(Event::ConnectSucceeded);
                            this.pane = Pane::Session(view.clone());
                            this.drive(cx);
                        } else {
                            this.warmed.insert(
                                alias.clone(),
                                Warm {
                                    view,
                                    connection: connected.connection.clone(),
                                    forwarders,
                                },
                            );
                        }
                        this.report_cold_start(&alias);
                        cx.notify();
                    });
                }
                Err(error) => {
                    // Recorded against this host only. A host that refuses must
                    // not take the others down with it.
                    tracing::warn!(%alias, %error, "warming failed");
                    let _ = this.update(cx, |this, cx| {
                        this.statuses
                            .insert(alias.clone(), HostStatus::Failed(error));
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

    /// Act on a click on a host row.
    ///
    /// What happens is decided from what is true now rather than from the
    /// status the sidebar last drew: a warm host is shown, a host with no
    /// recorded key is verified first, anything else is connected. So a failed
    /// row is retried by clicking it, and a row that failed because it was
    /// never trusted asks the question instead of retrying the failure.
    fn open(&mut self, alias: String, cx: &mut Context<Self>) {
        // One question at a time, and never a connection started behind the
        // dialog that is asking whether to make one.
        if self.prompt.is_some() {
            return;
        }
        if matches!(self.statuses.get(&alias), Some(HostStatus::Verifying)) {
            return;
        }
        if self.warmed.contains_key(&alias) {
            self.show(&alias, cx);
            return;
        }
        if matches!(self.pane, Pane::Session(_))
            && self.session_host.as_ref().is_some_and(|h| h.alias == alias)
        {
            return;
        }
        let Some(host) = self.hosts.iter().find(|h| h.alias == alias).cloned() else {
            return;
        };
        if berth_core::known_hosts::is_known(&host.hostname, host.port) {
            self.statuses
                .insert(alias, HostStatus::Connection(ConnectionState::Connecting));
            self.connect(host, cx);
        } else {
            self.verify(host, cx);
        }
    }

    /// Put an already-warm session on screen, parking the one it replaces.
    ///
    /// Parking rather than dropping: the outgoing connection, its grid and its
    /// forwards all stay up, so coming back costs the same nothing that going
    /// did, and a port someone is using does not close because the user looked
    /// at another host.
    fn show(&mut self, alias: &str, cx: &mut Context<Self>) {
        let Some(warm) = self.warmed.remove(alias) else {
            return;
        };
        let Some(host) = self.hosts.iter().find(|h| h.alias == alias).cloned() else {
            return;
        };

        if let Pane::Session(view) = &self.pane
            && let (Some(previous), Some(connection)) =
                (self.session_host.clone(), self.connection.clone())
        {
            let parked = Warm {
                view: view.clone(),
                connection,
                forwarders: std::mem::take(&mut self.forwarders),
            };
            self.statuses.insert(
                previous.alias.clone(),
                HostStatus::Connection(ConnectionState::Connected),
            );
            self.warmed.insert(previous.alias, parked);
        }

        self.session_host = Some(host);
        self.session_slug = std::env::var("BERTH_SESSION").unwrap_or_else(|_| "main".to_string());
        self.connection = Some(warm.connection);
        self.forwarders = warm.forwarders;
        self.pane = Pane::Session(warm.view);
        // The incoming terminal has to take the keyboard, or the user is
        // looking at one session and typing into another.
        self.focused = false;

        // The machine follows the visible session, so a parked host is not
        // watched: if its link died while parked, that is noticed on the first
        // keepalive after it comes back rather than at the moment it happened.
        // Removing that is tabs' job, when each session gets its own machine.
        self.reconnect = crate::reconnect::Driver::new();
        self.reconnect.handle(Event::Open);
        self.reconnect.handle(Event::ConnectSucceeded);
        self.statuses.insert(
            alias.to_string(),
            HostStatus::Connection(ConnectionState::Connected),
        );
        self.drive(cx);
        cx.notify();
    }

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
                        this.prompt = Some(Prompt {
                            alias,
                            endpoint,
                            fingerprint,
                            key,
                        });
                    }
                    // Recorded between startup and the click, so there is
                    // nothing to ask and the click meant "open this".
                    Ok(Ok(session::Offer::Trusted)) => {
                        if let Some(host) = this.hosts.iter().find(|h| h.alias == alias).cloned() {
                            this.statuses
                                .insert(alias, HostStatus::Connection(ConnectionState::Connecting));
                            this.connect(host, cx);
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

    /// Record the key the user accepted, then open the host.
    fn accept(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
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
                self.connect(host, cx);
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
        cx.notify();
    }

    /// Decline. Nothing is written and the host goes back to waiting.
    fn reject(&mut self, cx: &mut Context<Self>) {
        if let Some(prompt) = self.prompt.take() {
            tracing::info!(alias = %prompt.alias, "host key declined");
            self.statuses.insert(prompt.alias, HostStatus::NeedsVerify);
        }
        cx.notify();
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

    fn connect(&mut self, host: Host, cx: &mut Context<Self>) {
        let alias = host.alias.clone();
        tracing::info!(%alias, "connecting");
        // The grid stays up across a reconnect. Only a first connect, which
        // has no grid yet, shows the connecting pane.
        if !matches!(self.pane, Pane::Session(_)) {
            self.pane = Pane::Connecting(alias.clone());
        }
        self.reconnect.handle(Event::Open);

        let slug = std::env::var("BERTH_SESSION").unwrap_or_else(|_| "main".to_string());
        self.session_host = Some(host.clone());
        self.session_slug = slug.clone();
        let forwards = host.forwards.clone();
        cx.spawn(async move |this, cx| {
            let palette = crate::terminal_style::TerminalStyle::default().palette;
            // Resolved once here, against what the platform actually has, so
            // the grid is not built on a proportional substitute.
            let installed = cx.update(|cx| cx.text_system().all_font_names());
            let result = runtime::spawn(session::connect(host, slug, 80, 24, palette))
                .await
                .map_err(|_| "the runtime dropped the connection task".to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));

            match result {
                Ok(connected) => {
                    // Same ordering as a reconnect: ports before the state.
                    let forwarders =
                        session::start_forwards(connected.connection.clone(), &forwards).await;
                    let _ = this.update(cx, |this, cx| {
                        let terminal = connected.terminal.clone();
                        let pty = connected.pty.clone();
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
                            session::start_pump(&cx.entity(), connected.events, cx);
                        });
                        this.connection = Some(connected.connection.clone());
                        this.forwarders = forwarders;
                        this.reconnect.handle(Event::ConnectSucceeded);
                        this.pane = Pane::Session(view.clone());
                        this.statuses.insert(
                            alias.clone(),
                            HostStatus::Connection(this.reconnect.state()),
                        );
                        this.report_cold_start(&alias);
                        this.drive(cx);
                        cx.notify();
                    });

                    // Benchmarks drive the real render loop, then quit.
                    if let Some(mode) = crate::bench::Bench::from_env() {
                        let view = this
                            .read_with(cx, |t, _| match &t.pane {
                                Pane::Session(v) => Some(v.downgrade()),
                                _ => None,
                            })
                            .ok()
                            .flatten();
                        if let Some(view) = view {
                            let connection = connected.connection.clone();
                            let pty = connected.pty.clone();
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
                            println!(
                                "
{}",
                                report.text
                            );
                            let _ = runtime::spawn(async move {
                                let _ = connection.exec(&crate::bench::remove_file_command()).await;
                            })
                            .await;
                            if !report.passed {
                                // Exit non-zero so a regression is a failure,
                                // not a number someone has to notice.
                                eprintln!("benchmark did not meet its criteria");
                                std::process::exit(1);
                            }
                            cx.update(|cx| cx.quit());
                            return;
                        }
                    }
                    // A first RTT sample for the status bar.
                    if let Ok(Some(connection)) = this.read_with(cx, |t, _| t.connection.clone()) {
                        let rtt = runtime::spawn(session::round_trip(connection)).await;
                        if let Ok(Some(rtt)) = rtt {
                            let _ = this.update(cx, |this, cx| {
                                this.rtt = Some(rtt);
                                cx.notify();
                            });
                        }
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "connection failed");
                    let _ = this.update(cx, |this, cx| {
                        this.statuses
                            .insert(alias.clone(), HostStatus::Failed(error.clone()));
                        this.reconnect.handle(Event::ConnectFailed);
                        // A failure with a grid already on screen keeps it:
                        // the machine is Reconnecting, not Disconnected, and
                        // the last screen stays correct until told otherwise.
                        if !this.reconnect.holds_grid() {
                            this.pane = Pane::Failed(error);
                        }
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// Run the machine's clock: keepalives while connected, the slide into
    /// Degraded when they stop being answered, and the backoff after that.
    ///
    /// The loop sleeps until the machine's own next deadline rather than
    /// polling, and exits when the machine says it is resting, because only an
    /// external event can move it from there.
    fn drive(&mut self, cx: &mut Context<Self>) {
        self.driving += 1;
        let mine = self.driving;
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(Some(wait)) = this.read_with(cx, |this, _| {
                    // A loop that no longer owns the visible session exits.
                    // Two loops on one machine would tick it twice and send
                    // keepalives at double the interval the machine expects.
                    if this.driving != mine {
                        return None;
                    }
                    this.reconnect.next_wait()
                }) else {
                    return;
                };
                cx.background_executor().timer(wait).await;

                let Ok(step) = this.update(cx, |this, cx| {
                    let step = this.reconnect.handle(Event::Tick);
                    if step.changed {
                        tracing::info!(state = ?step.state, "connection state");
                        cx.notify();
                    }
                    step
                }) else {
                    return;
                };

                for action in step.actions {
                    match action {
                        Action::SendKeepalive => {
                            let Ok(Some(connection)) =
                                this.read_with(cx, |this, _| this.connection.clone())
                            else {
                                return;
                            };
                            // Deliberately not awaited inline: a keepalive on a
                            // dead link blocks until its own timeout, and the
                            // machine has to keep ticking through that or it
                            // can never reach Degraded.
                            tracing::debug!("keepalive");
                            let alive = this.clone();
                            cx.spawn(async move |cx| {
                                if runtime::spawn(crate::reconnect::keepalive(connection))
                                    .await
                                    .unwrap_or(false)
                                {
                                    let _ = alive.update(cx, |this, cx| {
                                        if this.reconnect.handle(Event::KeepaliveAck).changed {
                                            cx.notify();
                                        }
                                    });
                                }
                            })
                            .detach();
                        }
                        Action::Connect => {
                            Self::reattach(&this, cx).await;
                        }
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

    /// Rebuild the transport underneath the live view.
    ///
    /// Everything the session needs is done before `ConnectSucceeded` is fed
    /// in: the connection, the tmux attach, the pty swap, the pump, and the
    /// port forwards. The machine only returns to Connected once the session
    /// is genuinely usable, rather than announcing it and then catching up.
    async fn reattach(this: &WeakEntity<Self>, cx: &mut AsyncApp) {
        let Ok(Some((host, slug, view))) = this.read_with(cx, |this, _| {
            let Pane::Session(view) = &this.pane else {
                return None;
            };
            Some((
                this.session_host.clone()?,
                this.session_slug.clone(),
                view.clone(),
            ))
        }) else {
            return;
        };
        let forwards = host.forwards.clone();

        // Attach at the size the grid already is, so tmux repaints into the
        // same shape and nothing reflows on the way back.
        let size = view.read_with(cx, |view, _| view.grid_size());

        let attached = runtime::spawn(session::attach(host, slug, size.cols, size.rows)).await;
        let attached = match attached {
            Ok(Ok(attached)) => attached,
            failed => {
                if let Ok(Err(error)) = failed {
                    tracing::warn!(%error, "reattach failed");
                }
                let _ = this.update(cx, |this, cx| {
                    this.reconnect.handle(Event::ConnectFailed);
                    cx.notify();
                });
                return;
            }
        };

        // Forwards come back before the state does, so nothing ever sees a
        // session reported usable with its ports dead. The old forwarders are
        // dropped first: their listeners belong to a connection that no longer
        // exists, and holding them would keep the local ports bound to nothing.
        let _ = this.update(cx, |this, _| this.forwarders.clear());
        let forwarders = session::start_forwards(attached.connection.clone(), &forwards).await;

        let _ = this.update(cx, |this, cx| {
            this.connection = Some(attached.connection.clone());
            this.forwarders = forwarders;
            view.update(cx, |view, cx| {
                // The grid is untouched. tmux's repaint overwrites it in place.
                view.swap_pty(attached.pty.clone());
                session::start_pump(&cx.entity(), attached.events, cx);
            });
            let step = this.reconnect.handle(Event::ConnectSucceeded);
            tracing::info!(
                state = ?step.state,
                forwards = this.forwarders.len(),
                "reattached"
            );
            cx.notify();
        });
    }

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

        for (index, host) in self.hosts.iter().enumerate() {
            let status = self
                .statuses
                .get(&host.alias)
                .cloned()
                .unwrap_or(HostStatus::Connection(ConnectionState::Disconnected));
            let needs_verify = matches!(status, HostStatus::NeedsVerify);
            let dot = if needs_verify {
                theme.warning
            } else {
                theme.state_colour(status.state())
            };

            let alias = host.alias.clone();
            let showing = matches!(self.pane, Pane::Session(_))
                && self.session_host.as_ref().is_some_and(|h| h.alias == alias);

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
                        .bg(hsla(dot)),
                )
                .child(div().text_color(hsla(theme.text)).child(host.alias.clone()));

            // The row on screen is marked, so a sidebar of connected hosts
            // still says which one is being looked at.
            if showing {
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

        for (index, forward) in self.forwarders.iter().map(|f| f.forward()).enumerate() {
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

    /// The host key question, over everything else.
    ///
    /// Modal on purpose: this is the one moment where a wrong click means
    /// trusting a machine the user did not mean to, so it does not sit in a
    /// corner competing with a terminal for attention.
    fn key_prompt(&self, prompt: &Prompt, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
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
                    .border_color(hsla(theme.border))
                    .child(
                        div()
                            .text_color(hsla(theme.text))
                            .child(strings::VERIFY_TITLE),
                    )
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
                            // Selectable would be better, but reading it aloud
                            // against the server is what this is for and the
                            // full string has to be visible to do that.
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
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .justify_end()
                            .child(
                                div()
                                    .id("reject")
                                    .cursor_pointer()
                                    .px(px(12.0))
                                    .py(px(6.0))
                                    .rounded(px(4.0))
                                    .border_1()
                                    .border_color(hsla(theme.border))
                                    .text_color(hsla(theme.text))
                                    .hover(|b| b.bg(hsla(theme.border)))
                                    .on_click(cx.listener(|this, _, _, cx| this.reject(cx)))
                                    .child(strings::VERIFY_REJECT),
                            )
                            .child(
                                div()
                                    .id("accept")
                                    .cursor_pointer()
                                    .px(px(12.0))
                                    .py(px(6.0))
                                    .rounded(px(4.0))
                                    .bg(hsla(theme.accent))
                                    .text_color(hsla(theme.background))
                                    .on_click(cx.listener(|this, _, _, cx| this.accept(cx)))
                                    .child(strings::VERIFY_ACCEPT),
                            ),
                    ),
            )
    }

    fn status_bar(&self) -> impl IntoElement {
        let theme = self.theme;
        let rtt = match self.rtt {
            Some(rtt) => format!("{} ms", rtt.as_millis()),
            None => "-".to_string(),
        };
        div()
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
            .child(match &self.pane {
                // The host actually on screen, which with warming is whichever
                // answered first and not necessarily the first in the file.
                Pane::Session(_) | Pane::Connecting(_) => self
                    .session_host
                    .as_ref()
                    .map(|h| h.alias.clone())
                    .unwrap_or_else(|| strings::STATUS_NO_HOST.to_string()),
                _ => strings::STATUS_NO_HOST.to_string(),
            })
            .child(strings::connection_state(self.reconnect.state()))
            .child(rtt)
            .child(strings::STATUS_NOT_RECORDING)
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Light and dark follow the OS.
        self.theme = Theme::for_appearance(window.appearance().into());
        let theme = self.theme;

        if let Pane::Session(view) = &self.pane
            && !self.focused
            // A dialog is in front of the terminal, and the terminal must not
            // take the keyboard out from under it.
            && self.prompt.is_none()
        {
            // Without this the terminal never receives a keystroke.
            window.focus(&view.focus_handle(cx), cx);
            self.focused = true;
        }

        let main = match &self.pane {
            Pane::Session(view) => div().flex().flex_1().child(view.clone()),
            Pane::Connecting(alias) => centred(
                div()
                    .text_color(hsla(theme.text_muted))
                    .child(format!("{} {alias}", strings::STATE_CONNECTING)),
            ),
            Pane::Failed(error) => centred(
                div()
                    .max_w(px(560.0))
                    .text_color(hsla(theme.warning))
                    .child(error.clone()),
            ),
            Pane::FirstRun => centred(self.first_run()),
        };

        let mut root = div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(hsla(theme.background))
            .text_sm()
            .child(div().flex().flex_1().child(self.sidebar(cx)).child(main))
            .child(self.status_bar());

        if let Some(prompt) = &self.prompt {
            root = root.child(self.key_prompt(prompt, cx));
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
