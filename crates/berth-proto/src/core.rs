//! The core: the little that every build answers, for good (D54, D55).
//!
//! A protocol version can be retired, and before the first release v1 itself
//! is a draft that two builds may not share. Whatever else two builds cannot
//! read of each other, an app can always ask a server which sessions it
//! holds and end one: nobody is left with sessions running on a host that no
//! app they have can see or stop.
//!
//! It is frozen separately from every version, like the greeting, and it
//! does not grow: an older build could not answer anything added later,
//! which is the one thing the core is for. `tests/frozen.rs` holds its bytes.
//!
//! A core frame starts with [`MARK`]. No versioned message does: each of
//! those starts with its tag as a varint, and a version's tags stay far
//! below what `0xff` would begin.

use serde::{Deserialize, Serialize};

use crate::Error;

/// Opens every core frame.
pub const MARK: [u8; 4] = *b"\xffBRC";

/// What an app may always ask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ask {
    /// Which sessions the server holds.
    Sessions,
    /// End this session, whatever runs in it.
    End { session: u64 },
}

/// A session, as far as any build can describe one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: u64,
    /// What to call it. Empty when the server cannot say: a worker of a
    /// build it cannot read. Untrusted text, for display only.
    pub title: String,
    /// The build its worker runs, as that worker greeted. Empty if unknown.
    pub build: String,
    /// Whether its process is running.
    pub alive: bool,
}

/// What a server always answers: the sessions it holds now, to either ask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Answer {
    Sessions(Vec<Session>),
}

/// Whether a frame's payload is a core frame rather than a version's message.
pub fn is_core(payload: &[u8]) -> bool {
    payload.starts_with(&MARK)
}

fn marked(body: Vec<u8>) -> Vec<u8> {
    let mut bytes = MARK.to_vec();
    bytes.extend_from_slice(&body);
    bytes
}

fn unmarked<'a>(payload: &'a [u8], what: &'static str) -> Result<&'a [u8], Error> {
    payload.strip_prefix(&MARK[..]).ok_or(Error::Malformed {
        what,
        detail: "not a core frame".into(),
    })
}

impl Ask {
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        crate::encode(self, "core ask").map(marked)
    }

    pub fn decode(payload: &[u8]) -> Result<Self, Error> {
        crate::decode_exact(unmarked(payload, "core ask")?, "core ask")
    }
}

impl Answer {
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        crate::encode(self, "core answer").map(marked)
    }

    pub fn decode(payload: &[u8]) -> Result<Self, Error> {
        crate::decode_exact(unmarked(payload, "core answer")?, "core answer")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::{ToApp, ToServer};

    #[test]
    fn asks_and_answers_round_trip() {
        for ask in [Ask::Sessions, Ask::End { session: u64::MAX }] {
            let bytes = ask.encode().unwrap();
            assert!(is_core(&bytes));
            assert_eq!(Ask::decode(&bytes).unwrap(), ask);
        }
        let answer = Answer::Sessions(vec![Session {
            id: 3,
            title: "vim".into(),
            build: "0.1.0+0123456789abcdef".into(),
            alive: true,
        }]);
        assert_eq!(Answer::decode(&answer.encode().unwrap()).unwrap(), answer);
    }

    /// The two never read as each other: a version's message is not a core
    /// frame, and a core frame is no message of any version.
    #[test]
    fn a_core_frame_is_never_a_versions_message() {
        let ask = Ask::Sessions.encode().unwrap();
        assert!(ToServer::decode(&ask).is_err());
        assert!(ToApp::decode(&Answer::Sessions(Vec::new()).encode().unwrap()).is_err());

        let listed = ToServer::ListSessions.encode().unwrap();
        assert!(!is_core(&listed));
        assert!(Ask::decode(&listed).is_err());
        assert!(!is_core(&ToApp::Sessions(Vec::new()).encode().unwrap()));
        // A version's tag is one byte for as long as it has under 128
        // messages, and a byte under 0x80 is never the mark's first.
        const { assert!(crate::v1::TO_SERVER_VARIANTS < 128) };
        assert!(MARK[0] >= 0x80);
    }
}
