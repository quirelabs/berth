//! The app's side of the local socket: connect to the supervisor, and trade
//! messages over two channels.

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
    pub to_server: mpsc::Sender<ToServer>,
    /// Closes when the connection does.
    pub from_server: mpsc::Receiver<ToApp>,
}

/// Connect to the supervisor at `places`. It must already be running.
pub async fn connect(places: &Places) -> Result<Client> {
    let stream = super::transport::connect(&places.supervisor())
        .await
        .map_err(|e| Error::Server(format!("no supervisor answers: {e}")))?;
    let opened = link::open(stream, &link::hello()).await?;

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
    tokio::spawn(async move {
        while let Ok(Some(payload)) = read.recv().await {
            match ToApp::decode(&payload) {
                Ok(message) => {
                    if inbox.send(message).await.is_err() {
                        break;
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "the supervisor sent something unreadable");
                    break;
                }
            }
        }
    });

    Ok(Client {
        agreed: opened.agreed,
        server_build: opened.theirs.build,
        to_server,
        from_server,
    })
}
