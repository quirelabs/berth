/// A resolved connection target, merged from ssh config and berth.toml.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    /// The `Host` line from ssh config, or a user-chosen name for ad hoc hosts.
    pub alias: String,
    pub hostname: String,
    pub port: u16,
    pub user: Option<String>,
    pub proxy_jump: Option<String>,
    pub identity_agent: Option<String>,
    /// Local forwards declared for this host, restored on every reconnect.
    pub forwards: Vec<crate::forward::Forward>,
    /// From berth.toml, e.g. "prod".
    pub tags: Vec<String>,
    pub group: Option<String>,
}

impl Host {
    pub fn new(alias: impl Into<String>, hostname: impl Into<String>) -> Self {
        let alias = alias.into();
        Self {
            alias,
            hostname: hostname.into(),
            port: 22,
            user: None,
            proxy_jump: None,
            identity_agent: None,
            forwards: Vec::new(),
            tags: Vec::new(),
            group: None,
        }
    }

    /// Parse an ad hoc target of the form `[user@]host[:port]`.
    pub fn parse_ad_hoc(spec: &str) -> Option<Self> {
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        let (user, rest) = match spec.split_once('@') {
            Some((u, r)) if !u.is_empty() && !r.is_empty() => (Some(u.to_string()), r),
            Some(_) => return None,
            None => (None, spec),
        };
        let (hostname, port) = match rest.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() => (h, p.parse().ok()?),
            Some(_) => return None,
            None => (rest, 22u16),
        };
        if hostname.is_empty() {
            return None;
        }
        let mut host = Host::new(spec, hostname);
        host.port = port;
        host.user = user;
        Some(host)
    }

    pub fn is_prod(&self) -> bool {
        self.tags.iter().any(|t| t == "prod")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ad_hoc_defaults_to_port_22() {
        let h = Host::parse_ad_hoc("example.com").unwrap();
        assert_eq!(h.hostname, "example.com");
        assert_eq!(h.port, 22);
        assert_eq!(h.user, None);
    }

    #[test]
    fn ad_hoc_with_user_and_port() {
        let h = Host::parse_ad_hoc("root@203.0.113.10:2222").unwrap();
        assert_eq!(h.user.as_deref(), Some("root"));
        assert_eq!(h.hostname, "203.0.113.10");
        assert_eq!(h.port, 2222);
    }

    #[test]
    fn ad_hoc_rejects_malformed() {
        for bad in ["", "@host", "user@", "host:", "host:notaport"] {
            assert!(Host::parse_ad_hoc(bad).is_none(), "accepted {bad:?}");
        }
    }
}
