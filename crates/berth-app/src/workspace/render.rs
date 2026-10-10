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
use gpui::prelude::FluentBuilder as _;

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
        use crate::sidebar::{HostEntry, Input, Row};

        let theme = self.theme;
        let hosts: Vec<HostEntry> = self
            .hosts
            .iter()
            .map(|h| HostEntry {
                alias: &h.alias,
                group: h.group.as_deref(),
            })
            .collect();
        let open = self.open_sessions();
        let shell = local_label();
        let detached: Vec<(String, bool)> = self
            .detached_local()
            .into_iter()
            .map(|(id, name)| {
                // Claude running in it, as its worker last reported (D50).
                let agent = self
                    .local_sessions
                    .iter()
                    .find(|s| s.id == id)
                    .and_then(|s| s.claude.as_ref())
                    .is_some_and(|c| {
                        !matches!(
                            c.state,
                            berth_proto::v1::ClaudeState::Finished
                                | berth_proto::v1::ClaudeState::Interrupted
                        )
                    });
                (name, agent)
            })
            .collect();
        let rows = crate::sidebar::rows(&Input {
            hosts: &hosts,
            open: &open,
            running: &self.running,
            expanded: &self.expanded,
            local_label: &shell,
            local_detached: &detached,
        });

        let mut list = div().flex().flex_col().gap(px(1.0));
        for (index, row) in rows.into_iter().enumerate() {
            list = list.child(match row {
                Row::Heading(name) => div()
                    .px(px(10.0))
                    .pt(px(8.0))
                    .pb(px(2.0))
                    .text_xs()
                    .text_color(hsla(theme.text_muted))
                    .child(name)
                    .into_any_element(),
                Row::Folder {
                    folder,
                    expanded,
                    agent,
                } => self.folder_row(index, folder, expanded, agent, cx),
                Row::Session {
                    folder,
                    name,
                    pane,
                    agent,
                } => self.session_row(index, folder, name, pane, agent, cx),
                Row::NewTerminal(folder) => div()
                    .id(("new-terminal", index))
                    .cursor_pointer()
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.new_terminal(folder.clone(), cx)),
                    )
                    .hover(|row| row.bg(hsla(theme.border)).text_color(hsla(theme.text)))
                    .pl(px(INDENT))
                    .pr(px(10.0))
                    .py(px(2.0))
                    .text_xs()
                    .text_color(hsla(theme.text_muted))
                    .child(strings::SIDEBAR_NEW_TERMINAL)
                    .into_any_element(),
            });
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
            .id("sidebar")
            .w(px(220.0))
            .h_full()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .overflow_y_scroll()
            .bg(hsla(theme.surface))
            .border_r(px(1.0))
            .border_color(hsla(theme.border))
            .py(px(6.0))
            .child(list)
            .child(self.warnings())
    }

    /// A host, or this machine. Clicking a host that is not connected opens it,
    /// as a host row always did, so trust and retry are still one click away.
    fn folder_row(
        &self,
        index: usize,
        folder: crate::sidebar::Folder,
        expanded: bool,
        agent: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use crate::sidebar::Folder;

        let theme = self.theme;
        let showing = self.focused().map(|p| p.alias().to_string());
        let (name, dot, status) = match &folder {
            Folder::Local => (
                strings::SIDEBAR_LOCAL.to_string(),
                theme.host_colour(strings::LOCAL_ALIAS, false),
                None,
            ),
            Folder::Host(alias) => {
                let status = self
                    .statuses
                    .get(alias)
                    .cloned()
                    .unwrap_or(HostStatus::Connection(ConnectionState::Disconnected));
                let dot = if matches!(status, HostStatus::NeedsVerify) {
                    theme.warning.into()
                } else {
                    hsla(theme.state_colour(status.state()))
                };
                (self.host_label(alias), dot, Some(status))
            }
        };
        let alias = match &folder {
            Folder::Local => strings::LOCAL_ALIAS.to_string(),
            Folder::Host(alias) => alias.clone(),
        };
        // A closed folder holding the session on screen is marked, so the tree
        // still says where the keys are going.
        let on_screen = !expanded && showing.as_deref() == Some(alias.as_str());
        let no_local = self.panes.iter().all(|p| !p.is_local());

        let clicked = folder.clone();
        let menu_target = match &folder {
            Folder::Local => crate::menu::Target::Local,
            Folder::Host(alias) => crate::menu::Target::Host(alias.clone()),
        };
        let mut row = div()
            .id(("folder", index))
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    this.open_menu(menu_target.clone(), event.position, cx)
                }),
            )
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                // An empty "This machine" has nothing to fold. A click on it
                // means a shell, as the row always did.
                if clicked == Folder::Local && no_local {
                    this.expanded.insert(Folder::Local, true);
                    this.open_local(Place::Tab { show: true }, cx);
                } else {
                    this.click_folder(clicked.clone(), expanded, cx);
                }
            }))
            .hover(|row| row.bg(hsla(theme.border)))
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .py(px(3.0))
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(10.0))
                    .text_xs()
                    .text_color(hsla(theme.text_muted))
                    .child(if expanded {
                        strings::FOLDER_OPEN
                    } else {
                        strings::FOLDER_CLOSED
                    }),
            )
            // Never shrink: flex was collapsing this to nothing to make room
            // for a long label, which took the state indicator away from
            // exactly the host that most needed one.
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(6.0))
                    .h(px(6.0))
                    .rounded(px(3.0))
                    .bg(dot),
            )
            .child(div().text_color(hsla(theme.text)).child(name));

        if on_screen {
            row = row.bg(hsla(theme.border));
        }
        if agent {
            // The stone of an agent inside the closed folder.
            let program = match &folder {
                Folder::Host(alias) => self.programs.get(alias).and_then(|sessions| {
                    sessions
                        .values()
                        .find(|command| berth_core::agents::is_agent(command))
                        .map(String::as_str)
                }),
                Folder::Local => self
                    .local_sessions
                    .iter()
                    .filter_map(|info| info.program.as_deref())
                    .find(|command| berth_core::agents::is_agent(command)),
            };
            row = row.child(otter(
                ("folder-agent", index),
                theme,
                &OtterLook::presence(program),
                0,
            ));
        }

        if let Some(status) = status {
            // Only the states that need explaining carry a label. Connected is
            // the dot's job; "waiting for you" and "this one broke" are not.
            let needs_verify = matches!(status, HostStatus::NeedsVerify);
            if needs_verify || matches!(status, HostStatus::Verifying | HostStatus::Failed(_)) {
                row = row.child(
                    div()
                        .text_xs()
                        .text_color(hsla(theme.text_muted))
                        .child(status.label()),
                );
            }
            // Why it failed, or why it was left alone, goes on the row: this is
            // where someone is looking when they ask.
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
        }
        row.into_any_element()
    }

    /// One session under its folder. Open ones are switched to, detached ones
    /// are reattached in a tab of their own.
    fn session_row(
        &self,
        index: usize,
        folder: crate::sidebar::Folder,
        name: String,
        pane: Option<u64>,
        agent: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = self.theme;
        let focused = pane.is_some() && self.focused().map(|p| p.id) == pane;
        let detached = pane.is_none();

        let slug = name.clone();
        // A local server session no tab shows, found again by the name the
        // tree gave it.
        let local_session = self
            .detached_local()
            .into_iter()
            .find(|(_, n)| *n == name)
            .map(|(id, _)| id);
        // What runs in it, for the stone of an agent that says nothing.
        let program: Option<String> = match (pane.and_then(|id| self.pane(id)), &folder) {
            (Some(pane), _) => self.pane_program(pane),
            (None, crate::sidebar::Folder::Host(alias)) => self.remote_program(alias, &name),
            (None, crate::sidebar::Folder::Local) => {
                local_session.and_then(|session| self.session_program(session))
            }
        }
        .map(str::to_string);
        let menu_target = match (&folder, pane) {
            (_, Some(pane)) => Some(crate::menu::Target::Pane(pane)),
            (crate::sidebar::Folder::Host(alias), None) => Some(crate::menu::Target::Detached {
                alias: alias.clone(),
                slug: name.clone(),
            }),
            (crate::sidebar::Folder::Local, None) => {
                local_session.map(|session| crate::menu::Target::LocalDetached { session })
            }
        };
        let mut row = div()
            .id(("session", index))
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    if let Some(target) = menu_target.clone() {
                        this.open_menu(target, event.position, cx)
                    }
                }),
            )
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| match (&folder, pane) {
                (_, Some(pane)) => this.focus_pane(pane, cx),
                (crate::sidebar::Folder::Host(alias), None) => {
                    this.open_session(alias.clone(), slug.clone(), cx)
                }
                (crate::sidebar::Folder::Local, None) => {
                    if let Some(session) = local_session {
                        this.attach_local(session, cx)
                    }
                }
            }))
            .hover(|row| row.bg(hsla(theme.border)))
            .flex()
            .items_center()
            .gap(px(6.0))
            .pl(px(INDENT))
            .pr(px(10.0))
            .py(px(2.0))
            // Detached is dimmer: it is running, but nothing here shows it.
            .child(
                div()
                    .text_color(hsla(if detached {
                        theme.text_muted
                    } else {
                        theme.text
                    }))
                    .child(name),
            );
        if focused {
            row = row.bg(hsla(theme.border));
        }
        // What the session's programs report of themselves comes first
        // (OSC 7501, D53); Claude's hooks are the fallback for a Claude too
        // old to report that way; an agent that says nothing is asleep.
        let (key, look) = match pane {
            Some(id) => (OtterKey::Pane(id), self.pane_otter(id)),
            None => match local_session {
                Some(session) => (OtterKey::Session(session), self.session_otter(session)),
                None => (OtterKey::Session(u64::MAX), None),
            },
        };
        if let Some(look) = look.or_else(|| agent.then(|| OtterLook::presence(program.as_deref())))
        {
            let frame = self.otter_frame(key, look.pose);
            row = row.child(otter(("session-agent", index), theme, &look, frame));
        }
        if detached {
            row = row.tooltip(move |_, cx| {
                cx.new(|_| Note {
                    text: strings::SESSION_DETACHED_TOOLTIP.into(),
                    theme,
                })
                .into()
            });
        }
        row.into_any_element()
    }

    /// One tab per open session.
    ///
    /// Each carries the host's own colour, and a state dot only while its
    /// connection is not up, so a row of tabs answers "which host" and "is
    /// anything wrong" without being read word by word. The close and kill actions are on the active tab only: an inactive
    /// tab is a place to go, and putting a destructive control on it invites
    /// hitting one while aiming for the other.
    fn tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        // One row that scrolls sideways rather than squeezing: a tab too narrow
        // to read its host is worse than one you scroll to.
        let mut strip = div()
            .id("tabs")
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(1.0))
            .h(px(26.0))
            .px(px(4.0))
            .overflow_x_scroll()
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
            let state = self.state_of(id);
            // A named session says its name; an unnamed one is just its host,
            // because `web-1/main` on every tab is noise.
            let named = if !local && pane.slug != berth_core::session::DEFAULT_SLUG {
                format!("{}/{}", self.host_label(pane.alias()), pane.slug)
            } else if local {
                pane.alias().to_string()
            } else {
                self.host_label(pane.alias())
            };
            // What the program says of itself comes first (D53): its status
            // message, its terminal title, its name and directory. A remote
            // session says its host before it, since the tab is also how
            // one tells hosts apart.
            let label = match self.pane_title(pane) {
                Some(title) if local => title,
                Some(title) => format!("{named}: {title}"),
                None => named,
            };
            let program = crate::programs::lookup(self.pane_program(pane).unwrap_or_default());
            let status = self.pane_otter(id).map(|look| look.pose);

            // The cut name is still readable, on hover.
            let full: Option<gpui::SharedString> =
                (label.chars().count() > TAB_LABEL_CHARS).then(|| label.clone().into());

            let mut row = div()
                .id(("tab", tab_id as usize))
                .on_mouse_down(
                    gpui::MouseButton::Right,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                        this.open_menu(crate::menu::Target::Pane(id), event.position, cx)
                    }),
                )
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.activate(tab_id, cx)))
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(5.0))
                .px(px(6.0))
                .py(px(2.0))
                .rounded(px(3.0))
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
                // What runs in it: one glyph in a small tile, in the text's
                // colour whatever the program's own colours are.
                .child(program_tile(program, theme, active))
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
                        // Capped, so one long session name cannot push every
                        // other tab off the strip.
                        .max_w(px(TAB_LABEL_MAX))
                        .truncate()
                        .text_color(hsla(if active { theme.text } else { theme.text_muted }))
                        .child(label),
                );

            // After the name and before the connection's state: what the
            // program in it is doing, as a dot in the otter's colours, or a
            // turning arc while it works. The otter itself is in the sidebar.
            if let Some(mark) = status.and_then(|pose| {
                status_mark(
                    ("status", tab_id as usize),
                    pose,
                    theme,
                    self.arc_position(),
                )
            }) {
                row = row.child(mark);
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

            if let Some(full) = full {
                row = row.tooltip(move |_, cx| {
                    cx.new(|_| Note {
                        text: full.clone(),
                        theme,
                    })
                    .into()
                });
            }

            if shows_state(local, state) {
                row = row.child(
                    div()
                        .flex_shrink_0()
                        .w(px(6.0))
                        .h(px(6.0))
                        .rounded(px(3.0))
                        .bg(hsla(theme.state_colour(state))),
                );
            }

            if active {
                row = row.bg(hsla(theme.background)).child(
                    div()
                        .id(("detach", tab_id as usize))
                        .cursor_pointer()
                        .px(px(2.0))
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
                            .px(px(2.0))
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

    /// The right-click menu, over everything, at the pointer.
    ///
    /// A click anywhere outside it closes it without doing anything, as every
    /// context menu does.
    fn menu_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let Some((_, at)) = &self.menu else {
            return div();
        };
        let mut list = div()
            .absolute()
            .left(at.x)
            .top(at.y)
            .min_w(px(160.0))
            .flex()
            .flex_col()
            .py(px(4.0))
            .rounded(px(4.0))
            .bg(hsla(theme.surface))
            .border_1()
            .border_color(hsla(theme.border))
            .text_xs()
            // Clicks inside the menu are the menu's, not the backdrop's.
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation());
        for (index, action) in self.menu_actions().into_iter().enumerate() {
            let danger = action == crate::menu::Action::EndSession;
            list = list.child(
                div()
                    .id(("menu-item", index))
                    .cursor_pointer()
                    .px(px(12.0))
                    .py(px(4.0))
                    .text_color(hsla(if danger { theme.warning } else { theme.text }))
                    .hover(|row| row.bg(hsla(theme.border)))
                    .on_click(cx.listener(move |this, _, _, cx| this.run_menu(action, cx)))
                    .child(menu_label(action)),
            );
        }
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close_menu(cx)),
            )
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, _, _, cx| this.close_menu(cx)),
            )
            .child(list)
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
            Some(Modal::Rename { pane, field }) => {
                let local = self.pane(*pane).is_some_and(|p| p.is_local());
                body = body.child(field_box(field, theme)).child(
                    div()
                        .text_xs()
                        .text_color(hsla(theme.text_muted))
                        .child(if local {
                            strings::RENAME_BODY_LOCAL
                        } else {
                            strings::RENAME_BODY
                        }),
                );
                (
                    strings::RENAME_TITLE,
                    strings::RENAME_ACCEPT,
                    strings::RENAME_REJECT,
                    false,
                )
            }
            Some(Modal::RenameHost { alias, field }) => {
                body = body.child(field_box(field, theme)).child(
                    div()
                        .text_xs()
                        .text_color(hsla(theme.text_muted))
                        .child(strings::rename_host_body(alias)),
                );
                (
                    strings::RENAME_HOST_TITLE,
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
                let field = match this.modal.as_mut() {
                    Some(Modal::Rename { field, .. } | Modal::RenameHost { field, .. }) => {
                        Some(field)
                    }
                    _ => None,
                };
                if let Some(field) = field {
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
                    .flex_col()
                    .flex_1()
                    .overflow_hidden()
                    .border_1()
                    .border_color(hsla(if focused { theme.accent } else { theme.border }))
                    // In a split, each pane says what runs in it: the tab
                    // can only speak for one of them.
                    .when(
                        self.tab_of(pane_id)
                            .and_then(|index| self.tabs.get(index))
                            .is_some_and(|tab| tab.root.panes().len() > 1),
                        |this| {
                            let program = crate::programs::lookup(
                                self.pane_program(pane).unwrap_or_default(),
                            );
                            let title = self
                                .pane_title(pane)
                                .unwrap_or_else(|| pane.alias().to_string());
                            this.child(
                                div()
                                    .flex()
                                    .flex_shrink_0()
                                    .items_center()
                                    .gap(px(5.0))
                                    .px(px(6.0))
                                    .py(px(2.0))
                                    .text_xs()
                                    .bg(hsla(theme.surface))
                                    .border_b(px(1.0))
                                    .border_color(hsla(theme.border))
                                    .text_color(hsla(if focused {
                                        theme.text
                                    } else {
                                        theme.text_muted
                                    }))
                                    .child(
                                        svg()
                                            .path(program.icon)
                                            .flex_shrink_0()
                                            .size(px(PANE_ICON))
                                            .text_color(hsla(theme.text_muted)),
                                    )
                                    .child(div().truncate().child(title)),
                            )
                        },
                    )
                    // Never a silent fallback: a local tab running its shell
                    // outside the local server says so, and why (D50).
                    .when_some(pane.direct.clone(), |this, reason| {
                        this.child(
                            div()
                                .px(px(8.0))
                                .py(px(3.0))
                                .text_xs()
                                .bg(hsla(theme.border))
                                .text_color(hsla(theme.text_muted))
                                .child(strings::local_direct_notice(&reason)),
                        )
                    })
                    // Input the program did not take is said, not swallowed
                    // (D50).
                    .when(pane.view.read(cx).intake.input_dropped_recently(), |this| {
                        this.child(
                            div()
                                .px(px(8.0))
                                .py(px(3.0))
                                .text_xs()
                                .bg(hsla(theme.border))
                                .text_color(hsla(theme.warning))
                                .child(strings::INPUT_NOT_DELIVERED),
                        )
                    })
                    .child(div().flex().flex_1().min_h_0().child(pane.view.clone()))
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
        self.tend_otters(cx);

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
                if this.menu.is_some() && event.keystroke.key == "escape" {
                    this.close_menu(cx);
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

        if self.menu.is_some() {
            root = root.child(self.menu_overlay(cx));
        }
        if self.modal.is_some() {
            root = root.child(self.modal(cx));
        }
        root
    }
}

/// A one-line text field, drawn as its text with a cursor between the halves.
fn field_box(field: &TextField, theme: Theme) -> gpui::Div {
    let (before, after) = field.split();
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
        // The cursor is a block between the halves rather than a character in
        // the string, so it can never be saved as part of the name.
        .child(div().w(px(2.0)).h(px(16.0)).bg(hsla(theme.accent)))
        .child(after.to_string())
}

/// What a menu item says.
fn menu_label(action: crate::menu::Action) -> &'static str {
    use crate::menu::Action;
    match action {
        Action::Rename => strings::MENU_RENAME,
        Action::Detach => strings::MENU_DETACH,
        Action::Close => strings::MENU_CLOSE,
        Action::EndSession => strings::MENU_END_SESSION,
        Action::Open => strings::MENU_OPEN,
        Action::NewTerminal => strings::MENU_NEW_TERMINAL,
        Action::Connect => strings::MENU_CONNECT,
        Action::RenameHost => strings::MENU_RENAME_HOST,
    }
}

/// The widest a tab's name gets before it is cut with an ellipsis.
const TAB_LABEL_MAX: f32 = 160.0;
/// Roughly where that cut falls in the UI's small text, so only a name that
/// was actually cut gets a tooltip.
const TAB_LABEL_CHARS: usize = 22;

/// How far a session sits in from its folder: past the caret and the dot.
const INDENT: f32 = 36.0;

/// What a local shell is called in the tree: the shell's own name.
pub(super) fn local_label() -> String {
    let shell = berth_core::local::login_shell();
    let name = shell
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&shell)
        .trim_end_matches(".exe")
        .to_string();
    if name.is_empty() {
        strings::LOCAL_ALIAS.to_string()
    } else {
        name
    }
}

