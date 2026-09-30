use crate::Direction;

/// Everything that can be wrong with bytes on the wire. Each names what it did
/// not understand: a side never guesses past something it cannot read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("a frame of {len} bytes is over the {max} byte limit")]
    FrameTooLarge { len: usize, max: usize },

    #[error("the other side is not speaking Berth's protocol")]
    NotBerth,

    #[error(
        "no protocol version in common: this side speaks {ours_oldest} to {ours_newest}, \
         the other {theirs_oldest} to {theirs_newest}"
    )]
    NoCommonVersion {
        ours_oldest: u16,
        ours_newest: u16,
        theirs_oldest: u16,
        theirs_newest: u16,
    },

    #[error("protocol version {0} is not one this build speaks")]
    UnknownVersion(u16),

    #[error("unknown version {version} message {direction}, tag {tag}")]
    UnknownMessage {
        version: u16,
        direction: Direction,
        tag: u32,
    },

    #[error("malformed {what}: {detail}")]
    Malformed { what: &'static str, detail: String },

    #[error("a packed body unpacks to more than {max} bytes")]
    UnpackedTooLarge { max: usize },
}
