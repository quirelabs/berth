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

    #[error("host key mismatch for {alias}: known {known}, offered {offered}")]
    HostKeyMismatch {
        alias: String,
        known: String,
        offered: String,
    },

    #[error("no ssh agent available")]
    NoAgent,

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Ssh(#[from] russh::Error),
}
