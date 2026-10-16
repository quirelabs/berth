//! Putting Berth's server on a remote host (D49 step 3, D55).
//!
//! Three steps, each its own module, and none of them decides anything on
//! the user's behalf: [`manifest`] is what this app carries and what each
//! binary must hash to, [`probe`] asks a host what it is without changing
//! it, and [`install`] puts one build there once the user has agreed.
//! Whether to ask, install or stay on tmux is [`decide`], which is pure.
//!
//! Everything runs over exec channels: no login shell, no SFTP.

pub mod install;
pub mod manifest;
pub mod probe;

use std::future::Future;

use crate::error::Result;
use crate::transport::Output;

pub use install::{Plan, install};
pub use manifest::{Manifest, Servers, Target};
pub use probe::{Probe, probe};

/// Something commands can be run on: a host over SSH, or this machine.
pub trait Runs {
    /// Run `command` with `stdin`, and collect what it produced.
    fn run(&self, command: &str, stdin: &[u8]) -> impl Future<Output = Result<Output>> + Send;
}

impl Runs for crate::transport::Connection {
    async fn run(&self, command: &str, stdin: &[u8]) -> Result<Output> {
        self.exec_with_stdin(command, stdin).await
    }
}

/// This machine, through `sh -c`, as an exec channel would run a command.
/// What the scripts are tested against before they meet a host.
#[cfg(unix)]
pub struct ThisMachine;

#[cfg(unix)]
impl Runs for ThisMachine {
    async fn run(&self, command: &str, stdin: &[u8]) -> Result<Output> {
        use tokio::io::AsyncWriteExt;
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", command])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut pipe) = child.stdin.take() {
            // A command that reads nothing closes its end; that is its answer.
            let _ = pipe.write_all(stdin).await;
        }
        let out = child.wait_with_output().await?;
        Ok(Output {
            stdout: out.stdout,
            stderr: out.stderr,
            exit_code: out.status.code().and_then(|c| u32::try_from(c).ok()),
        })
    }
}

/// What the user chose for a host, remembered (D55).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Choice {
    /// Run Berth's server there.
    Install,
    /// Keep using tmux.
    Tmux,
}

/// Why a host stays on tmux without being asked anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stays {
    /// The user chose tmux for this host.
    Chosen,
    /// This app carries no server for what the host is.
    NoBuildFor { os: String, arch: String },
    /// This app carries no servers at all.
    NothingBundled,
    /// The host cannot be installed on as it is, and why.
    Unusable(install::Unusable),
    /// It would need the user's word, and nobody is there to give it: a
    /// reconnect in the background installs nothing (D55).
    NotNow,
}

/// What to do about a host's server, from what it is and what was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// The build this app carries is there: use it.
    Use(Plan),
    /// Nothing of Berth's is there and nothing was chosen, or what was
    /// installed has been removed since: ask, showing this.
    Ask(Plan),
    /// Another build is there. It is used as it is; this one is offered,
    /// never installed unasked. `running` is how many sessions it holds.
    Update {
        installed: Vec<String>,
        plan: Plan,
        running: usize,
    },
    /// tmux, as before.
    Tmux(Stays),
}

/// Decide. `asking` is whether the user is there to be asked: a connect they
/// started, not a reconnect in the background.
pub fn decide(
    probe: &Probe,
    servers: Option<&Manifest>,
    choice: Option<Choice>,
    asking: bool,
) -> Next {
    if choice == Some(Choice::Tmux) {
        return Next::Tmux(Stays::Chosen);
    }
    let Some(manifest) = servers else {
        return Next::Tmux(Stays::NothingBundled);
    };
    let entry = probe.target.and_then(|target| manifest.entry(target));
    let Some(entry) = entry else {
        return Next::Tmux(Stays::NoBuildFor {
            os: probe.os.clone(),
            arch: probe.arch.clone(),
        });
    };
    let plan = match Plan::new(probe, entry) {
        Ok(plan) => plan,
        Err(why) => return Next::Tmux(Stays::Unusable(why)),
    };
    if probe.builds.contains(&plan.build) {
        return Next::Use(plan);
    }
    if !probe.builds.is_empty() {
        return Next::Update {
            installed: probe.builds.clone(),
            plan,
            running: probe.sessions,
        };
    }
    // Nothing there. A remembered "install" with nothing installed means it
    // was removed on the host, so the choice is asked for again.
    if asking {
        Next::Ask(plan)
    } else {
        Next::Tmux(Stays::NotNow)
    }
}

