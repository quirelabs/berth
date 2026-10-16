//! Installing one build of the server on a host, once the user has agreed
//! (D55).
//!
//! `<data>/bin/<build>/berth-server`, the user's own and 0700, never
//! system-wide and never with sudo. The upload goes to a temporary name in
//! that directory, is hash-checked there, renamed, and then run once to say
//! which build it is. A home mounted noexec is found at that last step, and
//! nothing is left behind.

use super::manifest::{Entry, Payload, Target};
use super::probe::{HashTool, Probe};
use super::{Runs, quoted};
use crate::transport::Output;

/// The binary's name on a host.
pub const BINARY: &str = "berth-server";
const PARTIAL: &str = ".berth-server.partial";

/// Why a host cannot be installed on as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unusable {
    /// The data directory's path has a character that cannot be passed
    /// safely through the host's shell, or is not absolute.
    Path(String),
}

/// One install, as the user is shown it and as it is then made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub target: Target,
    /// `uname`'s words for the host, for the dialog.
    pub os: String,
    pub arch: String,
    pub build: String,
    /// The host's data directory, and the build's directory in it.
    pub data: String,
    pub dir: String,
    /// The exact path of the binary.
    pub path: String,
    pub size: u64,
    pub sha256: String,
    /// The tool the hash is checked with on the host, if it has one.
    pub hash_tool: Option<HashTool>,
    /// Whether the upload can be the gzip file.
    pub gzip: bool,
    /// Sessions there would end when the connection does.
    pub sessions_end_at_logout: bool,
}

impl Plan {
    pub fn new(probe: &Probe, entry: &Entry) -> Result<Self, Unusable> {
        let data = probe.data.trim_end_matches('/').to_string();
        // Quoted with single quotes on the way to the host's shell, and the
        // shells in use agree on nothing about escaping inside them.
        let plain = data.starts_with('/')
            && !data
                .chars()
                .any(|c| c == '\'' || c == '\\' || c.is_control());
        if !plain {
            return Err(Unusable::Path(probe.data.clone()));
        }
        let build = entry.build();
        let dir = format!("{data}/bin/{build}");
        Ok(Self {
            target: entry.target,
            os: probe.os.clone(),
            arch: probe.arch.clone(),
            path: format!("{dir}/{BINARY}"),
            build,
            data,
            dir,
            size: entry.binary.size,
            sha256: entry.binary.sha256.clone(),
            hash_tool: probe.hash_tool,
            gzip: probe.gzip,
            sessions_end_at_logout: probe.sessions_end_at_logout(),
        })
    }

    /// The one line that takes it off the host again.
    pub fn removal(&self) -> String {
        format!("{} --remove", quoted(&self.path))
    }

