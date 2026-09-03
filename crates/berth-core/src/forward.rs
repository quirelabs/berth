//! Local port forwards. v1 binds loopback only and never listens publicly.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Forwards always bind here; a public bind is not offered in v1.
pub const BIND_ADDR: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
}

impl Forward {
    pub fn new(local_port: u16, remote_host: impl Into<String>, remote_port: u16) -> Self {
        Self {
            local_port,
            remote_host: remote_host.into(),
            remote_port,
        }
    }

    pub fn bind_addr(&self) -> SocketAddr {
        SocketAddr::new(BIND_ADDR, self.local_port)
    }

    /// Two forwards conflict when they want the same local port.
    pub fn conflicts_with(&self, other: &Forward) -> bool {
        self.local_port == other.local_port
    }
}

impl std::fmt::Display for Forward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "127.0.0.1:{} -> {}:{}",
            self.local_port, self.remote_host, self.remote_port
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_loopback_only() {
        let f = Forward::new(8080, "localhost", 80);
        assert_eq!(f.bind_addr().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(f.bind_addr().port(), 8080);
    }

    #[test]
    fn conflicts_on_local_port() {
        let a = Forward::new(8080, "a", 80);
        let b = Forward::new(8080, "b", 443);
        let c = Forward::new(8081, "a", 80);
        assert!(a.conflicts_with(&b));
        assert!(!a.conflicts_with(&c));
    }

    #[test]
    fn displays_readably() {
        assert_eq!(
            Forward::new(5432, "db.internal", 5432).to_string(),
            "127.0.0.1:5432 -> db.internal:5432"
        );
    }
}
