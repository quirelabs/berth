//! The handshake: the first frame each way, before any version is agreed.

use serde::{Deserialize, Serialize};

use crate::Error;

/// Opens every [`Hello`], so a connection to something that is not Berth
/// fails on its first frame, by name.
pub const MAGIC: [u8; 4] = *b"BRTH";

/// The protocol versions this build speaks. `OLDEST` only ever rises when a
/// version is retired on purpose, never because it was inconvenient to keep.
pub const OLDEST: u16 = 1;
pub const NEWEST: u16 = 1;

/// The first frame each way.
///
/// Its encoding is frozen for good, separately from every protocol version:
/// it is what two builds that share no version use to find that out. New
/// capabilities are announced as `features`, never as new fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub magic: [u8; 4],
    pub oldest: u16,
    pub newest: u16,
    /// Optional capabilities by name. Unknown names are ignored.
    pub features: Vec<String>,
    /// The sender's build, for logs and for the host row's upgrade notice.
    pub build: String,
}

/// What both sides settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agreed {
    pub version: u16,
    /// Features both sides announced, in this side's order.
    pub features: Vec<String>,
}

impl Hello {
    /// This build's greeting.
    pub fn ours(build: impl Into<String>, features: &[&str]) -> Self {
        Self {
            magic: MAGIC,
            oldest: OLDEST,
            newest: NEWEST,
            features: features.iter().map(|f| f.to_string()).collect(),
            build: build.into(),
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        crate::encode(self, "hello")
    }

    /// A greeting as received. Wrong magic is [`Error::NotBerth`], whatever
    /// else is in it.
    pub fn decode(payload: &[u8]) -> Result<Self, Error> {
        if payload.get(..MAGIC.len()) != Some(&MAGIC[..]) {
            return Err(Error::NotBerth);
        }
        crate::decode_exact(payload, "hello")
    }
}

/// The newest version both sides speak, and the features both announced.
pub fn negotiate(ours: &Hello, theirs: &Hello) -> Result<Agreed, Error> {
    if theirs.magic != MAGIC {
        return Err(Error::NotBerth);
    }
    let newest = ours.newest.min(theirs.newest);
    let oldest = ours.oldest.max(theirs.oldest);
    if newest < oldest || theirs.oldest > theirs.newest {
        return Err(Error::NoCommonVersion {
            ours_oldest: ours.oldest,
            ours_newest: ours.newest,
            theirs_oldest: theirs.oldest,
            theirs_newest: theirs.newest,
        });
    }
    let features = ours
        .features
        .iter()
        .filter(|f| theirs.features.contains(f))
        .cloned()
        .collect();
    Ok(Agreed {
        version: newest,
        features,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speaking(oldest: u16, newest: u16) -> Hello {
        Hello {
            oldest,
            newest,
            ..Hello::ours("test", &[])
        }
    }

    #[test]
    fn the_newest_common_version_wins() {
        let agreed = negotiate(&speaking(1, 3), &speaking(2, 5)).unwrap();
        assert_eq!(agreed.version, 3);
    }

    /// An app updated past an old worker still reaches it.
    #[test]
    fn a_newer_side_meets_an_older_one_where_it_is() {
        assert_eq!(
            negotiate(&speaking(1, 4), &speaking(1, 1)).unwrap().version,
            1
        );
        assert_eq!(
            negotiate(&speaking(1, 1), &speaking(1, 4)).unwrap().version,
            1
        );
    }

    #[test]
    fn no_overlap_is_refused_with_both_ranges() {
        assert_eq!(
            negotiate(&speaking(1, 2), &speaking(3, 4)),
            Err(Error::NoCommonVersion {
                ours_oldest: 1,
                ours_newest: 2,
                theirs_oldest: 3,
                theirs_newest: 4,
            })
        );
    }

    #[test]
    fn an_inverted_range_is_refused() {
        assert!(negotiate(&speaking(1, 3), &speaking(3, 1)).is_err());
    }

    #[test]
    fn only_shared_features_are_agreed() {
        let ours = Hello::ours("a", &["history", "claude"]);
        let theirs = Hello::ours("b", &["claude", "roaming"]);
        assert_eq!(negotiate(&ours, &theirs).unwrap().features, ["claude"]);
    }

    #[test]
    fn a_greeting_round_trips() {
        let hello = Hello::ours("0.1.0 (abc123)", &["claude"]);
        assert_eq!(Hello::decode(&hello.encode().unwrap()).unwrap(), hello);
    }

    /// An ssh banner, an HTTP reply, a shell prompt: named, not misread.
    #[test]
    fn something_else_on_the_line_is_not_berth() {
        assert_eq!(Hello::decode(b"SSH-2.0-OpenSSH_9.6"), Err(Error::NotBerth));
        assert_eq!(Hello::decode(b""), Err(Error::NotBerth));
        let mut wrong = Hello::ours("x", &[]);
        wrong.magic = *b"NOPE";
        assert_eq!(
            negotiate(&Hello::ours("y", &[]), &wrong),
            Err(Error::NotBerth)
        );
    }

    #[test]
    fn trailing_bytes_after_a_greeting_are_refused() {
        let mut bytes = Hello::ours("x", &[]).encode().unwrap();
        bytes.push(0);
        assert!(matches!(
            Hello::decode(&bytes),
            Err(Error::Malformed { .. })
        ));
    }
}
