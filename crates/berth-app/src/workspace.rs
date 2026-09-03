//! The root view: sidebar, main area, status bar.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AppContext, Context, Entity, Focusable, IntoElement, ParentElement, Render, Styled, Window,
    div, px,
};

use berth_core::config::SshConfig;
use berth_core::connection::ConnectionState;
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
    warnings: Vec<String>,
    ssh_config_path: Option<String>,
    theme: Theme,
    pane: Pane,
    state: ConnectionState,
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
            state: ConnectionState::Disconnected,
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
        self.pane = Pane::Connecting(alias.clone());
        self.state = ConnectionState::Connecting;

        let slug = std::env::var("BERTH_SESSION").unwrap_or_else(|_| "main".to_string());
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn(session::connect(host, slug, 80, 24))
                .await
                .map_err(|_| "the runtime dropped the connection task".to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));

            match result {
                Ok(connected) => {
                    let _ = this.update(cx, |this, cx| {
                        let terminal = connected.terminal.clone();
                        let pty = connected.pty.clone();
                        let view = cx.new(|cx| {
                            TerminalView::new(terminal, Some(pty), "monospace", 13.0, cx)
                        });
                        view.update(cx, |_, cx| {
                            session::start_pump(&cx.entity(), connected.events, cx);
                        });
                        this.connection = Some(connected.connection.clone());
                        this.state = ConnectionState::Connected;
                        this.pane = Pane::Session(view.clone());
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
{report}"
                            );
                            let _ = runtime::spawn(async move {
                                let _ = connection.exec(&crate::bench::remove_file_command()).await;
                            })
                            .await;
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
                        this.state = ConnectionState::Disconnected;
                        this.pane = Pane::Failed(error);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
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
                self.state
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
            .child(strings::connection_state(self.state))
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
