//! Tabs kept across a quit or a reboot (D46, D55).
//!
//! Every tab, its split and each pane's last screen are saved every minute,
//! when a tab closes and on quit. On launch they come back as they were. A
//! local pane's shell starts, in its saved directory under its old screen,
//! when its tab is first shown, so a tab never looked at costs no process.
//! A host's panes attach again once the host connects, which it does at
//! launch: its tmux sessions by name, its server's by id, and a server
//! session that has ended since gets a new one in the same pane.

use std::path::PathBuf;

use berth_core::restore::{HostSession, Saved, SavedClaude, SavedHost, SavedPane, SavedTab};

use super::*;

/// How often local sessions are saved while Berth runs. The quit saves too;
/// this is for the quit that never happens, a crash or a power cut.
const SAVE_EVERY: Duration = Duration::from_secs(60);

/// A local pane with nothing running yet: the screen of a session from a
/// previous run, waiting for its tab to be shown.
pub(super) struct Restored {
    /// Where the new shell starts. `None` where the platform would not say,
    /// and then it starts at home.
    pub cwd: Option<PathBuf>,
    /// The Claude conversation to resume in it, if one was running (D48).
    pub claude: Option<SavedClaude>,
    /// The local server's session it showed, to attach to again if it is
    /// still running (D50), and that session's pane token.
    pub session: Option<u64>,
    pub token: Option<String>,
    /// For a host's pane, the session on the host to attach to again.
    pub host: Option<HostSession>,
}

/// Where a pane's directory comes from when saving: already known, or to be
/// asked of its running shell off the UI thread.
enum Dir {
    Known(Option<PathBuf>),
    Ask(u32),
}

impl Workspace {
    /// Put back the local tabs the last run saved, before anything connects.
    pub(super) fn restore_local(&mut self, cx: &mut Context<Self>) {
        let saved = Saved::load();
        if saved.tabs.is_empty() {
            return;
        }
        let installed = cx.text_system().all_font_names();
        let mut tabs = saved.tabs;
        tabs.sort_by_key(|t| t.position);

        for tab in tabs {
            // A host no longer in ssh config takes its tab with it: there
            // is nothing to attach its panes to.
            let unknown = tab.panes.iter().find_map(|p| {
                p.host
                    .as_ref()
                    .filter(|h| !self.hosts.iter().any(|known| known.alias == h.alias))
                    .map(|h| h.alias.clone())
            });
            if let Some(alias) = unknown {
                tracing::info!(%alias, "a restored tab's host is not configured now; not restored");
                continue;
            }
            let ids: Vec<u64> = tab
                .panes
                .into_iter()
                .map(|saved| self.restored_pane(saved, &installed, cx))
                .collect();
            let (Some(root), Some(first)) = (
                crate::restore::node_of(&tab.layout, &ids),
                ids.first().copied(),
            ) else {
                // Checked on load, so this is a bug rather than a bad file.
                // The panes exist; they just have no tab to be in.
                tracing::warn!("a restored layout did not match its panes");
                continue;
            };
            self.next_tab_id += 1;
            let at = tab.position.min(self.tabs.len());
            self.tabs.insert(
                at,
                Tab {
                    id: self.next_tab_id,
                    root,
                    focus: ids.get(tab.focus).copied().unwrap_or(first),
                },
            );
        }
        self.active = 0;
        tracing::info!(tabs = self.tabs.len(), "restored tabs");
        self.wake_active(cx);
    }

    /// Start the shells of restored panes in the tab on screen.
    ///
    /// Called wherever the shown tab can change. Cheap when there is nothing
    /// restored, and a pane only ever starts once because resuming takes its
    /// restored state.
    pub(super) fn wake_active(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let waiting: Vec<u64> = tab
            .root
            .panes()
            .into_iter()
            .filter(|id| self.pane(*id).is_some_and(|p| p.restored.is_some()))
            .collect();
        let mut hosts = std::collections::BTreeSet::new();
        for id in waiting {
            match self.pane(id).and_then(|p| p.host.as_ref()) {
                // A host's pane waits for its host; shown, it is tried again
                // if the host is not on its way.
                Some(host) => {
                    hosts.insert(host.alias.clone());
                }
                None => self.resume(id, cx),
            }
        }
        for alias in hosts {
            self.reconnect_restored(alias, cx);
        }
    }

