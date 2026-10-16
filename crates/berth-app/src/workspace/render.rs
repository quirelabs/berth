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
        let needs_you = |folder: &crate::sidebar::Folder, name: &str, pane: Option<u64>| {
            self.row_look(folder, name, pane)
                .is_some_and(|(_, look)| look.state == crate::glyph::State::NeedsYou)
        };
        let rows = crate::sidebar::rows(&Input {
            hosts: &hosts,
            open: &open,
            running: &self.running,
            expanded: &self.expanded,
            local_label: &shell,
            local_detached: &detached,
            needs_you: &needs_you,
        });

        let mut list = div().flex().flex_col().gap(px(1.0));
        for (index, row) in rows.into_iter().enumerate() {
            list = list.child(match row {
                Row::NeedsYou => div()
                    .px(px(10.0))
                    .pt(px(8.0))
                    .pb(px(2.0))
                    .text_xs()
                    .text_color(hsla(theme.gold))
                    .child(strings::SIDEBAR_NEEDS_YOU)
                    .into_any_element(),
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
                    pinned,
                    ..
                } => self.session_row(index, folder, name, pane, pinned, cx),
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
            .font_family(self.chrome_font())
            .bg(hsla(theme.chrome))
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
                hsla(theme.text_muted),
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
        let _ = agent;
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
            .child(div().truncate().text_color(hsla(theme.text)).child(name));
        // How far away the host is, on the header's right.
        let latency = self
            .conns
            .get(&alias)
            .and_then(|c| c.rtt)
            .map(|rtt| format!("{} ms", rtt.as_millis()));
        if let Some(latency) = latency {
            row = row.child(
                div()
                    .ml_auto()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(hsla(theme.text_muted))
                    .child(latency),
            );
        }

        if on_screen {
            row = row.bg(hsla(theme.border));
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
            // A host on another server build says so, with both builds a
            // hover away. Never updated without being asked (D55).
            let update = self.server_updates.get(&alias);
            if update.is_some() {
                row = row.child(
                    div()
                        .text_xs()
                        .text_color(hsla(theme.text_muted))
                        .child(strings::SERVER_UPDATE),
                );
            }
            let note: Option<gpui::SharedString> = match &status {
                HostStatus::Failed(reason) => Some(reason.clone().into()),
                HostStatus::NeedsVerify => Some(strings::VERIFY_TOOLTIP.into()),
                HostStatus::Verifying | HostStatus::Connection(_) => {
                    update.map(|(there, offered)| strings::server_update(there, offered).into())
                }
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
    /// A local server session no tab shows, found again by the name the
    /// tree gave it.
    fn local_session_named(&self, name: &str) -> Option<u64> {
        self.detached_local()
            .into_iter()
            .find(|(_, n)| n == name)
            .map(|(id, _)| id)
    }

    /// What a sidebar row's session shows: its pane's status, or a local
    /// session's no pane shows. A remote session no pane shows has none.
    fn row_look(
        &self,
        folder: &crate::sidebar::Folder,
        name: &str,
        pane: Option<u64>,
    ) -> Option<(StatusKey, StatusLook)> {
        match (pane, folder) {
            (Some(id), _) => Some((StatusKey::Pane(id), self.pane_look(id)?)),
            (None, crate::sidebar::Folder::Local) => {
                let session = self.local_session_named(name)?;
                Some((StatusKey::Session(session), self.session_look(session)?))
            }
            (None, crate::sidebar::Folder::Host(_)) => None,
        }
    }

    /// One session (D53): its program's tile, its name, how long it has been
    /// as it is or how it exited, the number of the tab showing it, and its
    /// glyph, which never moves here. Open ones are switched to, detached
    /// ones are reattached in a tab of their own. A pinned one is in the
    /// needs-you group, a remote one there with its host in front.
    fn session_row(
        &self,
        index: usize,
        folder: crate::sidebar::Folder,
        name: String,
        pane: Option<u64>,
        pinned: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = self.theme;
        let focused = pane.is_some() && self.focused().map(|p| p.id) == pane;
        let in_tab = pane.is_some_and(|id| self.tab().is_some_and(|t| t.root.contains(id)));
        let detached = pane.is_none();
        let local_session = match (&folder, pane) {
            (crate::sidebar::Folder::Local, None) => self.local_session_named(&name),
            (_, Some(id)) => self.pane(id).and_then(|p| p.session),
            _ => None,
        };
        // What runs in it, for its tile.
        let program: Option<String> = match (pane.and_then(|id| self.pane(id)), &folder) {
            (Some(pane), _) => self.pane_program(pane),
            (None, crate::sidebar::Folder::Host(alias)) => self.remote_program(alias, &name),
            (None, crate::sidebar::Folder::Local) => {
                local_session.and_then(|session| self.session_program(session))
            }
        }
        .map(str::to_string);
        let target = match (&folder, pane) {
            (_, Some(pane)) => Some(RowTarget::Pane(pane)),
            (crate::sidebar::Folder::Host(alias), None) => Some(RowTarget::Remote {
                alias: alias.clone(),
                slug: name.clone(),
            }),
            (crate::sidebar::Folder::Local, None) => local_session.map(RowTarget::Local),
        };
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
        let look = self.row_look(&folder, &name, pane);
        let exit = local_session
            .and_then(|session| self.local_sessions.iter().find(|s| s.id == session))
            .and_then(|info| info.exit);
        let meta = strings::row_meta(
            look.as_ref().map(|(_, look)| look.state),
            look.as_ref()
                .map(|(key, _)| self.status_since(*key))
                .unwrap_or_default(),
            exit,
        );
        let tab_number = pane.and_then(|id| self.tab_of(id)).map(|at| at + 1);
        let host = match (&folder, pinned) {
            (crate::sidebar::Folder::Host(alias), true) => Some(self.host_label(alias)),
            _ => None,
        };
        let tile = program_tile(
            crate::programs::lookup(
                program
                    .as_deref()
                    .or(look.as_ref().and_then(|(_, l)| l.program.as_deref()))
                    .unwrap_or_default(),
            ),
            theme,
        );

        let spoken = strings::status_label(
            &name,
            match &folder {
                crate::sidebar::Folder::Host(alias) => Some(self.host_label(alias)),
                crate::sidebar::Folder::Local => None,
            }
            .as_deref(),
            program
                .as_deref()
                .or(look.as_ref().and_then(|(_, l)| l.program.as_deref()))
                .map(crate::programs::display_name)
                .as_deref(),
            look.as_ref()
                .map_or(crate::glyph::State::Idle, |(_, l)| l.state),
            look.as_ref().and_then(|(_, l)| l.msg.as_deref()),
            tab_number,
        );
        let mut row = div()
            .id(("session", index))
            .role(gpui::Role::ListItem)
            .aria_label(spoken)
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    if let Some(target) = menu_target.clone() {
                        this.open_menu(target, event.position, cx)
                    }
                }),
            )
            .cursor_pointer()
            // A click shows the session; ⌥-click splits it into this tab; a
            // drag onto a tab or a pane splits it in there.
            .when_some(target.clone(), |row, target| {
                let clicked = target.clone();
                row.on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                    if event.modifiers().alt {
                        this.split_in(clicked.clone(), None, cx);
                    } else {
                        this.click_row(clicked.clone(), cx);
                    }
                }))
                .on_drag(
                    RowDrag {
                        target,
                        name: name.clone().into(),
                        theme,
                    },
                    |drag, _, _, cx| cx.new(|_| drag.clone()),
                )
            })
            .hover(|row| row.bg(hsla(theme.surface)))
            .flex()
            .items_center()
            .gap(px(8.0))
            .mx(px(6.0))
            .pl(px(if pinned { 6.0 } else { 18.0 }))
            .pr(px(8.0))
            .py(px(3.0))
            .rounded(px(5.0))
            // Rows in the tab on screen are lit a little, the focused one
            // more.
            .when(in_tab && !focused, |row| row.bg(hsla(theme.surface)))
            .when(focused, |row| row.bg(hsla(theme.border)))
            .child(tile)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .when_some(host, |names, host| {
                        names.child(
                            div()
                                .flex_shrink_0()
                                .text_color(hsla(theme.text_muted))
                                .child(format!("{host}:")),
                        )
                    })
                    // Detached is dimmer: it is running, but nothing here
                    // shows it.
                    .child(
                        div()
                            .truncate()
                            .text_color(hsla(if detached {
                                theme.text_muted
                            } else {
                                theme.text
                            }))
                            .child(name),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(hsla(if exit.is_some_and(|code| code != 0) {
                        theme.rose_text
                    } else {
                        theme.text_muted
                    }))
                    .child(meta),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(12.0))
                    .text_xs()
                    .text_right()
                    .text_color(hsla(theme.text_muted))
                    .child(tab_number.map(|n| n.to_string()).unwrap_or_default()),
            );
        // The glyph, in a column of its own at the end; never moving, so a
        // visible sidebar costs no wakeups.
        let glyph = match &look {
            Some((key, look)) => self.glyph_of(*key, look, true),
            None => crate::glyph::glyph(
                crate::glyph::State::Idle,
                self.glyph_colours(program.as_deref()),
                crate::glyph::Moment {
                    clock: Duration::ZERO,
                    since: Duration::ZERO,
                    steady: true,
                    still: true,
                },
            ),
        };
        row = row.child(
            div()
                .flex_shrink_0()
                .w(px(crate::glyph::SIDEBAR.0))
                .child(crate::glyph::render(glyph, crate::glyph::SIDEBAR)),
        );
        let mut words = match &look {
            Some((key, look)) => format!(
                "{}\n{}",
                look.text,
                strings::status_since(self.status_since(*key))
            ),
            None => String::new(),
        };
        if detached {
            if !words.is_empty() {
                words.push('\n');
            }
            words.push_str(strings::SESSION_DETACHED_TOOLTIP);
        }
        if !words.is_empty() {
            row = row.tooltip(move |_, cx| {
                cx.new(|_| Note {
                    text: words.clone().into(),
                    theme,
                })
                .into()
            });
        }
        row.into_any_element()
    }

    /// The tab strip (D53, "cursor"): each tab is its number, its glyph or
    /// pane map, and its title, in the terminal's font. The selected one is
    /// a pill of the terminal's background on the chrome; the others are
    /// the same shape unfilled. A prod host keeps its warning bar and a
    /// connection in trouble its dot; nothing else is on a tab. A tab can be
    /// dragged along the strip, and resting on one opens its panel.
    fn tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let font = self.chrome_font();
        // One row that scrolls sideways rather than squeezing: a tab too narrow
        // to read is worse than one you scroll to.
        let mut strip = div()
            .id("tabs")
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(TAB_GAP))
            .h(px(TAB_BAR))
            .px(px(GAP))
            .overflow_x_scroll()
            .font_family(font.clone())
            .bg(hsla(theme.chrome))
            .on_drag_move(
                cx.listener(|this, event: &gpui::DragMoveEvent<TabDrag>, _, cx| {
                    let tab = event.drag(cx).tab;
                    this.drag_tab_to(tab, f32::from(event.event.position.x), cx);
                }),
            )
            .on_drop(cx.listener(|this, drag: &TabDrag, _, cx| this.drop_tab(drag.tab, cx)));

        // Shown order: with a tab being dragged, it is held where it would
        // land, as an empty space its own width.
        let mut order: Vec<usize> = (0..self.tabs.len()).collect();
        if let Some(dragging) = self.tab_dragging
            && dragging.from < order.len()
        {
            let held = order.remove(dragging.from);
            order.insert(dragging.to.min(order.len()), held);
        }
        let still = self.glyph_still;
        for index in order {
            let Some(tab) = self.tabs.get(index) else {
                continue;
            };
            // A tab is named by its focused pane, so a split tab still says
            // which host you are looking at rather than which one it started
            // as.
            let Some(pane) = self.pane(tab.focus) else {
                continue;
            };
            let id = pane.id;
            let tab_id = tab.id;
            let focus = tab.focus;
            let number = index + 1;
            let active = index == self.active;
            let others = tab.root.panes().len().saturating_sub(1);
            let local = pane.is_local();
            let state = self.state_of(id);
            let host = (!local).then(|| self.host_label(pane.alias()));
            let title = self.tab_title(pane);
            // The title turns gold when any pane in the tab needs the user,
            // light rose on an error in the focused one.
            let states: Vec<crate::glyph::State> = tab
                .root
                .panes()
                .iter()
                .filter_map(|pane| Some(self.pane_look(*pane)?.state))
                .collect();
            let tone = if states.contains(&crate::glyph::State::NeedsYou) {
                Some(theme.gold)
            } else if self.pane_look(id).map(|look| look.state) == Some(crate::glyph::State::Error)
            {
                Some(theme.rose_text)
            } else {
                None
            };
            // The focused pane's glyph, or a map of every pane's.
            let glyph = if others > 0 {
                crate::glyph::render_map(crate::glyph::pane_map(&tab.root, |pane| {
                    self.pane_glyph(pane)
                }))
            } else {
                let cell = pane
                    .view
                    .read(cx)
                    .resolved_cell
                    .map(|c| (c.width, c.height));
                crate::glyph::render(
                    self.pane_glyph(id),
                    crate::glyph::tab_size(cell, self.settings.font_size),
                )
            };
            let held = self.tab_dragging.is_some_and(|d| d.tab == tab_id);
            let offset = self.tab_slides.get(&tab_id).map_or(0.0, |(start, at)| {
                crate::tabs::slide(*start, at.elapsed(), still)
            });
            let dragged_label: gpui::SharedString = match &host {
                Some(host) => format!("{number} {host}:{title}").into(),
                None => format!("{number} {title}").into(),
            };

            let look = self.pane_look(id);
            let spoken = strings::status_label(
                &title,
                host.as_deref(),
                self.pane_program(pane)
                    .or(look.as_ref().and_then(|l| l.program.as_deref()))
                    .map(crate::programs::display_name)
                    .as_deref(),
                look.as_ref().map_or(crate::glyph::State::Idle, |l| l.state),
                look.as_ref().and_then(|l| l.msg.as_deref()),
                Some(number),
            );
            let bounds = self.tab_bounds.clone();
            let mut row = div()
                .id(("tab", tab_id as usize))
                .role(gpui::Role::Tab)
                .aria_label(spoken)
                .on_mouse_down(
                    gpui::MouseButton::Right,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                        this.open_menu(crate::menu::Target::Pane(id), event.position, cx)
                    }),
                )
                .cursor_pointer()
                .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                    // Renaming is a double click, as it is on a tab everywhere
                    // else. A single click switches, and that must stay the
                    // cheap one.
                    if event.click_count() >= 2 {
                        this.ask_rename(id, cx);
                    } else {
                        this.tab_panel = None;
                        this.activate(tab_id, cx);
                    }
                }))
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    this.hover_tab(tab_id, *hovered, cx)
                }))
                .on_drag(
                    TabDrag {
                        tab: tab_id,
                        label: dragged_label,
                        theme,
                        font: font.clone(),
                    },
                    |drag, _, _, cx| cx.new(|_| drag.clone()),
                )
                // A sidebar row dropped here splits into this tab.
                .drag_over::<RowDrag>(move |style, _, _, _| style.bg(hsla(theme.border)))
                .on_drop(cx.listener(move |this, drag: &RowDrag, _, cx| {
                    this.split_in(drag.target.clone(), Some(focus), cx)
                }))
                .relative()
                .left(px(offset))
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(6.0))
                .h(px(TAB_BAR - 2.0 * TAB_INSET))
                .px(px(10.0))
                .rounded(px(TAB_RADIUS))
                .text_xs()
                .when(held, |row| row.opacity(0.0))
                .when(active, |row| row.bg(hsla(theme.background)))
                .when(!active, |row| row.hover(|t| t.bg(hsla(theme.surface))))
                // Where it was drawn, for a drag's landing and the panel.
                .child(
                    gpui::canvas(
                        move |at, _, _| {
                            bounds.borrow_mut().insert(tab_id, at);
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                // Its number, as ⌘N reaches it, muted like tmux's.
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(hsla(theme.text_muted))
                        .child(number.to_string()),
                )
                // Production says so on its tabs, the one colour that is not
                // a program's status.
                .when(pane.is_prod(), |row| {
                    row.child(
                        div()
                            .flex_shrink_0()
                            .w(px(3.0))
                            .h(px(12.0))
                            .rounded(px(2.0))
                            .bg(hsla(theme.warning)),
                    )
                })
                .child(glyph)
                .child(
                    div()
                        .flex()
                        // Capped, so one long name cannot push every other
                        // tab off the strip.
                        .max_w(px(TAB_LABEL_MAX))
                        .overflow_hidden()
                        .when_some(host, |label, host| {
                            label.child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(hsla(theme.text_muted))
                                    .child(format!("{host}:")),
                            )
                        })
                        .child(
                            div()
                                .truncate()
                                .text_color(hsla(tone.unwrap_or(if active {
                                    theme.text
                                } else {
                                    theme.text_muted
                                })))
                                .child(title),
                        ),
                );
            // The other panes of a split, counted.
            if others > 0 {
                row = row.child(
                    div()
                        .flex_shrink_0()
                        .text_color(hsla(theme.text_muted))
                        .child(format!("+{others}")),
                );
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
            strip = strip.child(row);
        }
        strip
    }

    /// What a tab is called: the focused pane's title as its program gives
    /// it, else its session's name.
    fn tab_title(&self, pane: &Pane) -> String {
        self.pane_title(pane).unwrap_or_else(|| {
            if pane.is_local() {
                pane.alias().to_string()
            } else {
                pane.slug.clone()
            }
        })
    }

    /// A tab's hover panel (D53): flat, in the terminal's colours and font,
    /// under its tab. A row per pane, chosen with the arrows and Enter or a
    /// click.
    fn tab_panel(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let theme = self.theme;
        let panel = self.tab_panel?;
        let index = self.tabs.iter().position(|t| t.id == panel.tab)?;
        let tab = self.tabs.get(index)?;
        let at = self.tab_bounds.borrow().get(&panel.tab).copied()?;
        let panes = tab.root.panes();
        let mut list = div()
            .id("tab-panel")
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.over_panel = *hovered;
                if !*hovered && this.tab_hover.is_none() {
                    this.close_panel_soon(cx);
                }
            }))
            .flex()
            .flex_col()
            .min_w(px(280.0))
            .max_w(px(480.0))
            .p(px(6.0))
            .gap(px(2.0))
            .rounded(px(TAB_RADIUS))
            .bg(hsla(theme.background))
            .font_family(self.chrome_font())
            .text_xs()
            .text_color(hsla(theme.text))
            .child(
                div()
                    .px(px(6.0))
                    .pb(px(4.0))
                    .text_color(hsla(theme.text_muted))
                    .child(strings::tab_panel_header(index + 1, panes.len())),
            );
        for (row_index, pane_id) in panes.iter().enumerate() {
            let Some(pane) = self.pane(*pane_id) else {
                continue;
            };
            let pane_id = *pane_id;
            let look = self.pane_look(pane_id);
            let program = self
                .pane_program(pane)
                .or(look.as_ref().and_then(|l| l.program.as_deref()))
                .map(crate::programs::display_name);
            let who = match (&program, pane.is_local()) {
                (Some(program), true) => program.clone(),
                (Some(program), false) => format!("{program} · {}", self.host_label(pane.alias())),
                (None, true) => strings::LOCAL_ALIAS.to_string(),
                (None, false) => self.host_label(pane.alias()),
            };
            let directory = pane
                .session
                .and_then(|session| self.local_sessions.iter().find(|i| i.id == session))
                .and_then(|info| info.cwd.as_deref())
                .map(|cwd| short_directory(cwd, dirs_home().as_deref()));
            let what = [look.as_ref().and_then(|l| l.msg.clone()), directory]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            let since = look
                .as_ref()
                .map(|_| strings::status_since(self.status_since(StatusKey::Pane(pane_id))))
                .unwrap_or_default();
            let chosen = row_index == panel.selected;
            list = list.child(
                div()
                    .id(("tab-panel-row", pane_id as usize))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab_panel = None;
                        this.focus_pane(pane_id, cx);
                    }))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(6.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .when(chosen, |row| row.bg(hsla(theme.surface)))
                    .hover(|row| row.bg(hsla(theme.surface)))
                    .child(crate::glyph::render(
                        self.pane_glyph(pane_id),
                        crate::glyph::SIDEBAR,
                    ))
                    .child(div().flex_shrink_0().child(who))
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_color(hsla(theme.text_muted))
                            .child(what),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(hsla(theme.text_muted))
                            .child(since),
                    ),
            );
        }
        Some(
            gpui::deferred(
                gpui::anchored()
                    .position(gpui::point(
                        at.origin.x,
                        at.origin.y + at.size.height + px(4.0),
                    ))
                    .child(list),
            )
            .into_any_element(),
        )
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
            Some(Modal::Server { alias, plan }) => {
                let muted = |text: gpui::SharedString| {
                    div()
                        .text_xs()
                        .text_color(hsla(theme.text_muted))
                        .child(text)
                };
                let fact = |label: &'static str, value: String| {
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(
                            div()
                                .flex_shrink_0()
                                .w(px(56.0))
                                .text_xs()
                                .text_color(hsla(theme.text_muted))
                                .child(label),
                        )
                        .child(div().text_xs().text_color(hsla(theme.text)).child(value))
                };
                body = body
                    .child(
                        div()
                            .text_sm()
                            .text_color(hsla(theme.text_muted))
                            .child(strings::SERVER_WHAT),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .p(px(8.0))
                            .rounded(px(4.0))
                            .bg(hsla(theme.background))
                            .border_1()
                            .border_color(hsla(theme.border))
                            .child(fact(
                                strings::SERVER_HOST_LABEL,
                                strings::server_host(&self.host_label(alias), &plan.os, &plan.arch),
                            ))
                            .child(fact(strings::SERVER_PATH_LABEL, plan.path.clone()))
                            .child(fact(
                                strings::SERVER_SIZE_LABEL,
                                strings::server_size(plan.size),
                            ))
                            .child(fact(
                                strings::SERVER_HASH_LABEL,
                                strings::server_hash(&plan.sha256),
                            )),
                    );
                if plan.hash_tool.is_none() {
                    body = body.child(
                        div()
                            .text_xs()
                            .text_color(hsla(theme.warning))
                            .child(strings::SERVER_NO_HASH_TOOL),
                    );
                }
                if plan.sessions_end_at_logout {
                    body = body.child(
                        div()
                            .text_xs()
                            .text_color(hsla(theme.warning))
                            .child(strings::SERVER_LOGOUT_WARNING),
                    );
                }
                body = body
                    .child(muted(strings::server_removal(&plan.removal()).into()))
                    .child(muted(
                        strings::server_removal_by_hand(&plan.removal_by_hand()).into(),
                    ));
                (
                    strings::SERVER_TITLE,
                    strings::SERVER_ACCEPT,
                    strings::SERVER_REJECT,
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
            // A tab's hover panel takes the arrows, Enter and Escape before
            // the terminal sees them.
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if this.modal.is_none() && this.panel_key(&event.keystroke.key, cx) {
                    cx.stop_propagation();
                }
            }))
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
                                    .on_click(cx.listener(|this, _, _, cx| this.reject(cx)))
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
    fn render_node(
        &self,
        node: &Node,
        focus: u64,
        path: crate::panes::Path,
        corners: &std::collections::HashMap<u64, [bool; 4]>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match node {
            Node::Leaf(id) => {
                let theme = self.theme;
                let focused = *id == focus;
                let Some(pane) = self.pane(*id) else {
                    return div().flex().flex_1().into_any_element();
                };
                let pane_id = *id;
                let outer = corners.get(id).copied().unwrap_or([true; 4]);
                let split = self
                    .tab_of(pane_id)
                    .and_then(|index| self.tabs.get(index))
                    .is_some_and(|tab| tab.root.panes().len() > 1);
                let dropped = pane.view.read(cx).intake.input_dropped_recently();
                // Rounded where a corner faces the edge of the pane area,
                // square where it meets another pane, all as plain rounded
                // fills (D53). Whatever sits on top of the terminal takes
                // the top corners.
                let topped = split || pane.direct.is_some() || dropped;
                pane.view.update(cx, |view, _| {
                    view.corners = gpui::Corners {
                        top_left: px(corner(outer[0] && !topped)),
                        top_right: px(corner(outer[1] && !topped)),
                        // The pane's status line takes the bottom corners.
                        bottom_right: px(0.0),
                        bottom_left: px(0.0),
                    }
                });
                let first_bar = |bar: gpui::Div, first: bool| {
                    bar.rounded_tl(px(corner(outer[0] && first)))
                        .rounded_tr(px(corner(outer[1] && first)))
                };
                let mut body = div()
                    .id(("pane", pane_id as usize))
                    // Clicking a pane focuses it, which is the only thing that
                    // makes several terminals on screen usable with a mouse.
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.focus_pane(pane_id, cx)),
                    )
                    // A sidebar row dropped on a pane splits in beside it.
                    .on_drop(cx.listener(move |this, drag: &RowDrag, _, cx| {
                        this.split_in(drag.target.clone(), Some(pane_id), cx)
                    }))
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_hidden();
                // In a split, each pane says what runs in it: its glyph, its
                // program and directory, and its message on the right. One
                // that needs the user is tinted gold (D53).
                let look = self.pane_look(pane_id);
                let program = self
                    .pane_program(pane)
                    .or(look.as_ref().and_then(|l| l.program.as_deref()))
                    .map(crate::programs::display_name);
                let directory = pane
                    .session
                    .and_then(|session| self.local_sessions.iter().find(|i| i.id == session))
                    .and_then(|info| info.cwd.as_deref())
                    .map(|cwd| short_directory(cwd, dirs_home().as_deref()));
                let needs_you =
                    look.as_ref().map(|l| l.state) == Some(crate::glyph::State::NeedsYou);
                if split {
                    let glyph = match &look {
                        Some(look) => self.glyph_of(StatusKey::Pane(pane_id), look, true),
                        None => self.pane_glyph(pane_id),
                    };
                    let message = look
                        .as_ref()
                        .and_then(|l| l.msg.clone())
                        .unwrap_or_default();
                    let named = [program.clone(), directory.clone()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" ");
                    body = body.child(first_bar(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap(px(6.0))
                            .px(px(8.0))
                            .py(px(3.0))
                            .text_xs()
                            .font_family(self.chrome_font())
                            .bg(hsla(if needs_you {
                                crate::theme::mix(theme.background, theme.gold, NEEDS_YOU_TINT)
                            } else {
                                theme.background
                            }))
                            .text_color(hsla(if focused {
                                theme.text
                            } else {
                                theme.text_muted
                            }))
                            .child(crate::glyph::render(glyph, crate::glyph::SIDEBAR))
                            .child(div().flex_shrink_0().child(if named.is_empty() {
                                pane.alias().to_string()
                            } else {
                                named
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .truncate()
                                    .text_right()
                                    .text_color(hsla(theme.text_muted))
                                    .child(message),
                            ),
                        true,
                    ));
                }
                // Never a silent fallback: a local tab running its shell
                // outside the local server says so, and why (D50).
                if let Some(reason) = pane.direct.clone() {
                    body = body.child(first_bar(
                        div()
                            .px(px(8.0))
                            .py(px(3.0))
                            .text_xs()
                            .bg(hsla(theme.border))
                            .text_color(hsla(theme.text_muted))
                            .child(strings::local_direct_notice(&reason)),
                        !split,
                    ));
                }
                // Input the program did not take is said, not swallowed
                // (D50).
                if dropped {
                    body = body.child(first_bar(
                        div()
                            .px(px(8.0))
                            .py(px(3.0))
                            .text_xs()
                            .bg(hsla(theme.border))
                            .text_color(hsla(theme.warning))
                            .child(strings::INPUT_NOT_DELIVERED),
                        !split && pane.direct.is_none(),
                    ));
                }
                // Unfocused panes in a split are dimmed, so the one taking
                // keys needs no border to say so; how far is berth.toml's.
                let dim = split && !focused;
                let opacity = self.unfocused_opacity;
                // Each pane's own status line: its state and for how long.
                let line = match &look {
                    Some(look) => format!(
                        "{} {}",
                        strings::state_word(look.state),
                        strings::status_since(self.status_since(StatusKey::Pane(pane_id)))
                    ),
                    None => strings::state_word(crate::glyph::State::Idle).to_string(),
                };
                let tone = match look.as_ref().map(|l| l.state) {
                    Some(crate::glyph::State::NeedsYou) => theme.gold,
                    Some(crate::glyph::State::Error) => theme.rose_text,
                    _ => theme.text_muted,
                };
                body.child(
                    div()
                        .flex()
                        .flex_1()
                        .min_h_0()
                        .when(dim, |view| view.opacity(opacity))
                        .child(pane.view.clone()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .px(px(8.0))
                        .py(px(2.0))
                        .text_xs()
                        .font_family(self.chrome_font())
                        .bg(hsla(theme.background))
                        .rounded_bl(px(corner(outer[3])))
                        .rounded_br(px(corner(outer[2])))
                        .text_color(hsla(tone))
                        .when(dim, |line| line.opacity(opacity))
                        .child(line),
                )
                .into_any_element()
            }
            Node::Split {
                axis,
                share,
                first,
                second,
            } => {
                let theme = self.theme;
                let axis = *axis;
                let tab = self.tab().map(|t| t.id).unwrap_or_default();
                let part = f32::from(*share) / 1000.0;
                let child = |node: &Node, grow: f32, side: bool, cx: &mut Context<Self>| {
                    let mut path = path.clone();
                    path.push(side);
                    div()
                        .flex()
                        .flex_basis(gpui::relative(0.0))
                        .flex_grow(grow)
                        .flex_shrink(1.0)
                        .min_w_0()
                        .min_h_0()
                        .child(self.render_node(node, focus, path, corners, cx))
                };
                // The divider is the gap between the panes; its grip is the
                // gap and a little of each neighbour, drawn over both, with
                // a pill while it is hovered or dragged. The layout follows
                // the pointer every frame; the grid reflows from what it
                // holds and the pty hears of a new size at most once a frame
                // (D53). A double click evens the split out.
                let grip = DividerDrag {
                    tab,
                    path: path.clone(),
                };
                let held = self.dragging_divider.as_ref() == Some(&(tab, path.clone()));
                let group: gpui::SharedString = format!("divider-{tab}-{path:?}").into();
                let pill = div()
                    .bg(hsla(theme.text))
                    .rounded(px(1.0))
                    .opacity(if held { PILL_OPACITY } else { 0.0 })
                    .group_hover(group.clone(), |pill| pill.opacity(PILL_OPACITY));
                let pill = match axis {
                    Axis::Horizontal => pill.w(px(PILL.0)).h(px(PILL.1)),
                    Axis::Vertical => pill.w(px(PILL.1)).h(px(PILL.0)),
                };
                let evened = path.clone();
                let handle = div()
                    .id(gpui::ElementId::Name(group.clone()))
                    .group(group)
                    .absolute()
                    .flex()
                    .justify_center()
                    .items_center()
                    .on_drag(grip, |_, _, _, cx| cx.new(|_| gpui::EmptyView))
                    .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                        if event.click_count() >= 2 {
                            this.drag_divider(tab, &evened, 0.5, cx);
                        }
                    }))
                    .child(pill);
                let handle = match axis {
                    Axis::Horizontal => handle
                        .top_0()
                        .bottom_0()
                        .left(px(-GRIP_INTO))
                        .w(px(GAP + 2.0 * GRIP_INTO))
                        .cursor_ew_resize(),
                    Axis::Vertical => handle
                        .left_0()
                        .right_0()
                        .top(px(-GRIP_INTO))
                        .h(px(GAP + 2.0 * GRIP_INTO))
                        .cursor_ns_resize(),
                };
                // Deferred, so the grip is above the neighbour drawn after it
                // and takes the pointer there too.
                let divider = div()
                    .relative()
                    .flex_shrink_0()
                    .child(gpui::deferred(handle));
                let divider = match axis {
                    Axis::Horizontal => divider.w(px(GAP)).h_full(),
                    Axis::Vertical => divider.h(px(GAP)).w_full(),
                };
                let here = path.clone();
                let row = div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .on_drag_move(cx.listener(
                        move |this, event: &gpui::DragMoveEvent<DividerDrag>, _, cx| {
                            let grip = event.drag(cx);
                            if grip.tab != tab || grip.path != here {
                                return;
                            }
                            let bounds = event.bounds;
                            let at = event.event.position;
                            let share = match axis {
                                Axis::Horizontal => {
                                    (at.x - bounds.origin.x) / bounds.size.width.max(px(1.0))
                                }
                                Axis::Vertical => {
                                    (at.y - bounds.origin.y) / bounds.size.height.max(px(1.0))
                                }
                            };
                            this.drag_divider(tab, &here, share, cx);
                        },
                    ));
                let row = match axis {
                    Axis::Horizontal => row.flex_row(),
                    Axis::Vertical => row.flex_col(),
                };
                let first = child(first, part, false, cx);
                let second = child(second, 1.0 - part, true, cx);
                row.child(first)
                    .child(divider)
                    .child(second)
                    .into_any_element()
            }
        }
    }

    /// The window's status bar (D53), in the terminal's font: how many
    /// sessions are working, need you or have failed, each with its glyph,
    /// on the left, a click going to the next of them; each host's latency
    /// on the right.
    fn status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::glyph::State;
        let theme = self.theme;
        let mut bar = div()
            .h(px(24.0))
            .flex()
            .items_center()
            .gap(px(14.0))
            .px(px(12.0))
            .font_family(self.chrome_font())
            .bg(hsla(theme.chrome))
            .text_xs()
            .text_color(hsla(theme.text_muted));
        for state in [State::Working, State::NeedsYou, State::Error] {
            let count = self.status_targets(state).len();
            let glyph = crate::glyph::glyph(
                state,
                self.glyph_colours(None),
                crate::glyph::Moment {
                    clock: Duration::ZERO,
                    since: Duration::ZERO,
                    steady: true,
                    still: true,
                },
            );
            let tone = match (state, count) {
                (_, 0) => theme.text_muted,
                (State::NeedsYou, _) => theme.gold,
                (State::Error, _) => theme.rose_text,
                _ => theme.text,
            };
            bar = bar.child(
                div()
                    .id(gpui::ElementId::Name(format!("count-{state:?}").into()))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .when(count > 0, |chip| {
                        chip.cursor_pointer().on_click(
                            cx.listener(move |this, _, _, cx| this.cycle_status(state, cx)),
                        )
                    })
                    .role(gpui::Role::Button)
                    .aria_label(strings::status_count(state, count))
                    .child(crate::glyph::render(glyph, crate::glyph::SIDEBAR))
                    .child(div().text_color(hsla(tone)).child(count.to_string())),
            );
        }
        let mut hosts = div().ml_auto().flex().gap(px(14.0));
        for (alias, conn) in &self.conns {
            if alias == strings::LOCAL_ALIAS {
                continue;
            }
            let Some(rtt) = conn.rtt else { continue };
            hosts = hosts.child(
                div()
                    .flex()
                    .gap(px(5.0))
                    .child(self.host_label(alias))
                    .child(
                        div()
                            .text_color(hsla(theme.text))
                            .child(format!("{} ms", rtt.as_millis())),
                    ),
            );
        }
        bar = bar.child(hosts);
        bar
    }
}