/// The size of a program's icon on a tab, and in a split pane's header.
const TAB_ICON: f32 = 14.0;
const PANE_ICON: f32 = 12.0;

/// A program's icon on a tab: one glyph in a small rounded tile, tinted with
/// the text colour. Never in a brand's own colours.
fn program_tile(program: crate::programs::Program, theme: Theme, active: bool) -> gpui::Div {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .w(px(TAB_ICON + 4.0))
        .h(px(TAB_ICON + 4.0))
        .rounded(px(4.0))
        .bg(hsla(if active { theme.surface } else { theme.border }))
        .child(
            svg()
                .path(program.icon)
                .size(px(TAB_ICON))
                .text_color(hsla(if active { theme.text } else { theme.text_muted })),
        )
}

/// The size of the status mark on a tab.
const STATUS_DOT: f32 = 5.0;
const STATUS_ARC: f32 = 7.0;

/// What a tab's status mark is, for a pose: a dot in the otter's colour for
/// what needs a look, the arc for work under way, nothing for idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Dot(&'static str),
    Arc,
}

fn mark_for(pose: crate::otter::Pose) -> Option<Mark> {
    use crate::otter::Pose;
    match pose {
        Pose::NeedsYou => Some(Mark::Dot("need")),
        Pose::Done => Some(Mark::Dot("done")),
        Pose::Error => Some(Mark::Dot("error")),
        Pose::Working => Some(Mark::Arc),
        Pose::Idle => None,
    }
}