    /// Whether any pane of a host is a restored one still to attach.
    pub(super) fn has_restored(&self, alias: &str) -> bool {
        self.panes
            .iter()
            .any(|p| p.restored.is_some() && p.alias() == alias && !p.is_local())
    }

    /// Connect a host whose panes came back from the last run, and attach
    /// them (D55). Once at a time per host; a host already connected only
    /// attaches. Nothing opens a tab of its own here.
    pub(super) fn reconnect_restored(&mut self, alias: String, cx: &mut Context<Self>) {
        if self.conns.contains_key(&alias) {
            self.attach_restored(alias, cx);
            return;
        }
        let Some(host) = self.hosts.iter().find(|h| h.alias == alias).cloned() else {
            return;
        };
        if !berth_core::known_hosts::is_known(&host.hostname, host.port) {
            return;
        }
        if !self.restoring.insert(alias.clone()) {
            return;
        }
        self.statuses.insert(
            alias.clone(),
            HostStatus::Connection(ConnectionState::Connecting),
        );
        tracing::info!(%alias, "connecting for its restored panes");
        let choice = self.settings.host_server(&alias);
        let forwards = host.forwards.clone();
        cx.spawn(async move |this, cx| {
            let manifest = crate::servers::carried().map(|s| &s.manifest);
            let reached =
                runtime::spawn(async move { session::reach_host(&host, choice, manifest).await })
                    .await
                    .map_err(|_| strings::RUNTIME_DROPPED.to_string())
                    .and_then(|r| r.map_err(|e| e.to_string()));
            match reached {
                Ok(reached) => {
                    let forwarders =
                        session::start_forwards(reached.connection.clone(), &forwards).await;
                    let _ = this.update(cx, |this, cx| {
                        this.restoring.remove(&alias);
                        this.join_connection(&alias, reached.connection, forwarders, cx);
                        if let Some(held) = reached.server {
                            this.hold_host_server(alias.clone(), held, cx);
                        }
                        this.server_decided(alias.clone(), reached.next, false, cx);
                        this.attach_restored(alias, cx);
                    });
                }
                Err(error) => {
                    tracing::warn!(%alias, %error, "could not connect for restored panes");
                    let _ = this.update(cx, |this, cx| {
                        this.restoring.remove(&alias);
                        this.statuses
                            .insert(alias.clone(), HostStatus::Failed(error));
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// Attach every restored pane of a connected host, in whatever tab: a
    /// tmux one to its session by name, which tmux makes again if it has
    /// gone; a server one to its session by id, or to a new one in the same
    /// pane if it has ended or the host's server cannot be used.
    pub(super) fn attach_restored(&mut self, alias: String, cx: &mut Context<Self>) {
        let Some(connection) = self.conns.get(&alias).map(|c| c.connection.clone()) else {
            return;
        };
        let server = self.server_of(&Machine::Host(alias.clone()));
        let waiting: Vec<(u64, HostSession, Entity<TerminalView>)> = self
            .panes
            .iter_mut()
            .filter(|p| !p.is_local() && p.alias() == alias)
            .filter_map(|p| {
                let host = p.restored.as_ref()?.host.clone()?;
                p.restored = None;
                Some((p.id, host, p.view.clone()))
            })
            .collect();
        for (id, saved, view) in waiting {
            let size = view.read(cx).grid_size();
            let connection = connection.clone();
            let server = server.clone();
            // A tmux slug not in use here, should a server pane fall back.
            let spare = self.free_slug(&alias);
            cx.spawn(async move |this, cx| {
                let opened = runtime::spawn(async move {
                    reattach_one(connection, server, saved, spare, size.cols, size.rows).await
                })
                .await
                .map_err(|_| strings::RUNTIME_DROPPED.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
                let _ = this.update(cx, |this, cx| match opened {
                    Ok((pty, events, now)) => {
                        let Some(pane) = this.pane_mut(id) else {
                            return;
                        };
                        match now {
                            HostSession::Tmux(slug) => {
                                pane.on_server = false;
                                pane.session = None;
                                pane.slug = slug;
                            }
                            HostSession::Server(session) => {
                                pane.on_server = true;
                                pane.session = Some(session);
                                pane.slug = server_slug(session);
                            }
                        }
                        // The grid stays: the session's own screen comes
                        // over it.
                        view.update(cx, |view, _| {
                            view.swap_pty(pty);
                            session::start_pump(view, events);
                        });
                        cx.notify();
                    }
                    Err(error) => {
                        tracing::warn!(pane = id, %error, "a restored pane could not attach");
                        this.warnings.push(error);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn restored_pane(
        &mut self,
        saved: SavedPane,
        installed: &[String],
        cx: &mut Context<Self>,
    ) -> u64 {
        let palette = crate::terminal_style::TerminalStyle::default().palette;
        let terminal = session::terminal_for(saved.cols, saved.rows, palette);
        terminal.advance(saved.screen.as_bytes());
        let font_size = self.settings.font_size;
        let view = cx.new(|cx| {
            let mut style = crate::terminal_style::TerminalStyle::default();
            style.resolve_family(installed);
            style.font_size = font_size;
            TerminalView::new(terminal, None, style, cx)
        });
        self.next_pane_id += 1;
        let id = self.next_pane_id;
        self.watch_signals(id, &view, cx);
        let host = saved.host.as_ref().and_then(|saved| {
            self.hosts
                .iter()
                .find(|h| h.alias == saved.alias)
                .cloned()
                .map(|host| (host, saved.session.clone()))
        });
        let (slug, on_server) = match host.as_ref().map(|(_, session)| session) {
            None => (strings::LOCAL_ALIAS.to_string(), false),
            Some(HostSession::Tmux(slug)) => (slug.clone(), false),
            Some(HostSession::Server(session)) => (server_slug(*session), true),
        };
        let (host, host_session) = match host {
            Some((host, session)) => (Some(host), Some(session)),
            None => (None, None),
        };
        self.panes.push(Pane {
            id,
            host,
            slug,
            view,
            claude: None,
            restored: Some(Restored {
                cwd: saved.cwd,
                claude: saved.claude,
                session: saved.session,
                token: saved.token,
                host: host_session,
            }),
            session: None,
            direct: None,
            starting: false,
            on_server,
        });
        id
    }

    /// Bring a restored pane back: attach to its session if the local server
    /// still runs it (D50), and otherwise start a new one where the old one
    /// was, resuming its Claude conversation (D46, D48).
    fn resume(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_mut(id) else {
            return;
        };
        let Some(restored) = pane.restored.take() else {
            return;
        };
        let view = pane.view.clone();
        let size = view.read(cx).grid_size();
        // Claude's own directory first, since that is where its conversation
        // is found; the shell's saved one otherwise.
        let dir = restored
            .claude
            .as_ref()
            .map(|c| c.dir.clone())
            .filter(|d| d.is_dir())
            .or(restored.cwd.clone());
        let mut pane_env = self.pane_env();
        // zsh and bash resume it themselves from Berth's startup wrapper,
        // before their first prompt; anything else gets it typed.
        let mut type_it = None;
        if let Some(claude) = &restored.claude {
            if berth_core::local::resumes_itself(&pane_env) {
                pane_env = pane_env.resuming(&claude.session);
            } else {
                type_it = Some(claude.session.clone());
            }
        }
        let fresh_token = pane_env.token.clone();
        let saved_token = restored
            .token
            .as_deref()
            .and_then(berth_core::claude::PaneToken::parse);
        let cell = self.local_server.clone();
        let direct = self.local_direct();
        let saved_session = restored.session;
        tracing::info!(cwd = ?restored.cwd, session = ?saved_session, "bringing back a restored session");
        cx.notify();

        cx.spawn(async move |this, cx| {
            let opened = runtime::spawn(async move {
                if let Some(session_id) = saved_session
                    && let Ok(server) = session::local_server(cell.clone(), direct.clone()).await
                    && server
                        .list()
                        .await
                        .is_ok_and(|list| list.iter().any(|s| s.id == session_id))
                {
                    let local =
                        session::attach_session(&server, session_id, size.cols, size.rows).await?;
                    return Ok((
                        session::LocalTab {
                            local,
                            session: Some(session_id),
                            direct: None,
                        },
                        true,
                    ));
                }
                let tab =
                    session::open_local_tab(cell, direct, (size.cols, size.rows), pane_env, dir)
                        .await?;
                Ok::<_, berth_core::Error>((tab, false))
            })
            .await
            .map_err(|_| strings::RUNTIME_DROPPED.to_string())
            .and_then(|r| r.map_err(|e| e.to_string()));

            let _ = this.update(cx, |this, cx| match opened {
                Ok((tab, reattached)) => {
                    let pty = tab.local.pty;
                    if let Some(pane) = this.pane_mut(id) {
                        pane.session = tab.session;
                        pane.direct = tab.direct;
                        let token = if reattached {
                            saved_token
                        } else {
                            Some(fresh_token)
                        };
                        pane.claude = token.map(super::ClaudePane::new);
                    }
                    if !reattached && let Some(session) = &type_it {
                        resume_claude(&pty, session);
                    }
                    view.update(cx, |view, _| {
                        // A session still running brings its own screen; a
                        // new shell takes the old prompt's line.
                        if reattached {
                            view.swap_pty(pty);
                        } else {
                            view.resume(pty);
                        }
                        session::start_pump(view, tab.local.events);
                    });
                    this.listen_for_claude();
                    this.refresh_local_sessions(cx);
                    cx.notify();
                }
                Err(error) => {
                    tracing::warn!(%error, "could not bring back a restored session");
                    this.warnings.push(error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Every tab as it would be saved, local panes with their directories
    /// still to be looked up and hosts' panes with their host and session.
    fn snapshot_local(&self, cx: &Context<Self>) -> Vec<(SavedTab, Vec<Dir>)> {
        let mut out = Vec::new();
        for (position, tab) in self.tabs.iter().enumerate() {
            let order = tab.root.panes();
            let panes: Vec<&Pane> = order.iter().filter_map(|id| self.pane(*id)).collect();
            if panes.len() != order.len() {
                continue;
            }
            let Some(layout) = crate::restore::layout_of(&tab.root, &order) else {
                continue;
            };
            let mut saved = Vec::new();
            let mut dirs = Vec::new();
            for pane in panes {
                let view = pane.view.read(cx);
                let Some((screen, size)) = view.saved_screen() else {
                    continue;
                };
                if let Some(host) = &pane.host {
                    // A server pane whose session has not come yet has
                    // nothing to attach to again: its tab is not saved.
                    let Some(session) = host_session(pane) else {
                        continue;
                    };
                    dirs.push(Dir::Known(None));
                    saved.push(SavedPane {
                        cwd: None,
                        cols: size.cols,
                        rows: size.rows,
                        screen,
                        claude: None,
                        session: None,
                        token: None,
                        host: Some(SavedHost {
                            alias: host.alias.clone(),
                            session,
                        }),
                    });
                    continue;
                }
                dirs.push(
                    match (&pane.restored, view.pty().and_then(|p| p.local_pid())) {
                        (Some(restored), _) => Dir::Known(restored.cwd.clone()),
                        (None, Some(pid)) => Dir::Ask(pid),
                        (None, None) => Dir::Known(None),
                    },
                );
                saved.push(SavedPane {
                    session: match &pane.restored {
                        Some(restored) => restored.session,
                        None => pane.session,
                    },
                    token: match &pane.restored {
                        Some(restored) => restored.token.clone(),
                        None => pane
                            .session
                            .and(pane.claude.as_ref())
                            .map(|c| c.token.as_str().to_string()),
                    },
                    cwd: None,
                    cols: size.cols,
                    rows: size.rows,
                    screen,
                    claude: match &pane.restored {
                        // Not woken yet: still the conversation it was saved with.
                        Some(restored) => restored.claude.clone(),
                        None => pane.claude.as_ref().and_then(|c| conversation(&c.tracker)),
                    },
                    host: None,
                });
            }
            if saved.len() != order.len() {
                continue;
            }
            out.push((
                SavedTab {
                    position,
                    focus: order.iter().position(|id| *id == tab.focus).unwrap_or(0),
                    layout,
                    panes: saved,
                },
                dirs,
            ));
        }
        out
    }

    /// Save now, with the directory lookups and the write off the UI thread.
    pub(super) fn save_local(&self, cx: &mut Context<Self>) {
        let pending = self.snapshot_local(cx);
        cx.background_executor()
            .spawn(async move { finish(pending).save() })
            .detach();
    }

    /// Save every minute until the workspace goes, and once more on quit.
    pub(super) fn watch_saves(&self, cx: &mut Context<Self>) {
        cx.on_app_quit(|this, cx| {
            // On the way out there is no later to put this off to. The lookups
            // are a few milliseconds each and the quit waits for them.
            finish(this.snapshot_local(cx)).save();
            this.save_settings_due();
            async {}
        })
        .detach();

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SAVE_EVERY).await;
                if this.update(cx, |this, cx| this.save_local(cx)).is_err() {
                    return;
                }
            }
        })
        .detach();
    }
}

/// Type `claude --resume <id>` into a freshly started shell (D48), for a shell
/// Berth has no startup wrapper for; zsh and bash resume it themselves.
///
/// Through the shell rather than as its own process, because only the user's
/// shell has their PATH and environment: a Berth opened from the Dock does not
/// know where `claude` lives, and Claude's own tools would run without the
/// user's setup. The id is the only thing typed, and nothing but a strict UUID
/// ever is; the directory is the shell's own, never typed.
fn resume_claude(pty: &berth_core::pty::Pty, session: &str) {
    let Some(line) = berth_core::claude::resume_line(session) else {
        tracing::warn!("a saved Claude session id was not a UUID, not resuming");
        return;
    };
    tracing::info!(%session, "resuming a Claude conversation");
    let pty = pty.clone();
    // Fire and forget, as the terminal's own writes are: the line lands or
    // the pane is already gone.
    runtime::handle().spawn(async move {
        pty.write(format!("{line}\r")).await;
    });
}

/// The conversation worth resuming in a pane: one Claude has reported, and
/// that did not end with the user exiting it. An interrupted one is resumed;
/// one the user closed is not brought back.
fn conversation(tracker: &berth_core::claude::Tracker) -> Option<SavedClaude> {
    if tracker.state == Some(berth_core::claude::State::Finished) {
        return None;
    }
    Some(SavedClaude {
        session: tracker.session_id.clone()?,
        dir: tracker.cwd.clone()?,
    })
}

/// The session a host's pane shows, to attach to again: as it was restored
/// if it has not attached yet, else its id on the host's server or its tmux
/// slug. None for a server pane whose session has not come.
fn host_session(pane: &Pane) -> Option<HostSession> {
    if let Some(saved) = pane.restored.as_ref().and_then(|r| r.host.clone()) {
        return Some(saved);
    }
    match (pane.on_server, pane.session) {
        (true, Some(id)) => Some(HostSession::Server(id)),
        (true, None) => None,
        (false, _) => Some(HostSession::Tmux(pane.slug.clone())),
    }
}

/// Attach a restored host pane's session again, or what stands in for it:
/// see [`Workspace::attach_restored`]. What it shows now comes back with
/// the pty.
async fn reattach_one(
    connection: Arc<Connection>,
    server: Option<berth_core::server::local::Server>,
    saved: HostSession,
    spare: String,
    cols: u16,
    rows: u16,
) -> berth_core::Result<(
    berth_core::pty::Pty,
    tokio::sync::mpsc::Receiver<berth_core::pty::PtyEvent>,
    HostSession,
)> {
    let slug = match (saved, server) {
        (HostSession::Tmux(slug), _) => slug,
        (HostSession::Server(id), Some(server)) => {
            let running = server.list().await?.iter().any(|s| s.id == id);
            let id = if running {
                id
            } else {
                server.create(None, cols, rows, None, None).await?
            };
            let (pty, events) = server.attach(id, cols, rows, None).await?;
            return Ok((pty, events, HostSession::Server(id)));
        }
        // The host's server cannot be used now: a tmux session in its place.
        (HostSession::Server(_), None) => spare,
    };
    let attached = session::rejoin(connection, slug.clone(), cols, rows).await?;
    Ok((attached.pty, attached.events, HostSession::Tmux(slug)))
}

/// Fill in the directories and make the file's contents. Blocking.
fn finish(pending: Vec<(SavedTab, Vec<Dir>)>) -> Saved {
    let tabs = pending
        .into_iter()
        .map(|(mut tab, dirs)| {
            for (pane, dir) in tab.panes.iter_mut().zip(dirs) {
                pane.cwd = match dir {
                    Dir::Known(cwd) => cwd,
                    Dir::Ask(pid) => berth_core::local::cwd_of(pid),
                };
                // Resume looks in the project folder of the directory it runs
                // from, which is the resolved path: `/private/tmp`, not `/tmp`.
                if let Some(claude) = &mut pane.claude
                    && let Ok(real) = claude.dir.canonicalize()
                {
                    claude.dir = real;
                }
            }
            tab
        })
        .collect();
    Saved {
        version: berth_core::restore::VERSION,
        tabs,
    }
}
