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
        let Ok(sessions) = this.read_with(cx, |this, _| {
            this.panes
                .iter()
                .filter(|p| p.alias() == alias)
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
        let Some(connection) = connection else {
            return;
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

                let found =
                    crate::sidebar::listing(&berth_core::agents::parse_scan(&output));

                let updated = this.update(cx, |this, cx| {
                    // Repaint only on a change, or this is a wakeup every five
                    // seconds forever for a picture that did not move.
                    // Nothing recorded and nothing found is not a change,
                    // or every host logs one at startup for a picture that was
                    // always empty.
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

    /// Whether an agent is running in a pane's session.
    pub(super) fn has_agent(&self, pane: &Pane) -> bool {
        !pane.is_local()
            && self
                .running
                .get(pane.alias())
                .and_then(|sessions| sessions.get(&pane.slug))
                .copied()
                .unwrap_or(false)
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
            crate::bench::Bench::LocalCat => return,
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