    /// The same with nothing running, without the binary's help.
    pub fn removal_by_hand(&self) -> String {
        format!("rm -rf {}", quoted(&self.data))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Connection(String),
    #[error("could not write to {dir}: {detail}")]
    Upload { dir: String, detail: String },
    /// What arrived is not what was sent. Nothing is installed.
    #[error("the file on the host has SHA-256 {found}, not {expected}")]
    HashMismatch { expected: String, found: String },
    /// The file is whole and the host will not run it from there: the home
    /// directory is mounted noexec.
    #[error("the host will not run programs from {dir}")]
    Noexec { dir: String },
    /// It ran, and is not the build that was sent.
    #[error("the server on the host says it is {said}, not {expected}")]
    NotTheBuild { expected: String, said: String },
    #[error("{step} failed on the host: {detail}")]
    Failed { step: &'static str, detail: String },
}

/// What an install did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Installed {
    /// Whether the hash was checked on the host. Without a tool there it is
    /// not, which the user was told before agreeing.
    pub hash_checked: bool,
}

fn said(out: &Output) -> String {
    let text = format!(
        "{} {}",
        out.stderr_string().trim(),
        out.stdout_string().trim()
    );
    let text = text.trim();
    let shown: String = text.chars().take(300).collect();
    match out.exit_code {
        Some(code) => format!("{shown} (exit {code})"),
        None => shown,
    }
}

async fn run(host: &impl Runs, command: &str, stdin: &[u8]) -> Result<Output, Error> {
    host.run(command, stdin)
        .await
        .map_err(|e| Error::Connection(e.to_string()))
}

/// Install `payload` as `plan` describes. On any failure the build's
/// directory is removed again, so a host is never left with half of one.
pub async fn install(host: &impl Runs, plan: &Plan, payload: &Payload) -> Result<Installed, Error> {
    let result = put(host, plan, payload).await;
    if result.is_err() {
        // The directory is `<data>/bin/<build>` and nothing else: made from
        // a checked path and a build's name.
        let _ = run(host, &format!("rm -rf {}", quoted(&plan.dir)), b"").await;
    }
    result
}

async fn put(host: &impl Runs, plan: &Plan, payload: &Payload) -> Result<Installed, Error> {
    let partial = format!("{}/{PARTIAL}", plan.dir);

    // Written with nothing allowed to anyone else, directories included.
    // The script has no quote or backslash in it, so every shell hands it to
    // sh as it is; the directory arrives as its first argument.
    let unpack = if payload.packed { "gzip -dc" } else { "cat" };
    let upload = format!(
        "sh -c 'umask 077; mkdir -p \"$1\" && chmod 700 \"$1\" && {unpack} > \"$1/{PARTIAL}\" \
         && chmod 700 \"$1/{PARTIAL}\"' berth {}",
        quoted(&plan.dir)
    );
    let out = run(host, &upload, &payload.bytes).await?;
    if !out.succeeded() {
        return Err(Error::Upload {
            dir: plan.dir.clone(),
            detail: said(&out),
        });
    }

    let hash_checked = match plan.hash_tool {
        Some(tool) => {
            let out = run(
                host,
                &format!("{} {}", tool.command(), quoted(&partial)),
                b"",
            )
            .await?;
            let found = out
                .stdout_string()
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !out.succeeded() {
                return Err(Error::Failed {
                    step: "checking the hash",
                    detail: said(&out),
                });
            }
            if found != plan.sha256 {
                return Err(Error::HashMismatch {
                    expected: plan.sha256.clone(),
                    found,
                });
            }
            true
        }
        None => false,
    };

    let rename = format!("mv -f {} {}", quoted(&partial), quoted(&plan.path));
    let out = run(host, &rename, b"").await?;
    if !out.succeeded() {
        return Err(Error::Failed {
            step: "putting the server in place",
            detail: said(&out),
        });
    }

    // Run once: it is the build that was sent, and the host runs programs
    // from there at all.
    let out = run(host, &format!("{} --version", quoted(&plan.path)), b"").await?;
    let answer = out.stdout_string().trim().to_string();
    if out.succeeded() && answer == plan.build {
        return Ok(Installed { hash_checked });
    }
    let refused = out.exit_code == Some(126)
        || out
            .stderr_string()
            .to_lowercase()
            .contains("permission denied");
    if refused {
        return Err(Error::Noexec {
            dir: plan.dir.clone(),
        });
    }
    if out.succeeded() {
        return Err(Error::NotTheBuild {
            expected: plan.build.clone(),
            said: answer.chars().take(80).collect(),
        });
    }
    Err(Error::Failed {
        step: "running the server once",
        detail: said(&out),
    })
}

#[cfg(test)]
mod tests {
    use super::super::manifest::tests::manifest;
    use super::*;

    fn probe(data: &str) -> Probe {
        Probe {
            os: "Linux".into(),
            arch: "x86_64".into(),
            target: Some(Target::LinuxX86_64),
            home: "/home/u".into(),
            data: data.into(),
            builds: Vec::new(),
            hash_tool: Some(HashTool::Sha256sum),
            gzip: true,
            kills_at_logout: Some(true),
            linger: Some(false),
            sessions: 0,
        }
    }

    /// A host that answers each command from a script, and keeps what it
    /// was asked.
    struct Scripted {
        answers: Vec<(&'static str, Output)>,
        asked: std::sync::Mutex<Vec<String>>,
    }

    fn output(code: u32, stdout: &str, stderr: &str) -> Output {
        Output {
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            exit_code: Some(code),
        }
    }

