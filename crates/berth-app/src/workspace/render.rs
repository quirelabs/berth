//! What the workspace looks like.
//!
//! Split out because it is the half that changes for a reason the other half
//! never hears about, and because nothing above it calls in: `Render` is
//! invoked by gpui, and every method here is reached from there. That is why
//! none of it needs to be visible to its parent.
//!
//! `use super::*` rather than a list: this was one file, and the imports it
//! needs are the ones it already had.

use super::*;

impl Workspace {
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
        let showing = self.focused().map(|p| p.alias().to_string());

        // Grouped, in file order, with the heading printed when it changes.
        // Sorting by group would reorder the file, and the order someone put
        // their own ssh config in is information.
        let mut heading: Option<&str> = None;
        for (index, host) in self.hosts.iter().enumerate() {
            if host.group.as_deref() != heading {
                heading = host.group.as_deref();
                if let Some(name) = heading {
                    list = list.child(
                        div()
                            .px(px(10.0))
                            .pt(px(6.0))
                            .text_xs()
                            .text_color(hsla(theme.text_muted))
                            .child(name.to_string()),
                    );
                }
            }
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
            .and_then(|p| self.conns.get(p.alias()))
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
            // Above the hosts, because it is the one row that works before
            // anything is configured. An app whose first screen is "add a host
            // first" has no first ten seconds.
            .child(
                div()
                    .id("local")
                    .cursor_pointer()
                    .on_click(
                        cx.listener(|this, _, _, cx| {
                            this.open_local(Place::Tab { show: true }, cx)
                        }),
                    )
                    .hover(|row| row.bg(hsla(theme.border)))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(10.0))
                    .py(px(4.0))
                    .child(
                        div()
                            .flex_shrink_0()
                            .w(px(6.0))
                            .h(px(6.0))
                            .rounded(px(3.0))
                            .bg(theme.host_colour(strings::LOCAL_ALIAS, false)),
                    )
                    .child(
                        div()
                            .text_color(hsla(theme.text))
                            .child(strings::SIDEBAR_LOCAL),
                    ),
            )
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
            let colour = theme.host_colour(pane.alias(), pane.is_prod());
            let split_count = tab.root.panes().len();
            let local = pane.is_local();
            let agent = self.has_agent(pane);
            // A named session says its name; an unnamed one is just its host,
            // because `web-1/main` on every tab is noise.
            let label = if !local && pane.slug != berth_core::session::DEFAULT_SLUG {
                format!("{}/{}", pane.alias(), pane.slug)
            } else {
                pane.alias().to_string()
            };

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
                        .id(("label", tab_id as usize))
                        .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                            // Renaming is a double click, as it is on a tab
                            // everywhere else. A single click switches, and
                            // that must stay the cheap one.
                            if event.click_count() >= 2 {
                                this.ask_rename(id, cx);
                            }
                        }))
                        .text_color(hsla(if active { theme.text } else { theme.text_muted }))
                        .child(label),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(6.0))
                        .h(px(6.0))
                        .rounded(px(3.0))
                        .bg(hsla(theme.state_colour(self.state_of(id)))),
                );

            // Between the name and the state dot, because it is about what is
            // inside the session rather than about the connection.
            if agent {
                row = row.child(
                    div()
                        .id(("agent", tab_id as usize))
                        .flex_shrink_0()
                        .child(
                            svg()
                                .path(crate::icons::GHOST)
                                .size(px(13.0))
                                .text_color(hsla(theme.accent)),
                        )
                        .tooltip(move |_, cx| {
                            cx.new(|_| Note {
                                text: strings::AGENT_TOOLTIP.into(),
                                theme,
                            })
                            .into()
                        }),
                );
            }

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
                row = row.bg(hsla(theme.background)).child(
                    div()
                        .id(("detach", tab_id as usize))
                        .cursor_pointer()
                        .px(px(4.0))
                        .text_color(hsla(theme.text_muted))
                        .hover(|b| b.text_color(hsla(theme.text)))
                        .on_click(cx.listener(move |this, _, _, cx| this.close(id, cx)))
                        .tooltip(move |_, cx| {
                            cx.new(|_| Note {
                                text: if local {
                                    strings::TAB_CLOSE_TOOLTIP.into()
                                } else {
                                    strings::TAB_DETACH_TOOLTIP.into()
                                },
                                theme,
                            })
                            .into()
                        })
                        .child(if local {
                            strings::TAB_CLOSE
                        } else {
                            strings::TAB_DETACH
                        }),
                );
                // A local shell has no tmux session behind it, so closing it
                // ends it. Offering a separate kill would confirm an action
                // that does nothing the close did not already do.
                if !local {
                    row = row.child(
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
        let (title, accept, reject, danger) =
            match self.modal.as_ref() {
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
                Some(Modal::Rename { pane, field }) => {
                    let local = self.pane(*pane).is_some_and(|p| p.is_local());
                    let (before, after) = field.split();
                    body = body
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .p(px(8.0))
                                .rounded(px(4.0))
                                .bg(hsla(theme.background))
                                .border_1()
                                .border_color(hsla(theme.accent))
                                .text_color(hsla(theme.text))
                                .child(before.to_string())
                                // The cursor is a block between the halves rather
                                // than a character in the string, so it can never
                                // be saved as part of the name.
                                .child(div().w(px(2.0)).h(px(16.0)).bg(hsla(theme.accent)))
                                .child(after.to_string()),
                        )
                        .child(div().text_xs().text_color(hsla(theme.text_muted)).child(
                            if local {
                                strings::RENAME_BODY_LOCAL
                            } else {
                                strings::RENAME_BODY
                            },
                        ));
                    (
                        strings::RENAME_TITLE,
                        strings::RENAME_ACCEPT,
                        strings::RENAME_REJECT,
                        false,
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
                let key = event.keystroke.key.as_str();
                let m = &event.keystroke.modifiers;
                // Anything but shift is a command, not typing, so a paste does
                // not arrive in the name as a literal `v`.
                let command = m.control || m.platform || m.alt || m.function;
                if let Some(Modal::Rename { field, .. }) = this.modal.as_mut() {
                    match field.handle(key, event.keystroke.key_char.as_deref(), command) {
                        Outcome::Edited => {
                            cx.notify();
                            return;
                        }
                        Outcome::Submit => {
                            this.confirm(cx);
                            return;
                        }
                        Outcome::Cancel => {
                            this.dismiss(cx);
                            return;
                        }
                        Outcome::Ignored => {}
                    }
                }
                match key {
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
        let rtt = match pane.and_then(|p| self.conns.get(p.alias())?.rtt) {
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
                pane.map(|p| p.alias().to_string())
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