impl Workspace {
    /// The terminal's font, for the chrome around it.
    fn chrome_font(&self) -> gpui::SharedString {
        self.chrome_font.clone().unwrap_or_else(|| {
            crate::terminal_style::TerminalStyle::default()
                .font_family
                .into()
        })
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Light and dark follow the OS.
        // The chrome is the terminal's: its colours, and its font, found once
        // as a terminal finds it (D53).
        self.theme = Theme::of_terminal(&crate::terminal_style::TerminalStyle::default().palette);
        if self.chrome_font.is_none() {
            let mut style = crate::terminal_style::TerminalStyle::default();
            style.resolve_family(&window.text_system().all_font_names());
            self.chrome_font = Some(style.font_family.into());
        }
        let theme = self.theme;
        self.tend_glyphs(cx);
        self.rendered_at = Some(std::time::Instant::now());
        if !cx.has_active_drag() {
            self.dragging_divider = None;
            // A tab drag that ended anywhere but the strip changes nothing.
            if self.tab_dragging.take().is_some() {
                self.tab_slides.clear();
            }
        }
        // Tabs sliding aside move every frame until they are in place.
        let still = cx.reduce_motion();
        self.tab_slides
            .retain(|_, (_, at)| !still && at.elapsed() < crate::tabs::SLIDE);
        self.slide_timer = (!self.tab_slides.is_empty()).then(|| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SLIDE_FRAME).await;
                let _ = this.update(cx, |_, cx| cx.notify());
            })
        });

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
            Some((root, focus)) => {
                div()
                    .flex()
                    .flex_1()
                    // The pane area sits on the chrome's colour, inset from
                    // the window's edges, with gaps between panes (D53).
                    .bg(hsla(theme.chrome))
                    .px(px(GAP))
                    .pb(px(GAP))
                    .child({
                        let corners: std::collections::HashMap<u64, [bool; 4]> =
                            root.outer_corners().into_iter().collect();
                        self.render_node(&root, focus, Vec::new(), &corners, cx)
                    })
            }
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
            // A tab's hover panel takes the arrows, Enter and Escape before
            // the terminal sees them.
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if this.modal.is_none() && this.panel_key(&event.keystroke.key, cx) {
                    cx.stop_propagation();
                }
            }))
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
            .child(self.status_bar(cx));

        if let Some(panel) = self.tab_panel(cx) {
            root = root.child(panel);
        }
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
        Action::Server => strings::MENU_SERVER,
    }
}