/// A tab's status mark (D53): amber for needs you, green for done, red for
/// an error, each a 5 px dot in the otter's colours, and a thin arc that
/// turns while the program works. `position` is where the arc is in its
/// turn, from the timer every icon shares.
fn status_mark(
    id: (&'static str, usize),
    pose: crate::otter::Pose,
    theme: Theme,
    position: usize,
) -> Option<gpui::Stateful<gpui::Div>> {
    let text = match pose {
        crate::otter::Pose::NeedsYou => strings::STATUS_NEEDS_YOU,
        crate::otter::Pose::Done => strings::STATUS_DONE,
        crate::otter::Pose::Error => strings::STATUS_ERROR,
        crate::otter::Pose::Working => strings::STATUS_WORKING,
        crate::otter::Pose::Idle => return None,
    };
    let mark = match mark_for(pose)? {
        Mark::Dot(token) => {
            let colour = crate::otter::colour(token, theme.appearance).unwrap_or(theme.warning);
            div()
                .flex_shrink_0()
                .w(px(STATUS_DOT))
                .h(px(STATUS_DOT))
                .rounded(px(STATUS_DOT / 2.0))
                .bg(hsla(colour))
        }
        Mark::Arc => div()
            .flex_shrink_0()
            .w(px(STATUS_ARC))
            .h(px(STATUS_ARC))
            .child(
                gpui::canvas(
                    |_, _, _| {},
                    move |bounds, (), window, _| {
                        // Three quarters of a circle, starting an eighth of a
                        // turn further on for each position.
                        let centre = bounds.center();
                        let radius = f32::from(bounds.size.width) / 2.0 - 0.5;
                        let at = |turn: f32| {
                            let angle = turn * std::f32::consts::TAU;
                            gpui::point(
                                centre.x + px(radius * angle.cos()),
                                centre.y + px(radius * angle.sin()),
                            )
                        };
                        let start = position as f32 / crate::otter::ARC_POSITIONS as f32;
                        let mut path = gpui::PathBuilder::stroke(px(1.0));
                        path.move_to(at(start));
                        path.arc_to(
                            gpui::point(px(radius), px(radius)),
                            px(0.0),
                            true,
                            true,
                            at(start + 0.75),
                        );
                        if let Ok(path) = path.build() {
                            window.paint_path(path, theme.text_muted);
                        }
                    },
                )
                .size_full(),
            ),
    };
    Some(
        div()
            .id(id)
            .flex_shrink_0()
            .child(mark)
            .tooltip(move |_, cx| {
                cx.new(|_| Note {
                    text: text.into(),
                    theme,
                })
                .into()
            }),
    )
}

/// The status icon (D53): the otter in its pose with its stone, and the
/// tooltip that says in words what it shows. Should the sprite sheet ever
/// fail to load, the ghost it replaced stands in, coloured by what the pose
/// asks of you.
fn otter(
    id: (&'static str, usize),
    theme: Theme,
    look: &OtterLook,
    frame: usize,
) -> gpui::Stateful<gpui::Div> {
    let text = look.text.clone();
    let icon = match crate::otter::icon(look.pose, frame, look.stone, theme) {
        Some(icon) => icon,
        None => {
            let colour = match pose_tone(look.pose) {
                Tone::Working => theme.accent,
                Tone::NeedsYou => theme.warning,
                Tone::Done => theme.text_muted,
            };
            div().child(
                svg()
                    .path(crate::icons::GHOST)
                    .size(px(12.0))
                    .text_color(hsla(colour)),
            )
        }
    };
    div()
        .id(id)
        .flex_shrink_0()
        .child(icon)
        .tooltip(move |_, cx| {
            cx.new(|_| Note {
                text: text.clone().into(),
                theme,
            })
            .into()
        })
}

/// What a pose asks of you, for the fallback's colour.
fn pose_tone(pose: crate::otter::Pose) -> Tone {
    use crate::otter::Pose;
    match pose {
        Pose::Working => Tone::Working,
        Pose::NeedsYou | Pose::Done | Pose::Error => Tone::NeedsYou,
        Pose::Idle => Tone::Done,
    }
}

/// The longest message shown in a tooltip; a record's may run to 2 KB.
const STATUS_MSG_SHOWN: usize = 160;

/// A record in words: who (its title, else the program's name), what it
/// reported, and its message, cut to a line.
pub(super) fn status_text(status: &berth_proto::v1::Status) -> String {
    let who = status
        .title
        .as_deref()
        .or(status.app.as_deref())
        .unwrap_or(strings::STATUS_ANONYMOUS);
    let msg = status.msg.as_deref().map(|msg| {
        if msg.chars().count() > STATUS_MSG_SHOWN {
            let cut: String = msg.chars().take(STATUS_MSG_SHOWN).collect();
            format!("{cut}\u{2026}")
        } else {
            msg.to_string()
        }
    });
    strings::status_tooltip(
        who,
        status.state,
        status.kind,
        status.progress,
        msg.as_deref(),
    )
}

/// What Claude's hooks said, in words (D48).
pub(super) fn claude_text(state: berth_core::claude::State) -> &'static str {
    claude_look(state).1
}

/// How a Claude state reads at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Working,
    /// The one worth noticing from across the screen.
    NeedsYou,
    Done,
}

