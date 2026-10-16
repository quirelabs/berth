//! Deciding whether to trust a host key, and recording the answer.
//!
//! Small and kept together on purpose. This is the only place in the app that
//! writes to known_hosts, and D30 turns on the probed key being the recorded
//! key, so the four steps that decide it should be readable in one screen.

use super::*;

impl Workspace {
    /// Ask a host for its key and put the fingerprint in front of the user.
    pub(super) fn verify(&mut self, host: Host, cx: &mut Context<Self>) {
        let alias = host.alias.clone();
        let endpoint = format!("{}:{}", host.hostname, host.port);
        tracing::info!(%alias, "asking the host for its key");
        self.statuses.insert(alias.clone(), HostStatus::Verifying);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let offer = runtime::spawn(session::scout(host)).await;
            let _ = this.update(cx, |this, cx| {
                match offer {
                    Ok(Ok(session::Offer::Unknown { key, fingerprint })) => {
                        this.statuses.insert(alias.clone(), HostStatus::NeedsVerify);
                        this.modal = Some(Modal::Verify(Prompt {
                            alias,
                            endpoint,
                            fingerprint,
                            key,
                        }));
                        this.modal_focused = false;
                    }
                    // Recorded between startup and the click, so there is
                    // nothing to ask and the click meant "open this".
                    Ok(Ok(session::Offer::Trusted)) => {
                        if let Some(host) = this.hosts.iter().find(|h| h.alias == alias).cloned() {
                            this.statuses
                                .insert(alias, HostStatus::Connection(ConnectionState::Connecting));
                            this.connect(host, None, true, cx);
                        }
                    }
                    // Never offered as a question. There is nothing here a user
                    // can usefully be asked to accept.
                    Ok(Ok(session::Offer::Changed { line, fingerprint })) => {
                        tracing::error!(%alias, %line, %fingerprint, "host key has changed");
                        this.statuses.insert(
                            alias,
                            HostStatus::Failed(strings::changed_key(line, &fingerprint)),
                        );
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%alias, %error, "could not reach the host to see its key");
                        this.statuses
                            .insert(alias, HostStatus::Failed(error.to_string()));
                    }
                    Err(_) => {
                        this.statuses.insert(
                            alias,
                            HostStatus::Failed(strings::RUNTIME_DROPPED.to_string()),
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Say yes to whatever is being asked.
    pub(super) fn confirm(&mut self, cx: &mut Context<Self>) {
        match self.modal.take() {
            Some(Modal::Verify(prompt)) => self.accept(prompt, cx),
            Some(Modal::Kill {
                pane, alias, slug, ..
            }) => self.kill(alias, slug, pane, cx),
            Some(Modal::Rename { pane, field }) => self.rename(pane, field.value().to_string(), cx),
            Some(Modal::RenameHost { alias, field }) => self.rename_host(&alias, field.value(), cx),
            Some(Modal::Server { alias, plan }) => self.install_server(alias, *plan, cx),
            None => {}
        }
        cx.notify();
    }

    /// The dialog's no button. Only the server question tells it apart from
    /// Escape: "Keep using tmux" is an answer, and is remembered as one.
    pub(super) fn reject(&mut self, cx: &mut Context<Self>) {
        match self.modal.take() {
            Some(Modal::Server { alias, .. }) => {
                self.decline_server(&alias);
                cx.notify();
            }
            modal => {
                self.modal = modal;
                self.dismiss(cx);
            }
        }
    }

    /// Say no. Nothing is written, nothing is ended.
    pub(super) fn dismiss(&mut self, cx: &mut Context<Self>) {
        match self.modal.take() {
            Some(Modal::Verify(prompt)) => {
                tracing::info!(alias = %prompt.alias, "host key declined");
                self.statuses.insert(prompt.alias, HostStatus::NeedsVerify);
            }
            Some(Modal::Kill { alias, .. }) => {
                tracing::info!(%alias, "kill cancelled");
            }
            Some(Modal::Rename { .. } | Modal::RenameHost { .. }) => {}
            // Escape is "not now": nothing is remembered, and the next
            // connect the user makes, or the menu item, asks again.
            Some(Modal::Server { alias, .. }) => {
                tracing::info!(%alias, "the server question was put off");
            }
            None => {}
        }
        cx.notify();
    }

    /// Record the key the user accepted, then open the host.
    fn accept(&mut self, prompt: Prompt, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.iter().find(|h| h.alias == prompt.alias).cloned() else {
            return;
        };
        match session::trust(&host, &prompt.key) {
            Ok(()) => {
                tracing::info!(
                    alias = %prompt.alias,
                    fingerprint = %prompt.fingerprint,
                    "host key accepted and recorded"
                );
                self.statuses.insert(
                    prompt.alias,
                    HostStatus::Connection(ConnectionState::Connecting),
                );
                self.connect(host, None, true, cx);
            }
            Err(error) => {
                // Nothing was written, so the host stays a question. Connecting
                // anyway would be trusting a key that was never recorded, and
                // would ask again on the next launch with no explanation.
                tracing::error!(%error, alias = %prompt.alias, "could not record the accepted key");
                self.warnings.push(error.to_string());
                self.statuses.insert(prompt.alias, HostStatus::NeedsVerify);
            }
        }
    }
}