/// How often a sliding tab is redrawn: every frame at 120 Hz.
const SLIDE_FRAME: Duration = Duration::from_micros(8_333);

/// The gap between panes, and between the pane area and the window's
/// edges (D53).
const GAP: f32 = 6.0;
/// How far a divider's grip reaches into each neighbour, past the gap.
const GRIP_INTO: f32 = 3.0;
/// The pill on a hovered or dragged divider: its length across the gap and
/// along it, and how strongly it shows.
const PILL: (f32, f32) = (2.0, 32.0);
const PILL_OPACITY: f32 = 0.35;
/// A pane corner facing the edge of the pane area.
const PANE_RADIUS: f32 = 8.0;
/// How much gold tints the header of a pane that needs the user.
const NEEDS_YOU_TINT: f32 = 0.18;

/// A program's tile in the sidebar (D53): an agent's mark in white on its
/// brand's colour, dark on a light tile for one without, and anything else
/// a Phosphor glyph on the raised surface.
fn program_tile(program: crate::programs::Program, theme: Theme) -> gpui::Div {
    use crate::programs::Tile;
    let (fill, mark) = match program.tile {
        Tile::Brand(colour) => (gpui::rgb(colour), gpui::rgb(0xffffff)),
        Tile::Light => (gpui::rgb(0xececee), gpui::rgb(0x111114)),
        Tile::Neutral => (theme.surface, theme.text_muted),
    };
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .w(px(TILE))
        .h(px(TILE))
        .rounded(px(TILE_RADIUS))
        .bg(hsla(fill))
        .child(
            gpui::svg()
                .path(program.icon)
                .size(px(TILE_MARK))
                .text_color(hsla(mark)),
        )
}

