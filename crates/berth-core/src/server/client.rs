//! The app's side of the local socket: connect to the supervisor, and trade
//! messages over two channels.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use berth_proto::Agreed;
use berth_proto::v1::{ToApp, ToServer};
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
        while let Ok(Some(payload)) = read.recv().await {
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
