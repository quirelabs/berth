//! Protocol version 1. Frozen once released: nothing here is renamed,
//! reordered or given a new field. A change is a new version module, and this
//! one stays for as long as a worker built with it can be running.
//!
//! postcard encodes an enum as its variant's position, so the order of every
//! enum below is part of the format. New messages go at the end of the next
//! version, never in the middle of this one.

use serde::{Deserialize, Serialize};

use crate::{Direction, Error, Packed};

pub const VERSION: u16 = 1;

/// A session, as the server numbers it. Never reused while the server runs.
pub type SessionId = u64;

/// App to server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToServer {
    /// Answered with [`ToApp::Sessions`].
    ListSessions,
    /// Start a session: a new shell. Answered with [`ToApp::Created`].
    Create {
        name: Option<String>,
        cwd: Option<String>,
        cols: u16,
        rows: u16,
    },
    /// Show a session: a [`ToApp::Screen`] first, then its output from there.
    Attach {
        session: SessionId,
        cols: u16,
        rows: u16,
    },
    /// Stop showing a session. It keeps running.
    Detach { session: SessionId },
    /// End a session. The app sends this only after the user confirmed (D32).
    Kill { session: SessionId },
    /// Bytes typed or pasted, as the terminal encoded them.
    Input { session: SessionId, bytes: Vec<u8> },
    Resize {
        session: SessionId,
        cols: u16,
        rows: u16,
    },
    /// The app has taken output up to `seq`. How the server knows it has
    /// fallen behind, and switches to screens (D49).
    Confirmed { session: SessionId, seq: u64 },
    /// The app's window was hidden or shown. While hidden the server sends
    /// nothing; on showing, one screen per attached session (D50).
    Visible { visible: bool },
}

/// Server to app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToApp {
    Sessions(Vec<SessionInfo>),
    Created {
        session: SessionId,
    },
    /// Raw output, in order. `seq` counts bytes of output since the session
    /// began, at the end of this chunk.
    Output {
        session: SessionId,
        seq: u64,
        bytes: Vec<u8>,
    },
    /// The whole visible screen as of output `seq`, a packed [`Screen`]. Sent
    /// on attach, when the app has fallen behind, and when a program leaves
    /// the alternate screen.
    Screen {
        session: SessionId,
        seq: u64,
        screen: Packed,
    },
    /// History lines, a packed `Vec<Row>`, newest first: line `newest`, then
    /// `newest - 1`, and so on. Numbers grow with the output, a line keeps its
    /// number while the server holds it, and none is used twice in a session,
    /// so ranges sent at different times fit together. They can run behind
    /// the count of lines produced. Sent on its own, lower-priority stream
    /// (D49).
    History {
        session: SessionId,
        newest: u64,
        lines: Packed,
    },
    Event {
        session: SessionId,
        event: Event,
    },
    /// A request that could not be done, named, with why.
    Refused {
        what: String,
        why: String,
    },
}

/// A session in the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub name: String,
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub claude: Option<Claude>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Title(String),
    Bell,
    /// The shell exited; the session is gone.
    Exited {
        code: Option<i32>,
    },
    Claude(Claude),
}

/// A session's Claude, as its hook reported it (D48).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claude {
    pub state: ClaudeState,
    /// Claude's own session id, for exact-id resume.
    pub session_id: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaudeState {
    Working,
    /// Waiting on a permission prompt.
    Permission,
    /// Idle at the prompt for a while.
    Idle,
    /// The turn ended; the next message is the user's.
    YourMove,
    /// Exited after a finished turn.
    Finished,
    /// Exited in the middle of a turn.
    Interrupted,
}

/// The visible screen: what [`ToApp::Screen`] unpacks to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screen {
    pub cols: u16,
    pub rows: u16,
    pub cursor: Cursor,
    pub modes: Modes,
    /// Every row of the grid, top to bottom.
    pub lines: Vec<Row>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub line: u16,
    pub column: u16,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modes {
    pub alternate_screen: bool,
    pub application_cursor: bool,
    pub mouse_reporting: bool,
    pub bracketed_paste: bool,
    pub alternate_scroll: bool,
}

/// One row, every column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub cells: Vec<Cell>,
    /// The row continues on the next one: a soft wrap, not a newline. Copy
    /// and reflow need it.
    pub wrapped: bool,
}