/// Single-quote `text` for the command line an exec channel hands to the
/// user's shell. Callers have already refused text with a quote in it
/// ([`install::Unusable::Path`]): the shells in use do not agree on how one is
/// escaped.
pub(crate) fn quoted(text: &str) -> String {
    format!("'{text}'")
}

#[cfg(test)]
mod tests {
    use super::manifest::tests::manifest;
    use super::*;

    fn linux() -> Probe {
        Probe {
            os: "Linux".into(),
            arch: "x86_64".into(),
            target: Some(Target::LinuxX86_64),
            home: "/home/u".into(),
            data: "/home/u/.local/share/berth".into(),
            builds: Vec::new(),
            hash_tool: Some(probe::HashTool::Sha256sum),
            gzip: true,
            kills_at_logout: Some(false),
            linger: Some(false),
            sessions: 0,
        }
    }

    #[test]
    fn a_host_with_nothing_is_asked_about_once_someone_is_there() {
        let manifest = manifest();
        let next = decide(&linux(), Some(&manifest), None, true);
        assert!(matches!(next, Next::Ask(_)), "{next:?}");
        // In the background nothing is asked and nothing is installed.
        assert_eq!(
            decide(&linux(), Some(&manifest), None, false),
            Next::Tmux(Stays::NotNow)
        );
        assert_eq!(
            decide(&linux(), Some(&manifest), Some(Choice::Install), false),
            Next::Tmux(Stays::NotNow)
        );
    }

    #[test]
    fn tmux_once_chosen_is_not_asked_about_again() {
        let manifest = manifest();
        assert_eq!(
            decide(&linux(), Some(&manifest), Some(Choice::Tmux), true),
            Next::Tmux(Stays::Chosen)
        );
    }

    #[test]
    fn the_build_this_app_carries_is_used_when_it_is_there() {
        let manifest = manifest();
        let mut probe = linux();
        let build = manifest.entry(Target::LinuxX86_64).unwrap().build();
        probe.builds = vec!["0.0.9+0000000000000000".into(), build.clone()];
        for asking in [true, false] {
            let next = decide(&probe, Some(&manifest), Some(Choice::Install), asking);
            assert!(
                matches!(next, Next::Use(ref plan) if plan.build == build),
                "{next:?}"
            );
        }
    }

    /// Another build is an offer, in the background too: it is shown on the
    /// host row and installs nothing by itself.
    #[test]
    fn another_build_is_an_update_to_offer_never_to_make() {
        let manifest = manifest();
        let mut probe = linux();
        probe.builds = vec!["0.0.9+0000000000000000".into()];
        probe.sessions = 2;
        for asking in [true, false] {
            let next = decide(&probe, Some(&manifest), Some(Choice::Install), asking);
            assert!(
                matches!(next, Next::Update { running: 2, ref installed, .. } if installed.len() == 1),
                "{next:?}"
            );
        }
    }

    /// Installed once and since removed on the host: the remembered choice no
    /// longer stands, and the user is asked again.
    #[test]
    fn a_removed_install_is_asked_about_again() {
        let manifest = manifest();
        let next = decide(&linux(), Some(&manifest), Some(Choice::Install), true);
        assert!(matches!(next, Next::Ask(_)), "{next:?}");
    }

    #[test]
    fn a_host_this_app_has_no_build_for_stays_on_tmux_and_says_what_it_is() {
        let manifest = manifest();
        let mut probe = linux();
        probe.os = "FreeBSD".into();
        probe.target = None;
        assert_eq!(
            decide(&probe, Some(&manifest), None, true),
            Next::Tmux(Stays::NoBuildFor {
                os: "FreeBSD".into(),
                arch: "x86_64".into()
            })
        );
        assert_eq!(
            decide(&linux(), None, None, true),
            Next::Tmux(Stays::NothingBundled)
        );
    }
}
