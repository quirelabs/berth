//! The server binaries an app carries, and what each must hash to (D55).
//!
//! Four builds, one per kind of host, stored gzip-compressed beside the app.
//! The manifest is compiled into the app: a line per target with the
//! binary's size and SHA-256, and the same for its compressed file. A file
//! is checked against it before a byte is uploaded, and the binary again on
//! the host once unpacked.

use std::path::{Path, PathBuf};

use crate::server::paths;

/// A kind of host there is a server for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    LinuxX86_64,
    LinuxAarch64,
    MacArm64,
    MacX86_64,
}

impl Target {
    pub const ALL: [Target; 4] = [
        Target::LinuxX86_64,
        Target::LinuxAarch64,
        Target::MacArm64,
        Target::MacX86_64,
    ];

    /// The target as the build names it.
    pub fn triple(self) -> &'static str {
        match self {
            Target::LinuxX86_64 => "x86_64-unknown-linux-musl",
            Target::LinuxAarch64 => "aarch64-unknown-linux-musl",
            Target::MacArm64 => "aarch64-apple-darwin",
            Target::MacX86_64 => "x86_64-apple-darwin",
        }
    }

    fn of_triple(triple: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.triple() == triple)
    }

    /// The target for a host, from `uname -s` and `uname -m`.
    pub fn for_host(os: &str, arch: &str) -> Option<Self> {
        match (os, arch) {
            ("Linux", "x86_64" | "amd64") => Some(Target::LinuxX86_64),
            ("Linux", "aarch64" | "arm64") => Some(Target::LinuxAarch64),
            ("Darwin", "arm64" | "aarch64") => Some(Target::MacArm64),
            ("Darwin", "x86_64") => Some(Target::MacX86_64),
            _ => None,
        }
    }

    /// The binary's file name beside the app: `berth-server-<triple>`, as CI
    /// names it. The compressed one adds `.gz`.
    pub fn file(self) -> String {
        format!("berth-server-{}", self.triple())
    }
}

/// A file's size and SHA-256 in hex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sum {
    pub size: u64,
    pub sha256: String,
}

impl Sum {
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            size: bytes.len() as u64,
            sha256: paths::sha256_hex(bytes),
        }
    }
}

/// One target's server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub target: Target,
    /// The binary itself, as it must be on the host.
    pub binary: Sum,
    /// Its gzip file, when there is one.
    pub packed: Option<Sum>,
}

impl Entry {
    /// The build this binary is, as its handshake will name it (D54).
    pub fn build(&self) -> String {
        paths::build_with(env!("CARGO_PKG_VERSION"), &self.binary.sha256)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("manifest line {line}: {why}")]
    Line { line: usize, why: String },
    #[error("no server for {0} beside this app")]
    Missing(&'static str),
    #[error("the server for {target} beside this app is not the one this app was built with")]
    Altered { target: &'static str },
    #[error("could not read {path}: {why}")]
    Unreadable { path: PathBuf, why: String },
}

fn sum(size: &str, sha256: &str) -> Result<Sum, String> {
    let size = size.parse().map_err(|_| format!("{size} is not a size"))?;
    let hex = sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit());
    if !hex {
        return Err(format!("{sha256} is not a SHA-256"));
    }
    Ok(Sum {
        size,
        sha256: sha256.to_ascii_lowercase(),
    })
}

impl Manifest {
    /// A manifest from its text: a line per target,
    /// `<triple> <size> <sha256> [<gz size> <gz sha256>]`. Blank lines and
    /// lines starting `#` are skipped. A line that cannot be read fails the
    /// whole of it: half a manifest is not one.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let mut entries: Vec<Entry> = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fail = |why: String| Error::Line {
                line: index + 1,
                why,
            };
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (triple, binary, packed) = match fields[..] {
                [triple, size, sha] => (triple, sum(size, sha), None),
                [triple, size, sha, gz_size, gz_sha] => {
                    (triple, sum(size, sha), Some(sum(gz_size, gz_sha)))
                }
                _ => return Err(fail("expected 3 or 5 fields".into())),
            };
            let target = Target::of_triple(triple)
                .ok_or_else(|| fail(format!("unknown target {triple}")))?;
            if entries.iter().any(|e| e.target == target) {
                return Err(fail(format!("{triple} is listed twice")));
            }
            entries.push(Entry {
                target,
                binary: binary.map_err(fail)?,
                packed: packed.transpose().map_err(fail)?,
            });
        }
        Ok(Self { entries })
    }

    /// The manifest of the server files in `dir`, from the files themselves:
    /// for a directory of development builds, where nothing was compiled in.
    pub fn of_dir(dir: &Path) -> Result<Self, Error> {
        let mut entries = Vec::new();
        for target in Target::ALL {
            let path = dir.join(target.file());
            if !path.is_file() {
                continue;
            }
            let binary = Sum::of(&read(&path)?);
            let gz = dir.join(format!("{}.gz", target.file()));
            let packed = if gz.is_file() {
                Some(Sum::of(&read(&gz)?))
            } else {
                None
            };
            entries.push(Entry {
                target,
                binary,
                packed,
            });
        }
        Ok(Self { entries })
    }

    pub fn entry(&self, target: Target) -> Option<&Entry> {
        self.entries.iter().find(|e| e.target == target)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The text [`Manifest::parse`] reads.
    pub fn to_text(&self) -> String {
        let mut text = String::new();
        for entry in &self.entries {
            text.push_str(&format!(
                "{} {} {}",
                entry.target.triple(),
                entry.binary.size,
                entry.binary.sha256
            ));
            if let Some(packed) = &entry.packed {
                text.push_str(&format!(" {} {}", packed.size, packed.sha256));
            }
            text.push('\n');
        }
        text
    }
}

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|e| Error::Unreadable {
        path: path.to_path_buf(),
        why: e.to_string(),
    })
}

