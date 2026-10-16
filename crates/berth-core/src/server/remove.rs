//! `berth-server --remove`: take Berth's server off this machine (D55).
//!
//! It refuses while sessions run unless forced, stops the supervisor and any
//! workers, and deletes what the server put in its data directory: the
//! builds under `bin/` and the runtime files. Nothing else is touched, and
//! the data directory itself goes only if that leaves it empty.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::paths::{self, Places};

/// What this mode says on the host's terminal. The server has no window, so
/// its few words live here rather than with the app's.
pub mod text {
    pub fn sessions_running(count: usize) -> String {
        let sessions = if count == 1 {
            "session is"
        } else {
            "sessions are"
        };
        format!(
            "{count} {sessions} running. End them first, or run this again with --force to end them."
        )
    }
    pub fn removed(path: &std::path::Path) -> String {
        format!("removed {}", path.display())
    }
    pub fn not_stopped(pid: u32) -> String {
        format!("process {pid} did not stop; nothing was deleted")
    }
    pub const NOTHING: &str = "Berth's server is not installed here.";
    pub const DONE: &str = "Berth's server is removed.";
    pub const NOT_HERE: &str = "berth-server --remove is for Linux and macOS hosts.";
}

/// How long the processes get to end once asked, before they are made to.
const STOP_WITHIN: Duration = Duration::from_secs(3);

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Refused: this many sessions are running and it was not forced.
    SessionsRunning(usize),
    /// A process would not stop, so nothing was deleted.
    NotStopped(u32),
    /// Done. Everything deleted, outermost last; empty when there was
    /// nothing to remove.
    Removed(Vec<PathBuf>),
}

/// Remove the server kept at `places`.
pub fn remove(places: &Places, force: bool) -> io::Result<Outcome> {
    let sessions = paths::live_workers(places);
    if !sessions.is_empty() && !force {
        return Ok(Outcome::SessionsRunning(sessions.len()));
    }
    // The supervisor first, so nothing new starts while the rest is stopped.
    let mut pids: Vec<u32> = Vec::new();
    pids.extend(
        paths::read_pid(&places.supervisor_pid()).filter(|pid| is_ours(*pid, "--supervisor")),
    );
    for session in sessions {
        pids.extend(
            paths::read_pid(&places.worker_pid(session)).filter(|pid| is_ours(*pid, "--worker")),
        );
    }
    if let Some(pid) = stop(&pids) {
        return Ok(Outcome::NotStopped(pid));
    }

    let mut removed = Vec::new();
    if places.run.is_dir() {
        std::fs::remove_dir_all(&places.run)?;
        removed.push(places.run.clone());
    }
    if let Ok(entries) = std::fs::read_dir(&places.bin) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if paths::is_build(&name) && entry.file_type().is_ok_and(|t| t.is_dir()) {
                std::fs::remove_dir_all(entry.path())?;
                removed.push(entry.path());
            }
        }
    }
    // Only what is now empty: anything else in there is not the server's.
    let mut empty = vec![places.bin.clone()];
    empty.extend(places.run.parent().map(Path::to_path_buf));
    empty.extend(places.bin.parent().map(Path::to_path_buf));
    for dir in empty {
        if std::fs::remove_dir(&dir).is_ok() {
            removed.push(dir);
        }
    }
    Ok(Outcome::Removed(removed))
}

/// Whether `pid` is one of this server's processes, by the mode it was
/// started in. A pid file can outlive its process, and the number can be
/// someone else's by now; that one is left alone.
fn is_ours(pid: u32, mode: &str) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(mode))
}

/// End the worker process `pid`, for a supervisor that cannot read it
/// (D55). Whether it is gone. A pid that is not a worker's is left alone.
pub(super) fn end_worker(pid: u32) -> bool {
    if !is_ours(pid, "--worker") {
        return !paths::alive(pid);
    }
    stop(&[pid]).is_none()
}

/// Ask each process to end, then make whatever is left. The first that is
/// still running after that is returned.
fn stop(pids: &[u32]) -> Option<u32> {
    signal("-TERM", pids);
    let deadline = Instant::now() + STOP_WITHIN;
    while Instant::now() < deadline && pids.iter().any(|pid| paths::alive(*pid)) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let left: Vec<u32> = pids
        .iter()
        .copied()
        .filter(|pid| paths::alive(*pid))
        .collect();
    signal("-KILL", &left);
    let deadline = Instant::now() + STOP_WITHIN;
    while Instant::now() < deadline && left.iter().any(|pid| paths::alive(*pid)) {
        std::thread::sleep(Duration::from_millis(50));
    }
    left.into_iter().find(|pid| paths::alive(*pid))
}

