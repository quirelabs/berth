//! Host key verification against `~/.ssh/known_hosts`.
//!
//! Three outcomes, and they are not interchangeable. A key Berth has never seen
//! is a question for the user. A key that has *changed* is never a question:
//! it is a hard error, because that is what a man-in-the-middle looks like.

use std::path::{Path, PathBuf};

pub use russh::keys::ssh_key::PublicKey;

/// An override for the known_hosts file, so a test can exercise the trust path
/// against a fixture. Berth writes to this file when the user accepts a key,
/// and a test must never do that to the user's own.
pub fn path_override() -> Option<PathBuf> {
    std::env::var("BERTH_KNOWN_HOSTS")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn keys_for(host: &str, port: u16) -> Result<Vec<(usize, PublicKey)>, russh::keys::Error> {
    match path_override() {
        Some(path) => russh::keys::known_hosts::known_host_keys_path(host, port, path),
        None => russh::keys::known_hosts::known_host_keys(host, port),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The key matches a recorded entry.
    Trusted,
    /// The host is not in known_hosts. Ask the user; this is trust on first use.
    Unknown { fingerprint: String },
    /// A different key is recorded for this host. Never auto-accept.
    Changed { line: usize, fingerprint: String },
}

impl Verdict {
    /// Whether a connection may proceed without asking anyone.
    pub fn is_trusted(&self) -> bool {
        matches!(self, Verdict::Trusted)
    }

    /// Whether this must abort regardless of user preference.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Verdict::Changed { .. })
    }
}

/// What the user, or a policy, decided to do about a non-trusted key.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TrustPolicy {
    /// Refuse anything not already recorded. The safe default, and what a
    /// headless test should use.
    #[default]
    KnownHostsOnly,
    /// Accept an unknown host and record it. Set only after the user has
    /// answered the trust-on-first-use prompt. A changed key is still refused.
    TrustOnFirstUse,
    /// Refuse every key, recorded or not.
    ///
    /// For looking at a host's key without connecting to it. The handshake
    /// stops at the key check, so no identity is ever offered to a host the
    /// user has not accepted, and no session is opened on one they then reject.
    Refuse,
}

pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(Default::default()).to_string()
}

/// Check a key against the user's known_hosts.
/// Whether anything at all is recorded for this host.
///
/// Asked before a connection rather than after, because warming every host at
/// startup must never raise a trust-on-first-use prompt: ten dialogs on launch
/// is a bad first run. A host with no entry is left alone until the user asks
/// for it and sees the fingerprint.
///
/// This deliberately says nothing about whether the key still matches. That
/// question needs the server's key and is answered by [`verify`] on connect.
pub fn is_known(host: &str, port: u16) -> bool {
    keys_for(host, port)
        .map(|entries| !entries.is_empty())
        .unwrap_or(false)
}

pub fn is_known_in(host: &str, port: u16, path: &Path) -> bool {
    russh::keys::known_hosts::known_host_keys_path(host, port, path)
        .map(|entries| !entries.is_empty())
        .unwrap_or(false)
}

pub fn verify(host: &str, port: u16, key: &PublicKey) -> Verdict {
    match keys_for(host, port) {
        Ok(entries) => decide(key, entries),
        // An unreadable or absent file means nothing is known yet, which is the
        // same question as an unrecorded host rather than a failure.
        Err(_) => Verdict::Unknown {
            fingerprint: fingerprint(key),
        },
    }
}

/// As [`verify`], against an explicit file.
pub fn verify_in(host: &str, port: u16, key: &PublicKey, path: &Path) -> Verdict {
    match russh::keys::known_hosts::known_host_keys_path(host, port, path) {
        Ok(entries) => decide(key, entries),
        Err(_) => Verdict::Unknown {
            fingerprint: fingerprint(key),
        },
    }
}

/// Compare against every recorded entry for the host.
///
/// Key *material* is compared, not the parsed `PublicKey`. `PartialEq` on
/// `ssh_key::PublicKey` also compares the comment field, and the known_hosts
/// format allows a trailing comment, so comparing whole keys would report a
/// changed host key for an entry that merely carries a comment. That would
/// raise a false man-in-the-middle alarm, which is the one error in this module
/// that must never be cried wolf.
fn decide(key: &PublicKey, entries: Vec<(usize, PublicKey)>) -> Verdict {
    let mut same_algorithm = None;
    for (line, recorded) in entries {
        if recorded.key_data() == key.key_data() {
            return Verdict::Trusted;
        }
        if recorded.algorithm() == key.algorithm() && same_algorithm.is_none() {
            same_algorithm = Some(line);
        }
    }
    match same_algorithm {
        Some(line) => Verdict::Changed {
            line,
            fingerprint: fingerprint(key),
        },
        None => Verdict::Unknown {
            fingerprint: fingerprint(key),
        },
    }
}

