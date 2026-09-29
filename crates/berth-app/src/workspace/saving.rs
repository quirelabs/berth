//! Local sessions kept across a quit or a reboot (D46).
//!
//! The shell itself ends with Berth (D36). Its tab, its split, its directory
//! and its last screen are saved every minute, when a local tab closes and on
//! quit. On launch the tabs come back with nothing running, and a tab's shells
//! start, in their saved directories under their old screens, when that tab is
//! first shown. A tab never looked at costs no process.

use std::path::PathBuf;

use berth_core::restore::{Saved, SavedClaude, SavedPane, SavedTab};

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
        tracing::info!(tabs = self.tabs.len(), "restored local sessions");
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
        for id in waiting {
            self.resume(id, cx);
        }
    }

    fn restored_pane(
        &mut self,
        saved: SavedPane,
        installed: &[String],
        cx: &mut Context<Self>,
    ) -> u64 {
        let palette = crate::terminal_style::TerminalStyle::default().palette;
        let terminal = session::terminal_for(saved.cols, saved.rows, palette);
        if let Ok(mut term) = terminal.lock() {
            term.advance(saved.screen.as_bytes());
        }
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
        self.panes.push(Pane {
            id,
            host: None,
            slug: strings::LOCAL_ALIAS.to_string(),
            view,
            claude: None,
            restored: Some(Restored {
                cwd: saved.cwd,
                claude: saved.claude,
            }),
        });
        id
    }

    /// Start a new shell in a restored pane, where the old one was.
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
        if let Some(pane) = self.pane_mut(id) {
            pane.claude = Some(super::ClaudePane::new(pane_env.token.clone()));
        }
        tracing::info!(cwd = ?restored.cwd, "starting a shell in a restored session");
        cx.notify();

        cx.spawn(async move |this, cx| {
            let opened = runtime::spawn(async move {
                match &dir {
                    Some(cwd) => {
                        berth_core::local::open_in(cwd, size.cols, size.rows, Some(&pane_env)).await
                    }
                    None => berth_core::local::open(size.cols, size.rows, Some(&pane_env)).await,
                }
            })
            .await
            .map_err(|_| strings::RUNTIME_DROPPED.to_string())
            .and_then(|r| r.map_err(|e| e.to_string()));

            let _ = this.update(cx, |this, cx| match opened {
                Ok((pty, events)) => {
                    if let Some(session) = &type_it {
                        resume_claude(&pty, session);
                    }
                    view.update(cx, |view, cx| {
                        view.resume(pty);
                        session::start_pump(&cx.entity(), events, cx);
                    });
                    cx.notify();
                }
                Err(error) => {
                    tracing::warn!(%error, "could not start a shell in a restored session");
                    this.warnings.push(error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Every tab that holds only local shells, as it would be saved, with the
    /// directories still to be looked up.
    ///
    /// Remote tabs are not saved here: their sessions live in tmux on the
    /// server, and the sidebar lists them whether a tab is open or not.
    fn snapshot_local(&self, cx: &Context<Self>) -> Vec<(SavedTab, Vec<Dir>)> {
        let mut out = Vec::new();
        for (position, tab) in self.tabs.iter().enumerate() {
            let order = tab.root.panes();
            let panes: Vec<&Pane> = order.iter().filter_map(|id| self.pane(*id)).collect();
            if panes.len() != order.len() || panes.iter().any(|p| !p.is_local()) {
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
                dirs.push(
                    match (&pane.restored, view.pty().and_then(|p| p.local_pid())) {
                        (Some(restored), _) => Dir::Known(restored.cwd.clone()),
                        (None, Some(pid)) => Dir::Ask(pid),
                        (None, None) => Dir::Known(None),
                    },
                );
                saved.push(SavedPane {
                    cwd: None,
                    cols: size.cols,
                    rows: size.rows,
                    screen,
                    claude: match &pane.restored {
                        // Not woken yet: still the conversation it was saved with.
                        Some(restored) => restored.claude.clone(),
                        None => pane.claude.as_ref().and_then(|c| conversation(&c.tracker)),
                    },
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