/// The servers an app carries: what they must be, and where the files are.
#[derive(Debug, Clone)]
pub struct Servers {
    pub manifest: Manifest,
    pub dir: PathBuf,
}

/// What is uploaded for one install: the gzip file when there is one and
/// the host can unpack it, else the binary as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    pub bytes: Vec<u8>,
    pub packed: bool,
}

impl Servers {
    /// The bytes to upload for `target`, checked against the manifest before
    /// they go anywhere. `unpacks` is whether the host has gzip.
    pub fn payload(&self, target: Target, unpacks: bool) -> Result<Payload, Error> {
        let entry = self
            .manifest
            .entry(target)
            .ok_or(Error::Missing(target.triple()))?;
        let altered = Error::Altered {
            target: target.triple(),
        };
        let gz = self.dir.join(format!("{}.gz", target.file()));
        if let (true, Some(packed)) = (unpacks && gz.is_file(), &entry.packed) {
            let bytes = read(&gz)?;
            if Sum::of(&bytes) != *packed {
                return Err(altered);
            }
            return Ok(Payload {
                bytes,
                packed: true,
            });
        }
        let plain = self.dir.join(target.file());
        if !plain.is_file() {
            return Err(Error::Missing(target.triple()));
        }
        let bytes = read(&plain)?;
        if Sum::of(&bytes) != entry.binary {
            return Err(altered);
        }
        Ok(Payload {
            bytes,
            packed: false,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const ONE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TWO: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// A manifest with a build for each kind of Linux host.
    pub(crate) fn manifest() -> Manifest {
        Manifest::parse(&format!(
            "# built by the release\n\
             x86_64-unknown-linux-musl 5214864 {ONE} 2000000 {TWO}\n\
             aarch64-unknown-linux-musl 4900000 {TWO}\n"
        ))
        .unwrap()
    }

    #[test]
    fn a_manifest_reads_back_as_it_was_written() {
        let manifest = manifest();
        let entry = manifest.entry(Target::LinuxX86_64).unwrap();
        assert_eq!(entry.binary.size, 5_214_864);
        assert_eq!(entry.packed.as_ref().unwrap().sha256, TWO);
        assert_eq!(manifest.entry(Target::LinuxAarch64).unwrap().packed, None);
        assert_eq!(manifest.entry(Target::MacArm64), None);
        assert_eq!(Manifest::parse(&manifest.to_text()).unwrap(), manifest);
    }

    /// The build is the version and the start of the binary's hash: what the
    /// server will say of itself, known before it has run.
    #[test]
    fn an_entry_names_the_build_its_binary_is() {
        let build = manifest().entry(Target::LinuxX86_64).unwrap().build();
        assert_eq!(
            build,
            format!("{}+aaaaaaaaaaaaaaaa", env!("CARGO_PKG_VERSION"))
        );
        assert!(paths::is_build(&build));
    }

    #[test]
    fn a_line_that_cannot_be_read_fails_the_whole_manifest() {
        for bad in [
            "x86_64-unknown-linux-musl 12",
            "riscv64-unknown-linux-musl 12 aaaa",
            &format!("x86_64-unknown-linux-musl twelve {ONE}"),
            "x86_64-unknown-linux-musl 12 abc",
            &format!("x86_64-unknown-linux-musl 1 {ONE}\nx86_64-unknown-linux-musl 1 {ONE}"),
        ] {
            assert!(Manifest::parse(bad).is_err(), "{bad}");
        }
        assert!(Manifest::parse("").unwrap().is_empty());
    }

    #[test]
    fn every_host_berth_has_a_server_for_is_told_from_uname() {
        assert_eq!(
            Target::for_host("Linux", "x86_64"),
            Some(Target::LinuxX86_64)
        );
        assert_eq!(
            Target::for_host("Linux", "aarch64"),
            Some(Target::LinuxAarch64)
        );
        assert_eq!(Target::for_host("Darwin", "arm64"), Some(Target::MacArm64));
        assert_eq!(
            Target::for_host("Darwin", "x86_64"),
            Some(Target::MacX86_64)
        );
        assert_eq!(Target::for_host("FreeBSD", "amd64"), None);
        assert_eq!(Target::for_host("Linux", "riscv64"), None);
    }

    /// The file beside the app is uploaded only if it is the one the app was
    /// built with; the compressed one is preferred when the host can unpack.
    #[test]
    fn a_payload_is_checked_before_it_goes_anywhere() {
        let dir = tempfile::tempdir().unwrap();
        let target = Target::LinuxX86_64;
        std::fs::write(dir.path().join(target.file()), b"the binary").unwrap();
        std::fs::write(dir.path().join(format!("{}.gz", target.file())), b"packed").unwrap();
        let servers = Servers {
            manifest: Manifest::of_dir(dir.path()).unwrap(),
            dir: dir.path().to_path_buf(),
        };
        let packed = servers.payload(target, true).unwrap();
        assert!(packed.packed);
        assert_eq!(packed.bytes, b"packed");
        let plain = servers.payload(target, false).unwrap();
        assert!(!plain.packed);
        assert_eq!(plain.bytes, b"the binary");
        assert_eq!(
            servers.payload(Target::MacArm64, true),
            Err(Error::Missing("aarch64-apple-darwin"))
        );

        std::fs::write(dir.path().join(format!("{}.gz", target.file())), b"other").unwrap();
        assert!(matches!(
            servers.payload(target, true),
            Err(Error::Altered { .. })
        ));
        std::fs::write(dir.path().join(target.file()), b"another binary").unwrap();
        assert!(matches!(
            servers.payload(target, false),
            Err(Error::Altered { .. })
        ));
    }
}