fn claude_look(state: berth_core::claude::State) -> (Tone, &'static str) {
    use berth_core::claude::{State, Wait};
    match state {
        State::Working => (Tone::Working, strings::CLAUDE_WORKING),
        State::Waiting(Wait::Permission) => (Tone::NeedsYou, strings::CLAUDE_PERMISSION),
        State::Waiting(Wait::Idle) => (Tone::NeedsYou, strings::CLAUDE_IDLE),
        State::Waiting(Wait::YourMove) => (Tone::NeedsYou, strings::CLAUDE_YOUR_MOVE),
        State::Finished => (Tone::Done, strings::CLAUDE_FINISHED),
        State::Interrupted => (Tone::Done, strings::CLAUDE_INTERRUPTED),
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

#[cfg(test)]
mod tests {
    use super::*;
    use berth_core::claude::{State, Wait};

    /// Every kind of waiting is the colour that asks for you; nothing else is.
    #[test]
    fn only_waiting_needs_you() {
        for state in [
            State::Waiting(Wait::Permission),
            State::Waiting(Wait::Idle),
            State::Waiting(Wait::YourMove),
        ] {
            assert_eq!(claude_look(state).0, Tone::NeedsYou, "{state:?}");
        }
        assert_eq!(claude_look(State::Working).0, Tone::Working);
        assert_eq!(claude_look(State::Finished).0, Tone::Done);
        assert_eq!(claude_look(State::Interrupted).0, Tone::Done);
    }

    fn status(state: berth_proto::v1::StatusState) -> berth_proto::v1::Status {
        berth_proto::v1::Status {
            id: String::new(),
            state,
            kind: None,
            progress: None,
            app: None,
            title: None,
            msg: None,
        }
    }

    /// The tab's mark: amber for needs you, green for done, red for an
    /// error, the arc for working, and nothing for idle.
    #[test]
    fn a_tabs_mark_follows_the_pose() {
        use crate::otter::Pose;
        assert_eq!(mark_for(Pose::NeedsYou), Some(Mark::Dot("need")));
        assert_eq!(mark_for(Pose::Done), Some(Mark::Dot("done")));
        assert_eq!(mark_for(Pose::Error), Some(Mark::Dot("error")));
        assert_eq!(mark_for(Pose::Working), Some(Mark::Arc));
        assert_eq!(mark_for(Pose::Idle), None);
        // Every dot's colour is one the otter's palette has, in both themes.
        for pose in [Pose::NeedsYou, Pose::Done, Pose::Error] {
            let Some(Mark::Dot(token)) = mark_for(pose) else {
                panic!("{pose:?}");
            };
            for appearance in [
                crate::theme::Appearance::Light,
                crate::theme::Appearance::Dark,
            ] {
                assert!(crate::otter::colour(token, appearance).is_some(), "{token}");
            }
        }
    }

    /// A pose asks for a look when the program is stuck, failed or has
    /// something to show; working and idle do not.
    #[test]
    fn poses_that_need_you() {
        use crate::otter::Pose;
        for pose in [Pose::NeedsYou, Pose::Error, Pose::Done] {
            assert_eq!(pose_tone(pose), Tone::NeedsYou, "{pose:?}");
        }
        assert_eq!(pose_tone(Pose::Working), Tone::Working);
        assert_eq!(pose_tone(Pose::Idle), Tone::Done);
    }

    /// The tooltip gives the program's own words, as a report, named by its
    /// title before its app, and never more than a line of its message.
    #[test]
    fn a_status_reads_as_what_the_program_reported() {
        use berth_proto::v1::{StatusKind, StatusState};
        let mut record = status(StatusState::Working);
        assert_eq!(status_text(&record), "A program reports it is working.");
        record.app = Some("make".into());
        record.progress = Some(40);
        record.msg = Some("Building tests".into());
        assert_eq!(
            status_text(&record),
            "make reports it is working (40%). Building tests"
        );
        record.title = Some("Tests".into());
        record.state = StatusState::Blocked;
        record.kind = Some(StatusKind::Permission);
        record.msg = None;
        assert_eq!(
            status_text(&record),
            "Tests reports it needs your permission."
        );

        record.msg = Some("x".repeat(500));
        let text = status_text(&record);
        assert!(text.ends_with('\u{2026}'), "{text}");
        assert!(text.chars().count() < 220, "{}", text.chars().count());
    }
}