/// Record a newly trusted host. Only ever called after an explicit yes.
///
/// The entry is written by russh in the standard OpenSSH known_hosts format,
/// appended to the same file `ssh` reads, so trusting a host in Berth means
/// `ssh` to it stops asking too. Berth deliberately does not keep its own
/// parallel trust store.
pub fn learn(host: &str, port: u16, key: &PublicKey) -> Result<(), russh::keys::Error> {
    match path_override() {
        Some(path) => learn_in(host, port, key, &path),
        None => russh::keys::known_hosts::learn_known_hosts(host, port, key),
    }
}

/// As [`learn`], against an explicit file.
pub fn learn_in(
    host: &str,
    port: u16,
    key: &PublicKey,
    path: &Path,
) -> Result<(), russh::keys::Error> {
    russh::keys::known_hosts::learn_known_hosts_path(host, port, key, path)
}

/// Decide whether a connection may continue.
pub fn accepts(verdict: &Verdict, policy: &TrustPolicy) -> bool {
    match (verdict, policy) {
        // Nothing gets through a probe, so it can never open a session.
        (_, TrustPolicy::Refuse) => false,
        (Verdict::Trusted, _) => true,
        // Never, under any policy. This is what a man-in-the-middle looks like.
        (Verdict::Changed { .. }, _) => false,
        (Verdict::Unknown { .. }, TrustPolicy::KnownHostsOnly) => false,
        (Verdict::Unknown { .. }, TrustPolicy::TrustOnFirstUse) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two throwaway ed25519 public keys. Fixed rather than generated so the
    // tests carry no key-generation dependency and stay deterministic.
    const KEY_A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ/AsGp+bb6TRnfNO5TyX7d/xT/SUkEgJKpUlV3ATLyx berth-test-a";
    const KEY_B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAV8JSHRVety7MMGyoT2zBRRPme8LlBbdWNyyr7N5n5v berth-test-b";

    fn key(openssh: &str) -> PublicKey {
        PublicKey::from_openssh(openssh).unwrap()
    }

    fn a_key() -> PublicKey {
        key(KEY_A)
    }

    fn another_key() -> PublicKey {
        key(KEY_B)
    }

    fn known_hosts_with(entries: &[(&str, &PublicKey)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let mut text = String::new();
        for (host, key) in entries {
            text.push_str(&format!("{host} {}\n", key.to_openssh().unwrap()));
        }
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn a_recorded_key_is_trusted() {
        let key = a_key();
        let (_dir, path) = known_hosts_with(&[("example.com", &key)]);
        assert_eq!(verify_in("example.com", 22, &key, &path), Verdict::Trusted);
    }

    #[test]
    fn an_unrecorded_host_is_unknown() {
        let key = a_key();
        let (_dir, path) = known_hosts_with(&[]);
        assert!(matches!(
            verify_in("example.com", 22, &key, &path),
            Verdict::Unknown { .. }
        ));
    }

    /// The case that matters: a different key for a host we already know.
    #[test]
    fn a_changed_key_is_fatal() {
        let recorded = a_key();
        let attacker = another_key();
        let (_dir, path) = known_hosts_with(&[("example.com", &recorded)]);
        let verdict = verify_in("example.com", 22, &attacker, &path);
        assert!(
            matches!(verdict, Verdict::Changed { .. }),
            "got {verdict:?}"
        );
        assert!(verdict.is_fatal());
        assert!(!accepts(&verdict, &TrustPolicy::TrustOnFirstUse));
        assert!(!accepts(&verdict, &TrustPolicy::KnownHostsOnly));
    }

    #[test]
    fn a_missing_file_asks_rather_than_fails() {
        let key = a_key();
        let missing = std::path::Path::new("definitely/not/here/known_hosts");
        assert!(matches!(
            verify_in("example.com", 22, &key, missing),
            Verdict::Unknown { .. }
        ));
    }

    #[test]
    fn the_default_policy_refuses_unknown_hosts() {
        let key = a_key();
        let unknown = Verdict::Unknown {
            fingerprint: fingerprint(&key),
        };
        assert!(!accepts(&unknown, &TrustPolicy::default()));
        assert!(accepts(&unknown, &TrustPolicy::TrustOnFirstUse));
    }

    #[test]
    fn a_nonstandard_port_is_matched_separately() {
        let key = a_key();
        let (_dir, path) = known_hosts_with(&[("[example.com]:2222", &key)]);
        assert_eq!(
            verify_in("example.com", 2222, &key, &path),
            Verdict::Trusted
        );
        assert!(matches!(
            verify_in("example.com", 22, &key, &path),
            Verdict::Unknown { .. }
        ));
    }

    /// The known_hosts format permits a trailing comment. Comparing whole
    /// `PublicKey` values would treat that as a changed key and raise a false
    /// man-in-the-middle alarm.
    #[test]
    fn a_trailing_comment_does_not_look_like_a_changed_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        std::fs::write(&path, format!("example.com {KEY_A}\n")).unwrap();
        assert_eq!(
            verify_in("example.com", 22, &a_key(), &path),
            Verdict::Trusted
        );
    }

    #[test]
    fn several_recorded_keys_match_on_any_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        std::fs::write(&path, format!("example.com {KEY_B}\nexample.com {KEY_A}\n")).unwrap();
        assert_eq!(
            verify_in("example.com", 22, &a_key(), &path),
            Verdict::Trusted
        );
    }

    #[test]
    fn comment_lines_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        std::fs::write(&path, format!("# a comment\nexample.com {KEY_A}\n")).unwrap();
        assert_eq!(
            verify_in("example.com", 22, &a_key(), &path),
            Verdict::Trusted
        );
    }

    /// A probe looks and never touches. Even a key it would have trusted must
    /// not open a session, or rejecting the prompt would leave one open.
    #[test]
    fn a_probe_refuses_every_key_including_a_trusted_one() {
        let key = a_key();
        for verdict in [
            Verdict::Trusted,
            Verdict::Unknown {
                fingerprint: fingerprint(&key),
            },
            Verdict::Changed {
                line: 3,
                fingerprint: fingerprint(&key),
            },
        ] {
            assert!(
                !accepts(&verdict, &TrustPolicy::Refuse),
                "a probe accepted {verdict:?}"
            );
        }
    }

    /// Accepting must write an entry the rest of the world can read: the same
    /// file `ssh` uses, in the format it expects.
    #[test]
    fn learning_writes_an_entry_that_verifies() {
        let key = a_key();
        let (_dir, path) = known_hosts_with(&[]);
        assert!(!is_known_in("example.com", 22, &path));

        learn_in("example.com", 22, &key, &path).unwrap();

        assert!(is_known_in("example.com", 22, &path));
        assert_eq!(verify_in("example.com", 22, &key, &path), Verdict::Trusted);
        let written = std::fs::read_to_string(&path).unwrap();
        // russh leads with a newline so it can never append to an unterminated
        // last line, which leaves a blank one at the top of a fresh file.
        let line = written
            .lines()
            .find(|l| !l.trim().is_empty())
            .expect("an entry was written");
        assert!(line.starts_with("example.com "), "{line}");
        assert!(line.contains("ssh-ed25519 "), "{line}");
    }

    /// A non-default port is recorded in bracket form, or `ssh` on port 22
    /// would silently inherit trust it was never given.
    #[test]
    fn learning_a_nonstandard_port_does_not_trust_port_22() {
        let key = a_key();
        let (_dir, path) = known_hosts_with(&[]);
        learn_in("example.com", 2222, &key, &path).unwrap();
        assert!(is_known_in("example.com", 2222, &path));
        assert!(!is_known_in("example.com", 22, &path));
    }

    #[test]
    fn fingerprints_are_sha256_and_stable() {
        let key = a_key();
        assert!(fingerprint(&key).starts_with("SHA256:"));
        assert_eq!(fingerprint(&key), fingerprint(&key));
    }
}

