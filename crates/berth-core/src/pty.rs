//! An interactive PTY channel.
//!
//! The channel is owned by one task. Callers talk to it over channels, which
//! keeps the SSH side off the UI thread entirely: the UI sends keystrokes and
//! resizes in, and drains output events out.

use russh::{Channel, ChannelMsg, client::Msg};
use tokio::sync::mpsc;

use crate::error::Result;
use crate::session::TERM;

/// How many output events may queue before the reader applies backpressure.
/// Output arrives far faster than a UI can paint during something like a large
/// `cat`, so this bounds memory rather than trying to keep every wakeup.
const OUTPUT_QUEUE: usize = 256;
const INPUT_QUEUE: usize = 256;

/// Something that happened on the remote side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyEvent {
    /// Raw bytes from the remote. Feed straight to the parser.
    Output(Vec<u8>),
    /// The remote command exited.
    Exit(u32),
    /// The channel closed, for any reason.
    Closed,
}

/// Something the UI wants to do to the remote side.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PtyInput {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

/// The UI-side handle. Cloneable and cheap; dropping every clone closes the
/// channel.
#[derive(Debug, Clone)]
pub struct Pty {
    input: mpsc::Sender<PtyInput>,
}

impl Pty {
    /// Send bytes as if typed. Returns false once the channel has closed.
    pub async fn write(&self, data: impl Into<Vec<u8>>) -> bool {
        self.input.send(PtyInput::Data(data.into())).await.is_ok()
    }

    /// Tell the remote the window changed. tmux repaints from its own state.
    pub async fn resize(&self, cols: u16, rows: u16) -> bool {
        self.input
            .send(PtyInput::Resize {
                cols: cols.max(1),
                rows: rows.max(1),
            })
            .await
            .is_ok()
    }

    pub async fn close(&self) -> bool {
        self.input.send(PtyInput::Close).await.is_ok()
    }
}

/// Open a PTY channel running `command`, or the login shell when `None`.
///
/// Returns the handle and the stream of events. The pumping task ends when the
/// remote closes the channel or every handle is dropped.
pub(crate) async fn open(
    mut channel: Channel<Msg>,
    command: Option<&str>,
    cols: u16,
    rows: u16,
) -> Result<(Pty, mpsc::Receiver<PtyEvent>)> {
    channel
        .request_pty(
            true,
            TERM,
            cols.max(1) as u32,
            rows.max(1) as u32,
            0,
            0,
            &[],
        )
        .await?;

    match command {
        Some(command) => channel.exec(true, command).await?,
        None => channel.request_shell(true).await?,
    }

    let (input_tx, mut input_rx) = mpsc::channel::<PtyInput>(INPUT_QUEUE);
    let (output_tx, output_rx) = mpsc::channel::<PtyEvent>(OUTPUT_QUEUE);

    tokio::spawn(async move {
        loop {
            tokio::select! {
                // Bias towards draining the remote so a slow UI cannot make
                // the SSH window stall.
                biased;

                message = channel.wait() => {
                    let Some(message) = message else { break };
                    match message {
                        ChannelMsg::Data { data } => {
                            if output_tx.send(PtyEvent::Output(data.to_vec())).await.is_err() {
                                break;
                            }
                        }
                        // A PTY merges stderr into the stream, but a server may
                        // still send extended data; it is still terminal output.
                        ChannelMsg::ExtendedData { data, .. } => {
                            if output_tx.send(PtyEvent::Output(data.to_vec())).await.is_err() {
                                break;
                            }
                        }
                        ChannelMsg::ExitStatus { exit_status } => {
                            let _ = output_tx.send(PtyEvent::Exit(exit_status)).await;
                        }
                        _ => {}
                    }
                }

                request = input_rx.recv() => {
                    match request {
                        Some(PtyInput::Data(bytes)) => {
                            if channel.data(bytes.as_slice()).await.is_err() {
                                break;
                            }
                        }
                        Some(PtyInput::Resize { cols, rows }) => {
                            if channel
                                .window_change(cols as u32, rows as u32, 0, 0)
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Some(PtyInput::Close) | None => break,
                    }
                }
            }
        }
        let _ = output_tx.send(PtyEvent::Closed).await;
    });

    Ok((Pty { input: input_tx }, output_rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_closed_channel_reports_write_failure() {
        let (tx, rx) = mpsc::channel::<PtyInput>(1);
        let pty = Pty { input: tx };
        drop(rx);
        assert!(!pty.write("x").await);
        assert!(!pty.resize(80, 24).await);
    }

    #[tokio::test]
    async fn resize_never_sends_a_zero_dimension() {
        let (tx, mut rx) = mpsc::channel::<PtyInput>(4);
        let pty = Pty { input: tx };
        assert!(pty.resize(0, 0).await);
        assert_eq!(rx.recv().await, Some(PtyInput::Resize { cols: 1, rows: 1 }));
    }

    #[tokio::test]
    async fn writes_arrive_in_order() {
        let (tx, mut rx) = mpsc::channel::<PtyInput>(8);
        let pty = Pty { input: tx };
        pty.write("a").await;
        pty.write("b").await;
        assert_eq!(rx.recv().await, Some(PtyInput::Data(b"a".to_vec())));
        assert_eq!(rx.recv().await, Some(PtyInput::Data(b"b".to_vec())));
    }
}
