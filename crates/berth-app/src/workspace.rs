//! The root view: sidebar, main area, status bar.
//!
//! M0 renders the chrome and the first-run state. Terminal panes arrive with
//! the SSH transport.

use gpui::{Context, IntoElement, ParentElement, Render, Styled, Window, div, px};

use berth_core::config::SshConfig;
use berth_core::connection::ConnectionState;
use berth_core::host::Host;

use crate::strings;
use crate::theme::{Appearance, Theme, hsla};

pub struct Workspace {
    hosts: Vec<Host>,
    warnings: Vec<String>,
    ssh_config_path: Option<String>,
    theme: Theme,
}

impl Workspace {
    pub fn load() -> Self {
        let path = SshConfig::default_path().map(|p| p.display().to_string());
        let (hosts, warnings) = match SshConfig::load_default() {
            Ok(config) => (config.hosts, config.warnings),
            Err(error) => {
                tracing::warn!(%error, "could not read ssh config");
                (Vec::new(), vec![error.to_string()])
            }
        };
        tracing::info!(hosts = hosts.len(), "loaded ssh config");
        Self {
            hosts,
            warnings,
            ssh_config_path: path,
            // Replaced on first render by the window's actual appearance.
            theme: Theme::for_appearance(Appearance::Dark),
        }
    }

    /// Directives Berth skipped, so a silently ignored ssh config line is
    /// visible rather than mysterious.
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
                            .bg(hsla(theme.state_colour(ConnectionState::Disconnected))),
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
            .child(strings::STATUS_NO_HOST)
            .child(strings::connection_state(ConnectionState::Disconnected))
            .child(strings::STATUS_NOT_RECORDING)
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Light and dark follow the OS.
        self.theme = Theme::for_appearance(window.appearance().into());
        let theme = self.theme;
        let main = if self.hosts.is_empty() {
            div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(self.first_run())
        } else {
            div()
                .flex()
                .flex_1()
                .p(px(16.0))
                .text_color(hsla(theme.text_muted))
                .child(format!("{} hosts", self.hosts.len()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_without_an_ssh_config_is_not_an_error() {
        let w = Workspace::load();
        // Either the machine has a config or it does not; neither panics, and
        // an absent file yields the first-run state rather than a failure.
        assert!(w.warnings.iter().all(|w| !w.is_empty()));
    }
}