#[cfg(test)]
mod known_lookup_tests {
    use super::*;
    use std::io::Write;

    fn file_with(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("temp file");
        f.write_all(contents.as_bytes()).expect("write");
        f.flush().expect("flush");
        f
    }

    #[test]
    fn a_recorded_host_is_known() {
        let f = file_with(
            "example.test ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIE/ffYdOAsp6Vvx81VjAzoFgvNYZGpEfkX0vZdwcZewo\n",
        );
        assert!(is_known_in("example.test", 22, f.path()));
    }

    #[test]
    fn an_unrecorded_host_is_not_known() {
        let f = file_with(
            "example.test ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIE/ffYdOAsp6Vvx81VjAzoFgvNYZGpEfkX0vZdwcZewo\n",
        );
        assert!(!is_known_in("other.test", 22, f.path()));
    }

    /// The question warming asks. An empty or missing file means every host
    /// waits for the user rather than every host raising a prompt.
    #[test]
    fn an_empty_file_knows_nothing() {
        let f = file_with("");
        assert!(!is_known_in("example.test", 22, f.path()));
    }

    #[test]
    fn a_missing_file_knows_nothing() {
        assert!(!is_known_in(
            "example.test",
            22,
            std::path::Path::new("/nonexistent/known_hosts")
        ));
    }
}