    impl Scripted {
        fn new(answers: Vec<(&'static str, Output)>) -> Self {
            Self {
                answers,
                asked: Default::default(),
            }
        }
        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Runs for Scripted {
        async fn run(&self, command: &str, _stdin: &[u8]) -> crate::error::Result<Output> {
            self.asked.lock().unwrap().push(command.to_string());
            let answer = self.answers.iter().find(|(part, _)| command.contains(part));
            Ok(answer.map_or_else(|| output(0, "", ""), |(_, out)| out.clone()))
        }
    }

    fn plan() -> Plan {
        let manifest = manifest();
        let entry = manifest.entry(Target::LinuxX86_64).unwrap();
        Plan::new(&probe("/home/u/.local/share/berth"), entry).unwrap()
    }

    fn payload() -> Payload {
        Payload {
            bytes: b"packed".to_vec(),
            packed: true,
        }
    }

    /// The whole of it, in order: unpack to the temporary name, check the
    /// hash there, rename, run once.
    #[tokio::test]
    async fn an_install_uploads_checks_renames_and_runs_it_once() {
        let plan = plan();
        let host = Scripted::new(vec![
            (
                "sha256sum",
                output(0, &format!("{}  file\n", plan.sha256), ""),
            ),
            ("--version", output(0, &format!("{}\n", plan.build), "")),
        ]);
        let done = install(&host, &plan, &payload()).await.unwrap();
        assert!(done.hash_checked);
        let asked = host.asked();
        assert_eq!(asked.len(), 4, "{asked:?}");
        assert!(asked[0].contains("gzip -dc") && asked[0].contains("umask 077"));
        assert!(asked[0].ends_with(&format!("berth '{}'", plan.dir)));
        assert!(asked[1].starts_with("sha256sum ") && asked[1].contains(".partial"));
        assert!(asked[2].starts_with("mv -f "));
        assert_eq!(asked[3], format!("'{}' --version", plan.path));
    }

    /// Without gzip there the binary goes as it is; without a hash tool the
    /// check is skipped and that is reported, not hidden.
    #[tokio::test]
    async fn a_host_without_the_tools_gets_the_plain_file_and_says_what_was_not_checked() {
        let mut plan = plan();
        plan.hash_tool = None;
        let host = Scripted::new(vec![("--version", output(0, &plan.build, ""))]);
        let plain = Payload {
            bytes: b"binary".to_vec(),
            packed: false,
        };
        let done = install(&host, &plan, &plain).await.unwrap();
        assert!(!done.hash_checked);
        let asked = host.asked();
        assert!(asked[0].contains("cat > ") && !asked[0].contains("gzip"));
        assert!(
            !asked
                .iter()
                .any(|c| c.contains("sha256sum") || c.contains("shasum")),
            "{asked:?}"
        );
    }

    /// A wrong hash installs nothing: the file is never renamed or run, and
    /// the directory made for it is taken away again.
    #[tokio::test]
    async fn a_file_that_arrived_changed_is_never_put_in_place() {
        let plan = plan();
        let other = "c".repeat(64);
        let host = Scripted::new(vec![("sha256sum", output(0, &format!("{other}  f"), ""))]);
        let error = install(&host, &plan, &payload()).await.unwrap_err();
        assert_eq!(
            error,
            Error::HashMismatch {
                expected: plan.sha256.clone(),
                found: other
            }
        );
        let asked = host.asked();
        assert!(
            !asked
                .iter()
                .any(|c| c.starts_with("mv ") || c.contains("--version"))
        );
        assert_eq!(asked.last().unwrap(), &format!("rm -rf '{}'", plan.dir));
    }

    /// A home mounted noexec: the file is whole and will not run. Said by
    /// name, and nothing is left on the host.
    #[tokio::test]
    async fn a_home_that_runs_nothing_is_named_and_left_clean() {
        let plan = plan();
        for refusal in [
            output(126, "", "sh: berth-server: Permission denied"),
            output(1, "", "Permission denied."),
        ] {
            let host = Scripted::new(vec![
                ("sha256sum", output(0, &plan.sha256, "")),
                ("--version", refusal),
            ]);
            let error = install(&host, &plan, &payload()).await.unwrap_err();
            assert_eq!(
                error,
                Error::Noexec {
                    dir: plan.dir.clone()
                }
            );
            assert_eq!(
                host.asked().last().unwrap(),
                &format!("rm -rf '{}'", plan.dir)
            );
        }
    }

    #[tokio::test]
    async fn a_server_that_says_it_is_another_build_is_not_accepted() {
        let plan = plan();
        let host = Scripted::new(vec![
            ("sha256sum", output(0, &plan.sha256, "")),
            ("--version", output(0, "0.0.1+0000000000000000", "")),
        ]);
        let error = install(&host, &plan, &payload()).await.unwrap_err();
        assert!(matches!(error, Error::NotTheBuild { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_directory_that_cannot_be_written_says_so() {
        let plan = plan();
        let host = Scripted::new(vec![(
            "umask",
            output(1, "", "mkdir: Read-only file system"),
        )]);
        let error = install(&host, &plan, &payload()).await.unwrap_err();
        assert!(
            matches!(error, Error::Upload { ref detail, .. } if detail.contains("Read-only")),
            "{error:?}"
        );
    }

    #[test]
    fn a_plan_names_the_exact_path_and_how_to_remove_it() {
        let manifest = manifest();
        let entry = manifest.entry(Target::LinuxX86_64).unwrap();
        let plan = Plan::new(&probe("/home/u/.local/share/berth/"), entry).unwrap();
        let build = entry.build();
        assert_eq!(
            plan.path,
            format!("/home/u/.local/share/berth/bin/{build}/berth-server")
        );
        assert_eq!(plan.removal(), format!("'{}' --remove", plan.path));
        assert_eq!(
            plan.removal_by_hand(),
            "rm -rf '/home/u/.local/share/berth'"
        );
        assert_eq!(plan.size, 5_214_864);
        assert!(plan.sessions_end_at_logout);
    }

    /// A path with a space is fine; one that could end the quoting, or that
    /// is not a whole path, is refused before any command is made of it.
    #[test]
    fn a_path_that_cannot_be_quoted_is_refused() {
        let manifest = manifest();
        let entry = manifest.entry(Target::LinuxX86_64).unwrap();
        assert!(
            Plan::new(
                &probe("/Users/u/Library/Application Support/berth-host"),
                entry
            )
            .is_ok()
        );
        for bad in [
            "/home/u/it's",
            "/home/u/a\\b",
            "/home/u/a\nb",
            "relative/berth",
            "",
            "$HOME/berth",
        ] {
            assert_eq!(
                Plan::new(&probe(bad), entry),
                Err(Unusable::Path(bad.into())),
                "{bad:?}"
            );
        }
    }
}
