//! Where the server keeps its endpoints, pid files and binaries (D50).
//!
//! The runtime directory is private to the user, is never `$TMPDIR` (macOS
//! clears files there that have gone untouched for about three days, so a
//! week-long session would lose its socket while it runs), and holds only
//! short names: macOS limits a socket path to 104 bytes.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use berth_proto::v1::SessionId;

/// Where one server's files live.
#[derive(Debug, Clone)]
pub struct Places {
    /// Endpoints and pid files. Private to the user.
    pub run: PathBuf,
    /// Versioned copies of the binary that workers run from.
    pub bin: PathBuf,
}

impl Places {
    /// The platform's places: under the local data directory
    /// (`~/Library/Application Support/berth` on macOS, `%LOCALAPPDATA%\berth`
    /// on Windows), with the runtime directory under `$XDG_RUNTIME_DIR` on
    /// Linux where there is one. `BERTH_DATA` moves all of it, for tests.
    pub fn standard() -> Option<Self> {
        let base = match crate::env::data() {
            Some(dir) => dir,
            None => dirs::data_local_dir()?.join("berth"),
        };
        let run = match (crate::env::data(), dirs::runtime_dir()) {
            (None, Some(runtime)) => runtime.join("berth"),
            _ => base.join("run"),
        };
        Some(Self {
            run,
            bin: base.join("bin"),
        })
    }

    /// Both directories, created, and the runtime one made private.
    pub fn prepare(&self) -> io::Result<()> {
        make_private(&self.run)?;
        std::fs::create_dir_all(&self.bin)?;
        Ok(())
    }

    pub fn supervisor(&self) -> PathBuf {
        endpoint(&self.run, "s")
    }

    pub fn worker(&self, session: SessionId) -> PathBuf {
        endpoint(&self.run, &format!("w{session}"))
    }

    pub fn supervisor_pid(&self) -> PathBuf {
        self.run.join("s.pid")
    }

    pub fn worker_pid(&self, session: SessionId) -> PathBuf {
        self.run.join(format!("w{session}.pid"))
    }
}

/// An endpoint's address: a socket file in the runtime directory, or on
/// Windows a pipe named after it, so two users' and two data directories'
/// servers never meet.
fn endpoint(run: &Path, name: &str) -> PathBuf {
    #[cfg(unix)]
    {
        run.join(format!("{name}.sock"))
    }
    #[cfg(windows)]
    {
        use sha2::{Digest, Sha256};
        let tag = hex::encode(&Sha256::digest(run.to_string_lossy().as_bytes())[..6]);
        PathBuf::from(format!(r"\\.\pipe\berth-{tag}-{name}"))
    }
}

/// Create `dir` and make it the user's alone. On Unix, setting the mode fails
/// unless the user owns it, so a directory someone else made first is refused
/// rather than used.
pub fn make_private(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn write_pid(path: &Path) -> io::Result<()> {
    std::fs::write(path, std::process::id().to_string())
}

pub fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Whether a process is running. Blocking: call it at startup, not per frame.
pub fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // `kill -0` asks the kernel without signalling anything, and needs
        // no libc binding.
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // STILL_ACTIVE: the exit code of a process that has not exited.
        const STILL_ACTIVE: u32 = 259;
        // SAFETY: a handle from OpenProcess, checked and closed.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code) != 0;
            CloseHandle(handle);
            ok && code == STILL_ACTIVE
        }
    }
}

/// Workers whose pid files name a running process. Entries for processes
/// that are gone are removed, with their endpoints.
pub fn live_workers(places: &Places) -> Vec<SessionId> {
    let Ok(entries) = std::fs::read_dir(&places.run) else {
        return Vec::new();
    };
    let mut live = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(session) = name
            .strip_prefix('w')
            .and_then(|n| n.strip_suffix(".pid"))
            .and_then(|n| n.parse::<SessionId>().ok())
        else {
            continue;
        };
        match read_pid(&entry.path()) {
            Some(pid) if alive(pid) => live.push(session),
            _ => {
                tracing::info!(session, "removing a stale worker entry");
                let _ = std::fs::remove_file(entry.path());
                #[cfg(unix)]
                let _ = std::fs::remove_file(places.worker(session));
            }
        }
    }
    live.sort_unstable();
    live
}