/// A program tile's size, corner and mark.
const TILE: f32 = 18.0;
const TILE_RADIUS: f32 = 4.0;
const TILE_MARK: f32 = 12.0;

/// A pane corner's radius: rounded on the outside, square where panes meet.
fn corner(outside: bool) -> f32 {
    if outside { PANE_RADIUS } else { 0.0 }
}

/// The widest a tab's name gets before it is cut with an ellipsis.
const TAB_LABEL_MAX: f32 = 200.0;
/// The tab bar's height, how far a tab sits in from its top and bottom, and
/// a tab's corner (D53).
const TAB_BAR: f32 = 34.0;
const TAB_INSET: f32 = 5.0;
const TAB_RADIUS: f32 = 7.0;

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
    use berth_core::claude::{State, Wait};
    match state {
        State::Working => strings::CLAUDE_WORKING,
        State::Waiting(Wait::Permission) => strings::CLAUDE_PERMISSION,
        State::Waiting(Wait::Idle) => strings::CLAUDE_IDLE,
        State::Waiting(Wait::YourMove) => strings::CLAUDE_YOUR_MOVE,
        State::Finished => strings::CLAUDE_FINISHED,
        State::Interrupted => strings::CLAUDE_INTERRUPTED,
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

    /// Claude's hooks and reported states map onto the glyph's, and the
    /// status line says who, what and for how long.
    #[test]
    fn states_map_and_read_as_words() {
        use crate::glyph::State as Glyph;
        use berth_core::claude::{State, Wait};
        assert_eq!(
            Glyph::of_claude(State::Waiting(Wait::Permission)),
            Glyph::NeedsYou
        );
        assert_eq!(
            Glyph::of_claude(State::Waiting(Wait::YourMove)),
            Glyph::Done
        );
        assert_eq!(Glyph::of_claude(State::Finished), Glyph::Idle);
        assert_eq!(
            Glyph::of_status(berth_proto::v1::StatusState::Error),
            Glyph::Error
        );
        assert_eq!(claude_text(State::Working), strings::CLAUDE_WORKING);
        assert_eq!(
            strings::status_count(Glyph::NeedsYou, 2),
            "2 sessions need you"
        );
        assert_eq!(
            strings::status_count(Glyph::NeedsYou, 1),
            "1 session needs you"
        );
        assert_eq!(strings::status_count(Glyph::Error, 3), "3 sessions failed");
    }

    /// A row says how long it has been as it is, "done" when it finished,
    /// and how its last command failed when it did.
    #[test]
    fn a_rows_meta_says_time_done_or_exit() {
        use crate::glyph::State;
        let s = Duration::from_secs;
        assert_eq!(strings::row_meta(Some(State::Working), s(130), None), "2m");
        assert_eq!(
            strings::row_meta(Some(State::Done), s(240), Some(0)),
            "done 4m"
        );
        assert_eq!(
            strings::row_meta(Some(State::Error), s(5), Some(1)),
            "exit 1"
        );
        assert_eq!(strings::row_meta(Some(State::Error), s(5), None), "5s");
        assert_eq!(strings::row_meta(None, s(5), Some(2)), "exit 2");
        assert_eq!(strings::row_meta(None, s(5), Some(0)), "");
        assert_eq!(
            strings::row_meta(Some(State::NeedsYou), s(7200), None),
            "2h"
        );
    }

    /// A label reads "name, host, provider, state, message, tab N", leaving
    /// out what is not known, so a screen reader hears the status without
    /// the colour.
    #[test]
    fn a_label_says_who_where_what_and_which_tab() {
        use crate::glyph::State;
        assert_eq!(
            strings::status_label(
                "api",
                Some("web-1"),
                Some("claude"),
                State::NeedsYou,
                Some("Allow edit?"),
                Some(2)
            ),
            "api, web-1, claude, needs you, Allow edit?, tab 2"
        );
        assert_eq!(
            strings::status_label("zsh", None, None, State::Idle, Some("  "), None),
            "zsh, idle"
        );
    }

    #[test]
    fn a_tab_panel_says_its_tab_panes_and_key() {
        assert_eq!(strings::tab_panel_header(2, 3), "tab 2 · 3 panes · ⌘2");
        assert_eq!(strings::tab_panel_header(1, 1), "tab 1 · 1 pane · ⌘1");
        assert_eq!(strings::tab_panel_header(12, 2), "tab 12 · 2 panes");
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