/// One cell. The second column of a wide glyph is a cell with `width` 0 and
/// no text, so column arithmetic never needs the glyph's width.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    /// The grapheme: one character, or a base and its combining marks. Empty
    /// for a blank or a wide glyph's second half.
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    /// [`style`] bits.
    pub style: u16,
    /// Columns the glyph covers: 1, 2 for wide, 0 for a wide glyph's second
    /// half.
    pub width: u8,
}

/// Style bits of a [`Cell`]. Unused bits are sent as zero and ignored.
pub mod style {
    pub const BOLD: u16 = 1 << 0;
    pub const ITALIC: u16 = 1 << 1;
    pub const UNDERLINE: u16 = 1 << 2;
    pub const STRIKETHROUGH: u16 = 1 << 3;
    pub const INVERSE: u16 = 1 << 4;
    pub const FAINT: u16 = 1 << 5;
    pub const BLINK: u16 = 1 << 6;
    pub const INVISIBLE: u16 = 1 << 7;
}

/// A colour as the program set it, not resolved, so the app's own palette
/// applies (D49).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Color {
    /// The terminal's default foreground or background.
    Default,
    Palette(u8),
    Rgb(u8, u8, u8),
}

/// How many variants each message enum has. The frozen-bytes test checks
/// these against the enums, so a variant added here without a new version
/// fails the build's tests.
pub const TO_SERVER_VARIANTS: u32 = 9;
pub const TO_APP_VARIANTS: u32 = 7;

impl ToServer {
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        crate::encode(self, "v1 message to the server")
    }

    pub fn decode(payload: &[u8]) -> Result<Self, Error> {
        known(payload, Direction::ToServer, TO_SERVER_VARIANTS)?;
        crate::decode_exact(payload, "v1 message to the server")
    }
}

impl ToApp {
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        crate::encode(self, "v1 message to the app")
    }

    pub fn decode(payload: &[u8]) -> Result<Self, Error> {
        known(payload, Direction::ToApp, TO_APP_VARIANTS)?;
        crate::decode_exact(payload, "v1 message to the app")
    }
}

/// Name a message this build does not know before trying to read it, so the
/// error says what arrived rather than how the bytes failed to parse.
fn known(payload: &[u8], direction: Direction, variants: u32) -> Result<(), Error> {
    let (tag, _) = postcard::take_from_bytes::<u32>(payload).map_err(|e| Error::Malformed {
        what: "message tag",
        detail: e.to_string(),
    })?;
    if tag >= variants {
        return Err(Error::UnknownMessage {
            version: VERSION,
            direction,
            tag,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_from_a_newer_build_is_refused_by_name() {
        // Tag 9, one past the last v1 message to the server.
        assert_eq!(
            ToServer::decode(&[9]),
            Err(Error::UnknownMessage {
                version: 1,
                direction: Direction::ToServer,
                tag: 9,
            })
        );
        assert_eq!(
            ToApp::decode(&[200, 1]).map_err(|e| e.to_string()),
            Err("unknown version 1 message to the app, tag 200".to_string())
        );
    }

    #[test]
    fn a_truncated_message_is_malformed() {
        let bytes = ToServer::Input {
            session: 3,
            bytes: b"ls\r".to_vec(),
        }
        .encode()
        .unwrap();
        for cut in 0..bytes.len() {
            assert!(
                matches!(
                    ToServer::decode(&bytes[..cut]),
                    Err(Error::Malformed { .. })
                ),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn a_screen_travels_packed() {
        let screen = Screen {
            cols: 80,
            rows: 24,
            cursor: Cursor {
                line: 0,
                column: 2,
                visible: true,
            },
            modes: Modes::default(),
            lines: vec![
                Row {
                    cells: vec![
                        Cell {
                            text: " ".into(),
                            fg: Color::Default,
                            bg: Color::Default,
                            style: 0,
                            width: 1,
                        };
                        80
                    ],
                    wrapped: false,
                };
                24
            ],
        };
        let message = ToApp::Screen {
            session: 1,
            seq: 42,
            screen: Packed::pack(&screen).unwrap(),
        };
        let bytes = message.encode().unwrap();
        assert!(bytes.len() < 200, "a blank screen is {} bytes", bytes.len());
        let ToApp::Screen { screen: packed, .. } = ToApp::decode(&bytes).unwrap() else {
            panic!("not a screen");
        };
        assert_eq!(packed.unpack::<Screen>().unwrap(), screen);
    }
}
