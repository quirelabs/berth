//! The root view: sidebar, main area, status bar.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AppContext, AsyncApp, Context, Entity, Focusable, IntoElement, ParentElement, Render, Styled,
    WeakEntity, Window, div, px,
};

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
}

impl Workspace {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let path = SshConfig::default_path().map(|p| p.display().to_string());
        let (hosts, warnings) = match SshConfig::load_default() {
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
            reconnect: crate::reconnect::Driver::new(),
            connection: None,
            rtt: None,
            focused: false,
        };

        // Connect straight away to the host named by BERTH_HOST, or the only
        // host in ssh config. Anything more deliberate waits for the command
        // palette in M1.
        if let Some(host) = this.startup_host() {
            this.connect(host, cx);
        }
        this
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
    fn drive(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(Some(wait)) = this.read_with(cx, |this, _| this.reconnect.next_wait())
                else {
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

    fn sidebar(&self) -> impl IntoElement {
        let theme = self.theme;
        let mut list = div().flex().flex_col().gap(px(2.0));

        for host in &self.hosts {
            let connected = matches!(self.pane, Pane::Session(_)) && self.hosts.len() == 1
                || Some(host.alias.as_str()) == std::env::var("BERTH_HOST").ok().as_deref();
            let state = if connected {
                self.reconnect.state()
            } else {
                ConnectionState::Disconnected
            };
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(10.0))
                    .py(px(4.0))
                    .child(
                        div()
                            .w(px(6.0))
                            .h(px(6.0))
                            .rounded(px(3.0))
                            .bg(hsla(theme.state_colour(state))),
                    )
                    .child(div().text_color(hsla(theme.text)).child(host.alias.clone())),
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
                Pane::Session(_) | Pane::Connecting(_) => self
                    .hosts
                    .first()
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
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Light and dark follow the OS.
        self.theme = Theme::for_appearance(window.appearance().into());
        let theme = self.theme;

        if let Pane::Session(view) = &self.pane
            && !self.focused
        {
            // Without this the terminal never receives a keystroke.
            window.focus(&view.focus_handle(_cx), _cx);
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

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(hsla(theme.background))
            .text_sm()
            .child(div().flex().flex_1().child(self.sidebar()).child(main))
            .child(self.status_bar())
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
