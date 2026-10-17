//! The app's side of the local socket: connect to the supervisor, and trade
//! messages over two channels.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use berth_proto::v1::{ToApp, ToServer};
use berth_proto::{Agreed, core};
use tokio::sync::mpsc;

use super::link;
use super::paths::Places;
use crate::error::{Error, Result};

const QUEUE: usize = 1024;

pub struct Client {
    pub agreed: Agreed,
    /// The supervisor's build, as it greeted.
    pub server_build: String,
    /// Set when that is not the build expected of it. The connection is used
    /// all the same (D54).
    pub other_build: Option<link::OtherBuild>,
    /// Whether the connection ended on a message this build could not read.
    pub unreadable: Arc<AtomicBool>,
    pub to_server: mpsc::Sender<ToServer>,
    /// Closes when the connection does.
    pub from_server: mpsc::Receiver<ToApp>,
}

/// Connect to the supervisor at `places`. It must already be running, and is
/// expected to be this very build.
pub async fn connect(places: &Places) -> Result<Client> {
    connect_expecting(places, &super::build()).await
}

/// Connect to the supervisor at `places`, which should be the build
/// `expected`: the one this side would have started or installed.
pub async fn connect_expecting(places: &Places, expected: &str) -> Result<Client> {
    places.trusted()?;
    let stream = super::transport::connect(&places.supervisor())
        .await
        .map_err(|e| Error::Server(format!("no supervisor answers: {e}")))?;
    let opened = link::open(stream, &link::hello()).await?;
    let other_build = opened.other_build(expected);
    if let Some(other) = &other_build {
        tracing::info!(theirs = %other.theirs, expected = %other.expected, "the supervisor is another build");
    }
    let unreadable = Arc::new(AtomicBool::new(false));

    let (to_server, mut outbox) = mpsc::channel::<ToServer>(QUEUE);
    let mut write = opened.write;
    tokio::spawn(async move {
        while let Some(message) = outbox.recv().await {
            let Ok(payload) = message.encode() else {
                continue;
            };
            if write.send(&payload).await.is_err() {
                break;
            }
        }
    });

    let (inbox, from_server) = mpsc::channel::<ToApp>(QUEUE);
    let mut read = opened.read;
    let (other, failed) = (other_build.clone(), unreadable.clone());
    tokio::spawn(async move {
        loop {
            // A client dropped by its app closes the connection now, not at
            // the next message: the supervisor counts it gone, and can go
            // idle (D50).
            let received = tokio::select! {
                received = read.recv() => received,
                _ = inbox.closed() => break,
            };
            let Ok(Some(payload)) = received else { break };
            match ToApp::decode(&payload) {
                Ok(message) => {
                    if inbox.send(message).await.is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let error = link::unreadable(error, other.as_ref());
                    tracing::warn!(%error, "the supervisor sent something unreadable");
                    failed.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    });

    Ok(Client {
        agreed: opened.agreed,
        server_build: opened.theirs.build,
        other_build,
        unreadable,
        to_server,
        from_server,
    })
}

/// How long a supervisor has to answer the core.
const CORE_WITHIN: std::time::Duration = std::time::Duration::from_secs(5);

/// Ask the supervisor at `places` one of the things every build answers
/// (D54, D55), on a connection of its own: which sessions it holds, or to
/// end one. It works whatever else this build and that one cannot read of
/// each other, and whatever the main connection is doing.
pub async fn core(places: &Places, ask: core::Ask) -> Result<Vec<core::Session>> {
    places.trusted()?;
    let stream = super::transport::connect(&places.supervisor())
        .await
        .map_err(|e| Error::Server(format!("no supervisor answers: {e}")))?;
    core_over(stream, ask).await
}

/// The same over any stream to a supervisor.
pub async fn core_over<S>(stream: S, ask: core::Ask) -> Result<Vec<core::Session>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite,
{
    let mut opened = link::open(stream, &link::hello()).await?;
    opened
        .write
        .send(&ask.encode().map_err(link::protocol)?)
        .await?;
    let answered = async {
        // Whatever of a version arrives meanwhile is not for this.
        loop {
            match opened.read.recv().await? {
                Some(payload) if core::is_core(&payload) => {
                    let core::Answer::Sessions(list) =
                        core::Answer::decode(&payload).map_err(link::protocol)?;
                    return Ok(list);
                }
                Some(_) => continue,
                None => return Err(Error::Server("closed before answering the core".into())),
            }
        }
    };
    tokio::time::timeout(CORE_WITHIN, answered)
        .await
        .map_err(|_| Error::Server("no answer to the core in time".into()))?
}
