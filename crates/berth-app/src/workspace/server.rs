//! Offering Berth's server to a host, and installing it when the user says
//! so (D55).
//!
//! Nothing here installs anything unasked. A host is looked at when the user
//! connects to it, never changed; the dialog is the only way to an install,
//! and a reconnect or a warm connection in the background never raises it.

use berth_core::remote::install::{self, Installed};
use berth_core::remote::{Choice, Next, Plan, Stays};

use super::*;

/// Why an install did not happen.
enum Failed {
    /// The file beside the app is missing or is not the one it was built with.
    Carried(String),
    Install(install::Error),
}

impl Workspace {
    /// Look at a host this app has just connected to, and decide what to do
    /// about its server. `asking` is whether the user asked for this host, so
    /// may be asked something; `explicit` is that they asked about the server
    /// itself, so every answer is said out loud.
    pub(super) fn check_server(
        &mut self,
        alias: String,
        asking: bool,
        explicit: bool,
        cx: &mut Context<Self>,
    ) {
        let servers = crate::servers::carried();
        let choice = self.settings.host_server(&alias);
        if !explicit && (servers.is_none() || choice == Some(Choice::Tmux)) {
            return;
        }
        let Some(connection) = self.conns.get(&alias).map(|c| c.connection.clone()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let probed = runtime::spawn(async move {
                berth_core::remote::probe(connection.as_ref(), None).await
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                match probed {
                    Ok(Ok(probe)) => {
                        let manifest = servers.map(|s| &s.manifest);
                        let next = berth_core::remote::decide(&probe, manifest, choice, asking);
                        this.server_decided(alias, next, explicit, cx);
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%alias, %error, "could not look at the host for its server");
                        if explicit {
                            this.warnings
                                .push(strings::server_probe_failed(&alias, &error.to_string()));
                        }
                    }
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn server_decided(
        &mut self,
        alias: String,
        next: Next,
        explicit: bool,
        cx: &mut Context<Self>,
    ) {
        match next {
            Next::Ask(plan) => {
                // One question at a time; this one is asked at the next connect.
                if self.modal.is_none() {
                    self.modal = Some(Modal::Server {
                        alias,
                        plan: Box::new(plan),
                    });
                    self.modal_focused = false;
                }
            }
            Next::Update {
                installed, plan, ..
            } => {
                tracing::info!(%alias, ?installed, offered = %plan.build, "the host runs another server build");
                let running = installed.last().cloned().unwrap_or_default();
                self.server_updates.insert(alias, (running, plan.build));
            }
            Next::Use(plan) => {
                self.server_updates.remove(&alias);
                if explicit {
                    self.warnings
                        .push(strings::server_in_use(&alias, &plan.build));
                }
                self.reach_host_server(alias, plan, cx);
            }
            Next::Tmux(stays) => {
                tracing::info!(%alias, ?stays, "the host stays on tmux");
                let said = match stays {
                    Stays::NoBuildFor { os, arch } => {
                        Some(strings::server_none_for(&alias, &os, &arch))
                    }
                    Stays::NothingBundled => Some(strings::SERVER_NOT_CARRIED.to_string()),
                    Stays::Unusable(install::Unusable::Path(path)) => {
                        Some(strings::server_path_unusable(&alias, &path))
                    }
                    Stays::Chosen | Stays::NotNow => None,
                };
                if let (true, Some(said)) = (explicit, said) {
                    self.warnings.push(said);
                }
            }
        }
    }

    /// The user asked about a host's server from its menu: whatever was
    /// chosen before is forgotten, and the host is looked at again.
    pub(super) fn ask_server(&mut self, alias: String, cx: &mut Context<Self>) {
        if self.settings.set_host_server(&alias, None) {
            self.settings.save();
        }
        self.check_server(alias, true, true, cx);
    }

    /// "Keep using tmux": remembered, and not asked again.
    pub(super) fn decline_server(&mut self, alias: &str) {
        tracing::info!(%alias, "the server was declined; the host stays on tmux");
        if self.settings.set_host_server(alias, Some(Choice::Tmux)) {
            self.settings.save();
        }
    }

    /// "Install": what the dialog showed is what is installed.
    pub(super) fn install_server(&mut self, alias: String, plan: Plan, cx: &mut Context<Self>) {
        let Some(connection) = self.conns.get(&alias).map(|c| c.connection.clone()) else {
            self.warnings.push(strings::server_install_failed(
                &alias,
                strings::STATE_DISCONNECTED,
            ));
            return;
        };
        let Some(servers) = crate::servers::carried() else {
            return;
        };
        tracing::info!(%alias, path = %plan.path, build = %plan.build, "installing the server");
        cx.spawn(async move |this, cx| {
            let dir = plan.dir.clone();
            let result = runtime::spawn(async move {
                let payload = servers
                    .payload(plan.target, plan.gzip)
                    .map_err(|e| Failed::Carried(e.to_string()))?;
                berth_core::remote::install(connection.as_ref(), &plan, &payload)
                    .await
                    .map_err(Failed::Install)
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(Installed { hash_checked })) => {
                        tracing::info!(%alias, hash_checked, "the server is installed");
                        if this.settings.set_host_server(&alias, Some(Choice::Install)) {
                            this.settings.save();
                        }
                        this.server_updates.remove(&alias);
                        this.warnings
                            .push(strings::server_installed(&alias, hash_checked));
                        // Looked at again, as a connect would: the build
                        // just installed is the one used from here on.
                        this.check_server(alias.clone(), false, false, cx);
                    }
                    // Not something asking again would change: the host
                    // stays on tmux until the user brings it up themselves.
                    Ok(Err(Failed::Install(install::Error::Noexec { .. }))) => {
                        tracing::warn!(%alias, %dir, "the host runs nothing from its home");
                        this.decline_server(&alias);
                        this.warnings.push(strings::server_noexec(&alias, &dir));
                    }
                    Ok(Err(Failed::Install(error))) => {
                        tracing::warn!(%alias, %error, "the server was not installed");
                        this.warnings
                            .push(strings::server_install_failed(&alias, &error.to_string()));
                    }
                    Ok(Err(Failed::Carried(error))) => {
                        tracing::error!(%alias, %error, "the server this app carries is not usable");
                        this.warnings
                            .push(strings::server_install_failed(&alias, &error));
                    }
                    Err(_) => {
                        this.warnings
                            .push(strings::server_install_failed(&alias, strings::RUNTIME_DROPPED));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Reach a host's server, installed and of this app's build, so new
    /// sessions there go to it (D55). Once per connection; nothing is
    /// installed or started here but the supervisor the relay starts.
    pub(super) fn reach_host_server(&mut self, alias: String, plan: Plan, cx: &mut Context<Self>) {
        let Some(connection) = self
            .conns
            .get(&alias)
            .filter(|c| c.server.is_none())
            .map(|c| c.connection.clone())
        else {
            return;
        };
        if !self.server_starting.insert(alias.clone()) {
            return;
        }
        tracing::info!(%alias, build = %plan.build, "reaching the host's server");
        cx.spawn(async move |this, cx| {
            let reached = runtime::spawn(async move { session::host_server(connection, &plan).await })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.server_starting.remove(&alias);
                match reached {
                    Ok(Ok(held)) => this.hold_host_server(alias, held, cx),
                    Ok(Err(error)) => {
                        tracing::warn!(%alias, %error, "the host's server did not answer; staying on tmux");
                    }
                    Err(_) => {}
                }
            });
        })
        .detach();
    }

    /// Keep a host's server for as long as its connection: its session list
    /// pushed into the workspace's, by host.
    pub(super) fn hold_host_server(
        &mut self,
        alias: String,
        held: session::HostServer,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.conns.get_mut(&alias) else {
            // Closed meanwhile: dropping the handle lets it go.
            return;
        };
        let lists = self.session_lists.clone();
        let machine = Machine::Host(alias.clone());
        held.server.on_sessions(move |list| {
            let _ = lists.send((machine.clone(), list));
        });
        let server = held.server.clone();
        conn.server = Some(held);
        tracing::info!(%alias, "using the host's server");
        let lists = self.session_lists.clone();
        cx.background_executor()
            .spawn(async move {
                if let Ok(Ok(list)) = runtime::spawn(async move { server.list().await }).await {
                    let _ = lists.send((Machine::Host(alias), list));
                }
            })
            .detach();
        cx.notify();
    }
}
