//! One connection's clock, one loop per host.
//!
//! Keepalives while connected, the slide into Degraded when they stop being
//! answered, the backoff after that, and the two things that ride alongside it:
//! the round-trip measurement and the agent scan. All of it is timers and
//! `cx.spawn`, none of it is layout, and the state machine it drives is tested
//! on its own in berth-core.

use super::*;

impl Workspace {
    /// Run one connection's clock: keepalives while connected, the slide into
    /// Degraded when they stop being answered, and the backoff after that.
    ///
    /// The loop sleeps until that machine's own next deadline rather than
    /// polling, and exits when the machine rests or when the connection is
    /// gone. The alias is what ends it: a retired connection stops resolving,
    /// so closing a host's last session ends its loop with no separate
    /// bookkeeping and two loops can never tick one machine.
    pub(super) fn drive(&mut self, alias: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(Some(wait)) = this.read_with(cx, |this, _| {
                    this.conns.get(&alias).and_then(|c| c.reconnect.next_wait())
                }) else {
                    return;
                };
                cx.background_executor().timer(wait).await;

                let Ok(Some(step)) = this.update(cx, |this, cx| {
                    let step = this
                        .conns
                        .get_mut(&alias)
                        .map(|c| c.reconnect.handle(Event::Tick))?;
                    if step.changed {
                        tracing::info!(%alias, state = ?step.state, "connection state");
                        this.statuses
                            .insert(alias.clone(), HostStatus::Connection(step.state));
                        cx.notify();
                    }
                    Some(step)
                }) else {
                    return;
                };

                for action in step.actions {
                    match action {
                        Action::SendKeepalive => {
                            let Ok(Some(connection)) = this.read_with(cx, |this, _| {
                                this.conns.get(&alias).map(|c| c.connection.clone())
                            }) else {
                                return;
                            };
                            // Deliberately not awaited inline: a keepalive on a
                            // dead link blocks until its own timeout, and the
                            // machine has to keep ticking through that or it
                            // can never reach Degraded.
                            tracing::debug!(%alias, "keepalive");
                            let alive = this.clone();
                            let alias = alias.clone();
                            cx.spawn(async move |cx| {
                                if runtime::spawn(crate::reconnect::keepalive(connection))
                                    .await
                                    .unwrap_or(false)
                                {
                                    let _ = alive.update(cx, |this, cx| {
                                        let changed = this
                                            .conns
                                            .get_mut(&alias)
                                            .map(|c| {
                                                c.reconnect.handle(Event::KeepaliveAck).changed
                                            })
                                            .unwrap_or(false);
                                        if changed {
                                            cx.notify();
                                        }
                                    });
                                }
                            })
                            .detach();
                        }
                        Action::Connect => Self::reattach(&this, &alias, cx).await,
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

    /// Rebuild a host's transport underneath its live views.
    ///
    /// Every session on the host comes back together, because they share the
    /// connection that dropped. Each one re-attaches to its own tmux session at
    /// the size its grid already is, so tmux repaints into the same shape and
    /// nothing reflows on the way back.
    ///
    /// Everything the sessions need is done before `ConnectSucceeded` is fed
    /// in: the connection, the attaches, the pty swaps, the pumps, and the port
    /// forwards. The machine only returns to Connected once the host is
    /// genuinely usable, rather than announcing it and then catching up.
    async fn reattach(this: &WeakEntity<Self>, alias: &str, cx: &mut AsyncApp) {
        // Local sessions never reach here: they have no connection to rebuild,
        // so nothing drives a machine for them.
        let Ok(Some(host)) = this.read_with(cx, |this, _| {
            this.panes
                .iter()
                .find(|p| p.alias() == alias)
                .and_then(|p| p.host.clone())
        }) else {
            return;
        };
        // Panes on the host's server are not reattached here: its handle
        // reconnects them itself, through relays on the new connection.
        let Ok(sessions) = this.read_with(cx, |this, _| {
            this.panes
                .iter()
                .filter(|p| p.alias() == alias && !p.on_server)
                .map(|p| (p.id, p.slug.clone(), p.view.clone()))
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        let forwards = host.forwards.clone();

        // The first session rebuilds the connection; the rest join it.
        let mut restored: Vec<(u64, session::Attached)> = Vec::new();
        let mut connection = None;
        for (id, slug, view) in sessions {
            let size = view.read_with(cx, |view, _| view.grid_size());
            let attached = match &connection {
                None => {
                    runtime::spawn(session::attach(host.clone(), slug, size.cols, size.rows)).await
                }
                Some(existing) => {
                    runtime::spawn(session::rejoin(
                        Arc::clone(existing),
                        slug,
                        size.cols,
                        size.rows,
                    ))
                    .await
                }
            };
            match attached {
                Ok(Ok(attached)) => {
                    connection.get_or_insert_with(|| attached.connection.clone());
                    restored.push((id, attached));
                }
                failed => {
                    if let Ok(Err(error)) = failed {
                        tracing::warn!(%alias, %error, "reattach failed");
                    }
                    let _ = this.update(cx, |this, cx| {
                        if let Some(conn) = this.conns.get_mut(alias) {
                            conn.reconnect.handle(Event::ConnectFailed);
                        }
                        cx.notify();
                    });
                    return;
                }
            }
        }
        // Only server panes on the host: the connection is made on its own.
        let connection = match connection {
            Some(connection) => connection,
            None => {
                let host = host.clone();
                match runtime::spawn(async move {
                    Connection::connect(&host, berth_core::known_hosts::TrustPolicy::KnownHostsOnly)
                        .await
                })
                .await
                {
                    Ok(Ok(connection)) => Arc::new(connection),
                    failed => {
                        if let Ok(Err(error)) = failed {
                            tracing::warn!(%alias, %error, "reconnect failed");
                        }
                        let _ = this.update(cx, |this, cx| {
                            if let Some(conn) = this.conns.get_mut(alias) {
                                conn.reconnect.handle(Event::ConnectFailed);
                            }
                            cx.notify();
                        });
                        return;
                    }
                }
            }
        };

        // Forwards come back before the state does, so nothing ever sees a host
        // reported usable with its ports dead. The old forwarders are dropped
        // first: their listeners belong to a connection that no longer exists,
        // and holding them would keep the local ports bound to nothing.
        let _ = this.update(cx, |this, _| {
            if let Some(conn) = this.conns.get_mut(alias) {
                conn.forwarders.clear();
            }
        });
        let forwarders = session::start_forwards(connection.clone(), &forwards).await;

        let _ = this.update(cx, |this, cx| {
            for (id, attached) in restored {
                let Some(view) = this.pane(id).map(|p| p.view.clone()) else {
                    continue;
                };
                view.update(cx, |view, _| {
                    // The grid is untouched. tmux repaints over it in place.
                    view.swap_pty(attached.pty.clone());
                    session::start_pump(view, attached.events);
                });
            }
            let Some(conn) = this.conns.get_mut(alias) else {
                return;
            };
            // The host's server opens its next relay on this one.
            if let Some(held) = &conn.server {
                *held.current.lock().unwrap_or_else(|p| p.into_inner()) = connection.clone();
            }
            conn.connection = connection;
            conn.forwarders = forwarders;
            let step = conn.reconnect.handle(Event::ConnectSucceeded);
            let forwards = conn.forwarders.len();
            tracing::info!(%alias, state = ?step.state, forwards, "reattached");
            this.statuses
                .insert(alias.to_string(), HostStatus::Connection(step.state));
            cx.notify();
        });
    }

    /// How often a host is asked what is running in its sessions.
    ///
    /// Presence does not need to be instant. Five seconds is under the time it
    /// takes to notice an icon appear, and it is one exec per host rather than
    /// per session, so a workspace of ten panes still asks once.
    const AGENT_POLL: Duration = Duration::from_secs(5);

    /// Watch one host's sessions and the agents in them, until its
    /// connection goes. The same scan feeds the sidebar tree and the ghost.
    pub(super) fn watch_agents(&self, alias: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            // A host without tmux fails this every five seconds forever. Say so
            // once, then stay quiet until it works again, so the reason exists
            // somewhere without the log becoming one.
            let mut reported = false;
            // Asked at once, then every poll, so a host's detached sessions are
            // in the tree as soon as it connects rather than five seconds later.
            let mut first = true;
            loop {
                if !first {
                    cx.background_executor().timer(Self::AGENT_POLL).await;
                }
                first = false;
                let Ok(Some(connection)) = this.read_with(cx, |this, _| {
                    this.conns.get(&alias).map(|c| c.connection.clone())
                }) else {
                    // The host is gone, and so is the reason to keep asking.
                    return;
                };

                let scanned = runtime::spawn(async move {
                    connection
                        .exec(&berth_core::agents::scan_command())
                        .await
                        .map(|out| out.stdout_string())
                })
                .await;
                let output = match scanned {
                    Ok(Ok(output)) => output,
                    Ok(Err(error)) => {
                        if !reported {
                            tracing::debug!(%alias, %error, "agent scan failed, no icons for this host");
                            reported = true;
                        }
                        continue;
                    }
                    // The runtime dropped the task, so nothing here will work
                    // again and there is no point asking every five seconds.
                    Err(_) => return,
                };
                reported = false;

                let scan = berth_core::agents::parse_scan(&output);
                let found = crate::sidebar::listing(&scan);
                let programs = crate::sidebar::programs(&scan);

                let updated = this.update(cx, |this, cx| {
                    // Repaint only on a change, or this is a wakeup every five
                    // seconds forever for a picture that did not move.
                    // Nothing recorded and nothing found is not a change,
                    // or every host logs one at startup for a picture that was
                    // always empty.
                    // What runs where is drawn too: an icon on a tab, a
                    // stone in the sidebar.
                    if this.programs.get(&alias) != Some(&programs)
                        && !(programs.is_empty() && !this.programs.contains_key(&alias))
                    {
                        this.programs.insert(alias.clone(), programs);
                        cx.notify();
                    }
                    let changed = presence_changed(this.running.get(&alias), &found);
                    if changed {
                        let agents = found.values().filter(|a| **a).count();
                        tracing::info!(%alias, sessions = found.len(), agents, "sessions changed");
                        this.running.insert(alias.clone(), found);
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// A first RTT sample for the status bar.
    pub(super) fn measure_rtt(&self, alias: String, cx: &mut Context<Self>) {
        let Some(connection) = self.conns.get(&alias).map(|c| c.connection.clone()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Ok(Some(rtt)) = runtime::spawn(session::round_trip(connection)).await {
                let _ = this.update(cx, |this, cx| {
                    if let Some(conn) = this.conns.get_mut(&alias) {
                        conn.rtt = Some(rtt);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    /// The local heavy-output benchmark, in the local shell that just opened.
    pub(super) async fn run_local_benchmark(this: &WeakEntity<Self>, cx: &mut AsyncApp) {
        if crate::bench::Bench::from_env() != Some(crate::bench::Bench::LocalCat) {
            return;
        }
        let Ok(Some((view, pty))) = this.read_with(cx, |this, _| {
            let pane = this.focused()?;
            Some((
                pane.view.downgrade(),
                pane.view.read_with(cx, |v, _| v.pty())?,
            ))
        }) else {
            return;
        };
        let report = crate::bench::run_local_cat(view, pty, cx).await;
        println!("\n{}", report.text);
        if !report.passed {
            eprintln!("benchmark did not meet its criteria");
            crate::bench::mark_failed();
        }
        cx.update(|cx| cx.quit());
    }

    /// The resize benchmark (D53): two local shells full of text side by
    /// side; the divider is dragged across and back, then the window is
    /// resized, one step a display frame, and every frame of both panes is
    /// held to 8.3 ms, one pty resize, and the size its bounds asked for.
    pub(super) async fn run_resize_benchmark(this: &WeakEntity<Self>, cx: &mut AsyncApp) {
        use crate::bench::{RESIZE_FRAME, RESIZE_STEPS};
        if crate::bench::Bench::from_env() != Some(crate::bench::Bench::Resize)
            || !crate::bench::start_resize_once()
        {
            return;
        }
        let executor = cx.background_executor().clone();
        let wait = |ms: u64| executor.timer(Duration::from_millis(ms));
        let fill = "seq 1 3000; ls -la /usr/bin | head -200\n";
        wait(1500).await;
        let _ = this.update(cx, |this, cx| {
            if let Some(pty) = this.focused().and_then(|p| p.view.read(cx).pty()) {
                crate::runtime::handle().spawn(async move { pty.write(fill).await });
            }
            this.split(Axis::Horizontal, cx);
        });
        wait(2000).await;
        let Ok(Some((tab, views))) = this.update(cx, |this, cx| {
            if let Some(pty) = this.focused().and_then(|p| p.view.read(cx).pty()) {
                crate::runtime::handle().spawn(async move { pty.write(fill).await });
            }
            let tab = this.tab()?;
            let views: Vec<_> = tab
                .root
                .panes()
                .iter()
                .filter_map(|id| Some(this.pane(*id)?.view.downgrade()))
                .collect();
            Some((tab.id, views))
        }) else {
            return;
        };
        wait(1500).await;
        let reset = |cx: &mut AsyncApp| {
            for view in &views {
                let _ = view.update(cx, |view, _| crate::bench::reset_frames(view));
            }
        };
        let measure = |cx: &mut AsyncApp| -> Vec<crate::bench::ResizeRun> {
            views
                .iter()
                .filter_map(|view| {
                    view.read_with(cx, |view, _| crate::bench::resize_run(view))
                        .ok()
                })
                .collect()
        };

        reset(cx);
        for i in 0..RESIZE_STEPS {
            let share = crate::bench::divider_at(i);
            let _ = this.update(cx, |this, cx| this.drag_divider(tab, &[], share, cx));
            cx.background_executor().timer(RESIZE_FRAME).await;
        }
        wait(200).await;
        let dragged = measure(cx);

        let window = cx.update(|cx| cx.windows().first().copied());
        let mut resized = Vec::new();
        if let Some(window) = window {
            let start = window.update(cx, |_, window, _| window.bounds().size).ok();
            if let Some(start) = start {
                reset(cx);
                for i in 0..RESIZE_STEPS {
                    let k = 0.6 + 0.4 * crate::bench::divider_at(i);
                    let size = gpui::size(start.width * k, start.height * k);
                    let _ = window.update(cx, |_, window, _| window.resize(size));
                    cx.background_executor().timer(RESIZE_FRAME).await;
                }
                let _ = window.update(cx, |_, window, _| window.resize(start));
                wait(200).await;
                resized = measure(cx);
            }
        }

        let mut text = String::from("resize benchmark (D53)\n");
        let mut passed = dragged.len() == 2 && resized.len() == 2;
        for (index, run) in dragged.iter().enumerate() {
            text.push_str(&run.line(&format!("divider drag, pane {}", index + 1)));
            passed &= run.passed();
        }
        for (index, run) in resized.iter().enumerate() {
            text.push_str(&run.line(&format!("window resize, pane {}", index + 1)));
            passed &= run.passed();
        }
        if !passed {
            text.push_str("FAIL\n");
        }
        println!("\n{text}");
        if !passed {
            eprintln!("benchmark did not meet its criteria");
            crate::bench::mark_failed();
        }
        cx.update(|cx| cx.quit());
    }

    /// The feel benchmark (D53): in local shells, each action timed from
    /// the moment it is asked for to the frame that shows it. A new tab's
    /// pane in the next frame and its prompt within 50 ms at p95; a split,
    /// a focus, a tab switch, a close and a tab drop in the next frame. A
    /// remote prompt's 1 RTT plus 30 ms needs a host and is not here.
    pub(super) async fn run_feel_benchmark(this: &WeakEntity<Self>, cx: &mut AsyncApp) {
        use crate::bench::{FeelCase, PROMPT_P95};
        use std::time::Instant;
        if crate::bench::Bench::from_env() != Some(crate::bench::Bench::Feel)
            || !crate::bench::start_feel_once()
        {
            return;
        }
        let executor = cx.background_executor().clone();
        let next = crate::bench::next_frame(refresh_hz());
        // Look every millisecond until `done` says so, for at most a
        // second; the time it said so, if it did.
        async fn until(
            executor: &gpui::BackgroundExecutor,
            cx: &mut AsyncApp,
            this: &WeakEntity<Workspace>,
            done: impl Fn(&Workspace, &gpui::App) -> Option<Instant>,
        ) -> Option<Instant> {
            for _ in 0..1000 {
                if let Ok(Some(at)) = this.read_with(cx, |this, cx| done(this, cx)) {
                    return Some(at);
                }
                executor.timer(Duration::from_millis(1)).await;
            }
            None
        }
        let rendered_after = |t0: Instant| {
            move |this: &Workspace, _: &gpui::App| this.rendered_at.filter(|at| *at > t0)
        };
        executor.timer(Duration::from_millis(1500)).await;

        let mut shown = FeelCase {
            what: "new tab, pane shown",
            samples: Vec::new(),
            limit: next,
        };
        let mut prompt = FeelCase {
            what: "new tab, local prompt",
            samples: Vec::new(),
            limit: PROMPT_P95,
        };
        for _ in 0..20 {
            let Ok(before) = this.read_with(cx, |this, _| {
                this.panes.iter().map(|p| p.id).collect::<Vec<_>>()
            }) else {
                return;
            };
            let t0 = Instant::now();
            let _ = this.update(cx, |this, cx| this.new_tab(cx));
            let fresh = |this: &Workspace| {
                this.panes
                    .iter()
                    .find(|p| !before.contains(&p.id))
                    .map(|p| p.id)
            };
            let painted = until(&executor, cx, this, |this, cx| {
                let id = fresh(this)?;
                this.pane(id)?.view.read(cx).first_painted_at
            })
            .await;
            let content = until(&executor, cx, this, |this, cx| {
                let id = fresh(this)?;
                this.pane(id)?.view.read(cx).first_content_at
            })
            .await;
            shown
                .samples
                .push(painted.map_or(Duration::MAX, |at| at - t0));
            prompt
                .samples
                .push(content.map_or(Duration::MAX, |at| at - t0));
            let _ = this.update(cx, |this, cx| {
                if let Some(id) = fresh(this) {
                    this.close(id, cx);
                }
            });
            executor.timer(Duration::from_millis(100)).await;
        }

        let mut split = FeelCase {
            what: "split",
            samples: Vec::new(),
            limit: next,
        };
        let mut focus = FeelCase {
            what: "focus pane",
            samples: Vec::new(),
            limit: next,
        };
        let mut switch = FeelCase {
            what: "switch tab",
            samples: Vec::new(),
            limit: next,
        };
        let mut close = FeelCase {
            what: "close pane",
            samples: Vec::new(),
            limit: next,
        };
        let mut drop = FeelCase {
            what: "tab drop",
            samples: Vec::new(),
            limit: next,
        };
        for _ in 0..3 {
            let t0 = Instant::now();
            let _ = this.update(cx, |this, cx| this.split(Axis::Horizontal, cx));
            let at = until(&executor, cx, this, rendered_after(t0)).await;
            split.samples.push(at.map_or(Duration::MAX, |at| at - t0));
            executor.timer(Duration::from_millis(300)).await;
        }
        for _ in 0..20 {
            let t0 = Instant::now();
            let _ = this.update(cx, |this, cx| this.run(crate::input::Command::NextPane, cx));
            let at = until(&executor, cx, this, rendered_after(t0)).await;
            focus.samples.push(at.map_or(Duration::MAX, |at| at - t0));
            executor.timer(Duration::from_millis(20)).await;
        }
        for _ in 0..3 {
            let _ = this.update(cx, |this, cx| this.new_tab(cx));
            executor.timer(Duration::from_millis(300)).await;
        }
        for i in 0..20 {
            let t0 = Instant::now();
            let _ = this.update(cx, |this, cx| this.select_tab(1 + i % 3, cx));
            let at = until(&executor, cx, this, rendered_after(t0)).await;
            switch.samples.push(at.map_or(Duration::MAX, |at| at - t0));
            executor.timer(Duration::from_millis(20)).await;
        }
        // A tab dragged across the strip a step a frame, then dropped.
        let drag = this.read_with(cx, |this, _| {
            let bounds = this.tab_bounds.borrow();
            let tab = this.tabs.first()?.id;
            let far = this
                .tabs
                .last()
                .and_then(|t| bounds.get(&t.id))
                .map(|b| f32::from(b.origin.x + b.size.width))?;
            Some((tab, far))
        });
        if let Ok(Some((tab, far))) = drag {
            for step in 0..=60 {
                let x = far * step as f32 / 60.0;
                let _ = this.update(cx, |this, cx| this.drag_tab_to(tab, x, cx));
                executor.timer(crate::bench::RESIZE_FRAME).await;
            }
            let t0 = Instant::now();
            let _ = this.update(cx, |this, cx| this.drop_tab(tab, cx));
            let at = until(&executor, cx, this, rendered_after(t0)).await;
            drop.samples.push(at.map_or(Duration::MAX, |at| at - t0));
        }
        loop {
            let Ok(Some(pane)) = this.read_with(cx, |this, _| {
                this.tab()
                    .filter(|t| t.root.panes().len() > 1)
                    .map(|t| t.focus)
            }) else {
                break;
            };
            let t0 = Instant::now();
            let _ = this.update(cx, |this, cx| this.close(pane, cx));
            let at = until(&executor, cx, this, rendered_after(t0)).await;
            close.samples.push(at.map_or(Duration::MAX, |at| at - t0));
            executor.timer(Duration::from_millis(50)).await;
        }

        let cases = [shown, prompt, split, focus, switch, close, drop];
        let mut text = String::from("feel benchmark (D53)\n");
        for case in &cases {
            text.push_str(&case.line());
        }
        let passed = cases.iter().all(FeelCase::passed);
        if !passed {
            text.push_str("FAIL\n");
        }
        println!("\n{text}");
        if !passed {
            eprintln!("benchmark did not meet its criteria");
            crate::bench::mark_failed();
        }
        cx.update(|cx| cx.quit());
    }

    /// Drive the real render loop for a benchmark, then quit.
    pub(super) async fn run_benchmark(this: &WeakEntity<Self>, cx: &mut AsyncApp) {
        let Some(mode) = crate::bench::Bench::from_env() else {
            return;
        };
        let Ok(Some((view, pty, connection))) = this.read_with(cx, |this, _| {
            let pane = this.focused()?;
            Some((
                pane.view.downgrade(),
                pane.view.read_with(cx, |v, _| v.pty())?,
                this.conn_of(pane.id)?.connection.clone(),
            ))
        }) else {
            return;
        };

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
                        berth_core::env::bench_settle_secs().unwrap_or(20),
                    ),
                    cx,
                )
                .await
            }
            crate::bench::Bench::Keys => {
                crate::bench::run_keys(view, pty, 40, refresh_hz(), cx).await
            }
            // Local only; started from `run_local_benchmark`.
            crate::bench::Bench::LocalCat
            | crate::bench::Bench::Resize
            | crate::bench::Bench::Feel => return,
        };
        println!("\n{}", report.text);
        let _ = runtime::spawn(async move {
            let _ = connection.exec(&crate::bench::remove_file_command()).await;
        })
        .await;
        if !report.passed {
            // Non-zero exit, so a regression is a failure rather than a number
            // someone has to notice. Recorded and read in `main`: quitting from
            // here runs the destructors, and exiting from here would not.
            eprintln!("benchmark did not meet its criteria");
            crate::bench::mark_failed();
        }
        cx.update(|cx| cx.quit());
    }
}
