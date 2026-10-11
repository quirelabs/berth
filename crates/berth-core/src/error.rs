use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ssh config: {path}: {message}")]
    SshConfig { path: PathBuf, message: String },

    #[error("berth.toml: {0}")]
    Config(String),

    #[error("unknown host alias: {0}")]
    UnknownHost(String),

    /// The host is not in known_hosts and the user has not accepted it. This is
    /// a question, not a failure.
    #[error("host key not recognised: {fingerprint}")]
    HostKeyUnknown { fingerprint: String },

    /// A different key is recorded for this host. Never recoverable in place.
    #[error(
        "host key has CHANGED: known_hosts line {line} records a different key, offered {fingerprint}"
    )]
    HostKeyChanged { line: usize, fingerprint: String },

    /// The user accepted a key and it could not be recorded, so the next
    /// connect would ask again. Reported rather than swallowed.
    #[error("could not record the host key: {0}")]
    KnownHosts(String),

    /// A pty on this machine would not open, start a shell, or be read. Not a
    /// configuration problem, and it must not read as one.
    #[error("local shell: {0}")]
    Pty(String),

    /// The connection did something the protocol allows and Berth does not
    /// expect. Rare, and worth saying out loud rather than folding into a
    /// nearby variant.
    #[error("ssh transport: {0}")]
    Transport(String),

    /// The host accepted nothing in time: a dropped packet, a machine that is
    /// asleep, or something listening that is not an ssh server. Named rather
    /// than left to the OS, whose own answer takes over a minute.
    #[error("no answer from {endpoint} after {seconds}s")]
    Timeout { endpoint: String, seconds: u64 },

    #[error("ssh agent: {0}")]
    Agent(String),

    #[error("no ssh agent available")]
    NoAgent,

    #[error("authentication failed for {user} after offering {offered} agent identities")]
    AuthFailed { user: String, offered: usize },

    /// Berth's own local server: a socket that would not open, a peer that
    /// did not speak the protocol, a worker that did not start.
    #[error("local server: {0}")]
    Server(String),

    /// The server answering is not the build expected of it, and said
    /// something this build could not read. Both speak the same protocol
    /// version, so this is never reported as malformed (D54).
    #[error("a different Berth server build is running: {theirs}, where {expected} was expected")]
    DifferentBuild { theirs: String, expected: String },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Ssh(#[from] russh::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A changed key must never read like a routine failure.
    #[test]
    fn a_changed_host_key_names_itself_loudly() {
        let e = Error::HostKeyChanged {
            line: 7,
            fingerprint: "SHA256:abc".into(),
        };
        let text = e.to_string();
        assert!(text.contains("CHANGED"), "{text}");
        assert!(text.contains('7'), "{text}");
        assert!(text.contains("SHA256:abc"), "{text}");
    }

    #[test]
    fn an_unknown_host_key_reads_as_a_question() {
        let e = Error::HostKeyUnknown {
            fingerprint: "SHA256:xyz".into(),
        };
        assert!(!e.to_string().contains("CHANGED"));
    }
}