fn signal(how: &str, pids: &[u32]) {
    for pid in pids {
        let _ = std::process::Command::new("kill")
            .args([how, &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// `berth-server --remove [--force]`. Whether it was removed.
pub fn run_process(args: &[String]) -> bool {
    if !cfg!(unix) {
        eprintln!("{}", text::NOT_HERE);
        return false;
    }
    let force = args.iter().any(|a| a == "--force");
    let Some(places) = Places::standard() else {
        eprintln!("{}", text::NOTHING);
        return false;
    };
    match remove(&places, force) {
        Ok(Outcome::SessionsRunning(count)) => {
            eprintln!("{}", text::sessions_running(count));
            false
        }
        Ok(Outcome::NotStopped(pid)) => {
            eprintln!("{}", text::not_stopped(pid));
            false
        }
        Ok(Outcome::Removed(removed)) if removed.is_empty() => {
            println!("{}", text::NOTHING);
            true
        }
        Ok(Outcome::Removed(removed)) => {
            for path in &removed {
                println!("{}", text::removed(path));
            }
            println!("{}", text::DONE);
            true
        }
        Err(error) => {
            eprintln!("{error}");
            false
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn installed(dir: &Path) -> (Places, PathBuf) {
        let places = Places::under(dir);
        places.prepare().unwrap();
        let exe = dir.join("berth-server");
        std::fs::write(&exe, b"a binary").unwrap();
        let copy = paths::install_copy(&places.bin, &exe).unwrap();
        std::fs::remove_file(exe).unwrap();
        (places, copy)
    }

    /// Everything the server put there goes, and the directory with it.
    #[test]
    fn an_install_with_nothing_running_is_removed_whole() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("berth");
        let (places, copy) = installed(&data);
        let outcome = remove(&places, false).unwrap();
        assert!(
            matches!(outcome, Outcome::Removed(ref r) if r.last() == Some(&data)),
            "{outcome:?}"
        );
        assert!(!copy.exists());
        assert!(!data.exists());
        // Again: nothing to do, and that is not an error.
        assert_eq!(
            remove(&places, false).unwrap(),
            Outcome::Removed(Vec::new())
        );
    }

    /// What is not the server's stays, and so does the directory holding it.
    #[test]
    fn what_the_server_did_not_put_there_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("berth");
        let (places, _) = installed(&data);
        std::fs::write(places.bin.join("claude"), b"shim").unwrap();
        std::fs::write(data.join("local-sessions.toml"), b"").unwrap();
        remove(&places, true).unwrap();
        assert!(places.bin.join("claude").exists());
        assert!(data.join("local-sessions.toml").exists());
        assert!(!places.run.exists());
    }

    /// A running session is a reason to stop and say so; forced, it is ended.
    #[test]
    fn running_sessions_refuse_it_unless_forced() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("berth");
        let (places, copy) = installed(&data);
        // A stand-in worker: a process whose arguments name the mode.
        // The loop keeps the shell from becoming its last command. It is
        // waited on from a thread: a real worker is nobody's child, and one
        // that has ended and not been waited on still counts as running.
        let mut worker = std::process::Command::new("sh")
            .args(["-c", "while :; do sleep 1; done", "--worker"])
            .spawn()
            .unwrap();
        let pid = worker.id();
        std::fs::write(places.worker_pid(4), pid.to_string()).unwrap();
        let ended = std::thread::spawn(move || worker.wait());

        assert_eq!(remove(&places, false).unwrap(), Outcome::SessionsRunning(1));
        assert!(copy.exists(), "nothing is deleted when it refuses");
        assert!(paths::alive(pid));

        let outcome = remove(&places, true).unwrap();
        signal("-KILL", &[pid]);
        let _ = ended.join();
        assert!(matches!(outcome, Outcome::Removed(_)), "{outcome:?}");
        assert!(!data.exists());
    }

    /// A pid file naming a process that is not the server's ends nothing.
    #[test]
    fn a_process_that_is_not_the_servers_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("berth");
        let (places, _) = installed(&data);
        let mut other = std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .unwrap();
        std::fs::write(places.worker_pid(9), other.id().to_string()).unwrap();
        remove(&places, true).unwrap();
        assert!(paths::alive(other.id()), "someone else's process");
        let _ = other.kill();
        let _ = other.wait();
    }
}