/// This build's copy of the binary in `bin`, made if it is not there yet, and
/// its name. Workers run from it, never from the app bundle or install
/// directory: on Windows an update cannot replace an executable a running
/// worker uses, and on macOS an update never touches one (D50).
///
/// Named by version and a hash of the binary, so two builds that share a
/// version number never share a copy.
pub fn install_copy(bin: &Path, exe: &Path) -> io::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(exe)?;
    let tag = hex::encode(&Sha256::digest(&bytes)[..4]);
    let name = format!(
        "berth-{}-{tag}{}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::EXE_SUFFIX
    );
    let copy = bin.join(&name);
    if copy.is_file() {
        return Ok(copy);
    }
    std::fs::create_dir_all(bin)?;
    // Written aside and renamed, so a half-written copy is never run.
    let partial = bin.join(format!(".{name}.partial"));
    std::fs::write(&partial, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&partial, &copy)?;
    Ok(copy)
}

/// Delete every copy in `bin` except those named in `keep`: the current one
/// and any a running worker still uses. On Windows a copy still running
/// refuses to go, which is the same answer.
pub fn remove_unused_copies(bin: &Path, keep: &HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(bin) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("berth-") && !keep.contains(&name) {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => tracing::info!(%name, "removed an unused worker binary"),
                Err(error) => tracing::debug!(%name, %error, "kept a worker binary"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places(dir: &Path) -> Places {
        Places {
            run: dir.join("run"),
            bin: dir.join("bin"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_runtime_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let places = places(dir.path());
        std::fs::create_dir_all(&places.run).unwrap();
        std::fs::set_permissions(&places.run, std::fs::Permissions::from_mode(0o755)).unwrap();
        places.prepare().unwrap();
        let mode = std::fs::metadata(&places.run).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    /// The real places on this machine stay under macOS's 104-byte limit.
    #[cfg(unix)]
    #[test]
    fn endpoint_names_are_short() {
        let places = places(Path::new(
            "/Users/a-rather-long-user-name/Library/Application Support/berth",
        ));
        let path = places.worker(u64::MAX);
        assert!(path.as_os_str().len() < 104, "{}", path.display());
    }

    #[test]
    fn entries_for_dead_processes_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let places = places(dir.path());
        places.prepare().unwrap();
        write_pid(&places.worker_pid(3)).unwrap();
        // A pid that is not running: past the usual maximum on every platform.
        std::fs::write(places.worker_pid(4), "99999999").unwrap();
        std::fs::write(places.worker_pid(5), "not a pid").unwrap();
        assert_eq!(live_workers(&places), vec![3]);
        assert!(!places.worker_pid(4).exists());
        assert!(!places.worker_pid(5).exists());
    }

    #[test]
    fn this_process_is_alive() {
        assert!(alive(std::process::id()));
        assert!(!alive(99_999_999));
    }

    #[test]
    fn a_copy_is_made_once_and_old_ones_go() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("exe");
        std::fs::write(&exe, b"binary one").unwrap();
        let bin = dir.path().join("bin");
        let first = install_copy(&bin, &exe).unwrap();
        assert_eq!(install_copy(&bin, &exe).unwrap(), first);
        std::fs::write(&exe, b"binary two").unwrap();
        let second = install_copy(&bin, &exe).unwrap();
        assert_ne!(first, second, "a changed binary is a new copy");

        let keep: HashSet<String> = [second.file_name().unwrap().to_string_lossy().to_string()]
            .into_iter()
            .collect();
        remove_unused_copies(&bin, &keep);
        assert!(!first.exists());
        assert!(second.exists());
    }
}
