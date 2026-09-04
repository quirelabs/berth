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

    #[error("ssh agent: {0}")]
    Agent(String),

    #[error("no ssh agent available")]
    NoAgent,

    #[error("authentication failed for {user} after offering {offered} agent identities")]
    AuthFailed { user: String, offered: usize },

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
