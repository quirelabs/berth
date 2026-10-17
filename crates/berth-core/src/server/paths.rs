//! Where the server keeps its endpoints, pid files and binaries (D50, D55).
//!
//! The runtime directory is private to the user, is never `$TMPDIR` (macOS
//! clears files there that have gone untouched for about three days, so a
//! week-long session would lose its socket while it runs), is never
//! `$XDG_RUNTIME_DIR` (logind removes `/run/user/<uid>` at the user's last
//! logout, which would leave a supervisor that outlives the connection with
//! no socket anyone can reach), and holds only short names: a socket path is
//! limited to 104 bytes on macOS and 108 on Linux.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use berth_proto::v1::SessionId;

/// Where one server's files live.
#[derive(Debug, Clone)]
pub struct Places {
    /// Endpoints and pid files. Private to the user.
    pub run: PathBuf,
    /// One directory per build, holding the binary that build's processes
    /// run from.
    pub bin: PathBuf,
}

impl Places {
    /// This server's places, the first of: `BERTH_DATA`, which moves all of
    /// it for tests; the directory this binary is installed in, when it runs
    /// from `<data>/bin/<build>/`, so a server on a host keeps its files
    /// where it was put (D55); the platform's local data directory
    /// (`~/Library/Application Support/berth` on macOS, `%LOCALAPPDATA%\berth`
    /// on Windows, `~/.local/share/berth` elsewhere).
    pub fn standard() -> Option<Self> {
        if let Some(base) = crate::env::data() {
            return Some(Self::under(&base));
        }
        if let Some(base) = std::env::current_exe()
            .ok()
            .and_then(|exe| installed_under(&exe))
        {
            return Some(Self::under(&base));
        }
        Some(Self::under(&dirs::data_local_dir()?.join("berth")))
    }

    /// The places under one data directory: `bin/` and `run/` inside it. The
    /// runtime directory moves to `/tmp/berth-<uid>/` only when a socket
    /// there would have too long a path to exist at all.
    pub fn under(base: &Path) -> Self {
        let run = base.join("run");
        #[cfg(unix)]
        let run = if fits(&run) {
            run
        } else {
            fallback_run(base).unwrap_or(run)
        };
        Self {
            run,
            bin: base.join("bin"),
        }
    }

    /// Both directories, created, and the runtime one made private.
    pub fn prepare(&self) -> io::Result<()> {
        #[cfg(unix)]
        if let Some(shared) = self.run.parent().filter(|p| is_fallback_root(p)) {
            make_own_in_tmp(shared)?;
        }
        make_private(&self.run)?;
        std::fs::create_dir_all(&self.bin)?;
        Ok(())
    }

    /// Before connecting to anything here: a runtime directory in `/tmp`
    /// must already be this user's alone, or whoever made it first could be
    /// the one answering. Nothing is created; under the data directory
    /// there is nothing to check.
    pub fn trusted(&self) -> io::Result<()> {
        #[cfg(unix)]
        if let Some(shared) = self.run.parent().filter(|p| is_fallback_root(p)) {
            own_in_tmp(shared)?;
        }
        Ok(())
    }

    pub fn supervisor(&self) -> PathBuf {
        endpoint(&self.run, "s")
    }

    pub fn worker(&self, session: SessionId) -> PathBuf {
        endpoint(&self.run, &format!("w{session}"))
    }

    /// Where a worker's Claude hook reports (D50).
    pub fn hook(&self, session: SessionId) -> PathBuf {
        endpoint(&self.run, &format!("h{session}"))
    }

    pub fn supervisor_pid(&self) -> PathBuf {
        self.run.join("s.pid")
    }

    /// Held while a supervisor is being started, so two arriving together
    /// start one (D55).
    pub fn start_lock(&self) -> PathBuf {
        self.run.join("start.lock")
    }

    pub fn worker_pid(&self, session: SessionId) -> PathBuf {
        self.run.join(format!("w{session}.pid"))
    }
}

/// The data directory a binary at `<data>/bin/<build>/<name>` is installed
/// in.
fn installed_under(exe: &Path) -> Option<PathBuf> {
    let build = exe.parent()?;
    let bin = build.parent()?;
    let named = is_build(build.file_name()?.to_str()?) && bin.file_name()? == "bin";
    named.then(|| bin.parent().map(Path::to_path_buf)).flatten()
}

/// The longest socket path the platform takes, less the byte that ends it.
#[cfg(unix)]
const SOCKET_PATH_AT_MOST: usize = if cfg!(target_os = "macos") { 103 } else { 107 };

/// Whether the longest endpoint name there is fits in `run`.
#[cfg(unix)]
fn fits(run: &Path) -> bool {
    endpoint(run, &format!("w{}", SessionId::MAX))
        .as_os_str()
        .len()
        <= SOCKET_PATH_AT_MOST
}

/// Where the runtime files go when the data directory's path is too long
/// for a socket: `/tmp/berth-<uid>/<data directory's tag>`, as tmux keeps
/// its sockets. `/tmp` is shared, so the directory is checked to be the
/// user's alone before anything is put in it ([`make_own_in_tmp`]).
#[cfg(unix)]
fn fallback_run(base: &Path) -> Option<PathBuf> {
    use sha2::{Digest, Sha256};
    let tag = hex::encode(&Sha256::digest(base.as_os_str().as_encoded_bytes())[..6]);
    Some(PathBuf::from(format!("/tmp/berth-{}", own_uid()?)).join(tag))
}

#[cfg(unix)]
/// Whether `dir` is this user's `/tmp/berth-<uid>`, exactly: a data
/// directory that only happens to sit in `/tmp` with a name starting
/// `berth-`, as a benchmark's does, is not it.
fn is_fallback_root(dir: &Path) -> bool {
    let Some(uid) = own_uid() else {
        return false;
    };
    dir == Path::new(&format!("/tmp/berth-{uid}"))
}

/// This user's id, asked of `id` once: the standard library has no call for
/// it, and it is needed only on the rare path above.
#[cfg(unix)]
fn own_uid() -> Option<u32> {
    static UID: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *UID.get_or_init(|| {
        let out = std::process::Command::new("id").arg("-u").output().ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    })
}

/// Create `dir` under `/tmp` for this user alone, or refuse it: it must be a
/// real directory, not a link, owned by this user, with nothing allowed to
/// anyone else. Someone else could have made it first.
#[cfg(unix)]
fn make_own_in_tmp(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    own_in_tmp(dir)
}

/// Whether `dir` under `/tmp` is this user's alone: a real directory, not a
/// link, owned by this user, with nothing allowed to anyone else.
#[cfg(unix)]
fn own_in_tmp(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(dir)?;
    let own = own_uid().is_some_and(|uid| uid == meta.uid());
    if !meta.file_type().is_dir() || !own || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not this user's alone", dir.display()),
        ));
    }
    Ok(())
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

/// How many hex digits of a binary's SHA-256 name its build.
const BUILD_ID_DIGITS: usize = 16;

/// The build a binary with these bytes is: this version and the start of
/// their SHA-256, `0.1.0+3f9c2a1b7d4e5f60`.
pub fn build_named(bytes: &[u8]) -> String {
    build_with(env!("CARGO_PKG_VERSION"), &sha256_hex(bytes))
}

/// A build's name from its version and its binary's whole SHA-256 in hex.
pub fn build_with(version: &str, sha256: &str) -> String {
    let id = sha256.get(..BUILD_ID_DIGITS).unwrap_or(sha256);
    format!("{version}+{id}")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// The build the binary at `exe` is. One installed under `bin/<build>/` is
/// taken at its directory's word: a worker starts often, and should not read
/// its whole binary to say hello.
pub fn build_of(exe: &Path) -> io::Result<String> {
    let named = exe
        .parent()
        .and_then(|dir| dir.file_name())
        .and_then(|n| n.to_str())
        .filter(|name| is_build(name) && name.starts_with(concat!(env!("CARGO_PKG_VERSION"), "+")));
    match named {
        Some(build) => Ok(build.to_string()),
        None => Ok(build_named(&std::fs::read(exe)?)),
    }
}

/// Whether `name` is shaped like a build: a version, `+`, and the id. It is
/// used as a directory name, so nothing that could step out of one passes.
pub fn is_build(name: &str) -> bool {
    let Some((version, id)) = name.split_once('+') else {
        return false;
    };
    let plain = !version.is_empty()
        && !version.starts_with('.')
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    plain && id.len() == BUILD_ID_DIGITS && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// This build's copy of the binary, `bin/<build>/<name>`, made if it is not
/// there yet. Workers run from it, never from the app bundle or install
/// directory: on Windows an update cannot replace an executable a running
/// worker uses, and on macOS an update never touches one (D50). The layout
/// is the one a host's install has (D55).
pub fn install_copy(bin: &Path, exe: &Path) -> io::Result<PathBuf> {
    let bytes = std::fs::read(exe)?;
    let name = exe
        .file_name()
        .ok_or_else(|| io::Error::other("the binary has no name"))?;
    let dir = bin.join(build_named(&bytes));
    let copy = dir.join(name);
    if copy.is_file() {
        return Ok(copy);
    }
    make_private(&dir)?;
    // Written aside and renamed, so a half-written copy is never run.
    let partial = dir.join(format!(".{}.partial", name.to_string_lossy()));
    std::fs::write(&partial, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o700))?;
    }
    std::fs::rename(&partial, &copy)?;
    Ok(copy)
}

/// Delete every build in `bin` except those named in `keep`: the current one
/// and any a running worker still uses. Only directories named as builds are
/// touched. On Windows a binary still running refuses to go, which is the
/// same answer.
pub fn remove_unused_builds(bin: &Path, keep: &HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(bin) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir && is_build(&name) && !keep.contains(&name) {
            match std::fs::remove_dir_all(entry.path()) {
                Ok(()) => tracing::info!(%name, "removed an unused build"),
                Err(error) => tracing::debug!(%name, %error, "kept a build"),
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

    /// Everything is under the data directory, the runtime files too.
    #[test]
    fn the_runtime_directory_is_inside_the_data_directory() {
        let places = Places::under(Path::new("/home/u/.local/share/berth"));
        assert_eq!(places.run, Path::new("/home/u/.local/share/berth/run"));
        assert_eq!(places.bin, Path::new("/home/u/.local/share/berth/bin"));
    }

    /// A data directory too deep for a socket keeps its runtime files in
    /// `/tmp/berth-<uid>/`, apart from any other data directory's, and that
    /// directory is made for this user alone.
    #[cfg(unix)]
    #[test]
    fn a_path_too_long_for_a_socket_falls_back_to_tmp() {
        let deep = |name: &str| PathBuf::from(format!("/home/{}/{name}", "a".repeat(90)));
        let places = Places::under(&deep("one"));
        let root = PathBuf::from(format!("/tmp/berth-{}", own_uid().unwrap()));
        assert_eq!(places.run.parent(), Some(root.as_path()), "{places:?}");
        assert!(fits(&places.run));
        assert!(is_fallback_root(&root));
        // A data directory in /tmp named like it is not it.
        assert!(!is_fallback_root(Path::new("/tmp/berth-bench-3874")));
        assert!(!is_fallback_root(&root.join("run")));
        // And nothing is asked of it before connecting.
        Places::under(Path::new("/tmp/berth-bench-1"))
            .trusted()
            .unwrap();
        assert_ne!(places.run, Places::under(&deep("two")).run);
        assert_eq!(places.bin, deep("one").join("bin"), "only the sockets move");
    }

    /// The shared directory in `/tmp` is refused unless it is a real
    /// directory of this user's with nothing open to anyone else.
    #[cfg(unix)]
    #[test]
    fn a_directory_in_tmp_someone_else_could_use_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ours = dir.path().join("berth-1");
        make_own_in_tmp(&ours).unwrap();
        let mode = std::fs::metadata(&ours).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        make_own_in_tmp(&ours).unwrap();

        std::fs::set_permissions(&ours, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(make_own_in_tmp(&ours).is_err(), "open to others");

        let link = dir.path().join("berth-2");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        assert!(make_own_in_tmp(&link).is_err(), "a link to somewhere else");
    }

    /// A client only looks: a directory in `/tmp` that is missing, open to
    /// others or a link is refused before anything connects, and nothing is
    /// made.
    #[cfg(unix)]
    #[test]
    fn a_client_checks_the_directory_in_tmp_before_it_connects() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("berth-3");
        assert!(own_in_tmp(&missing).is_err());
        assert!(!missing.exists(), "checking makes nothing");

        let open = dir.path().join("berth-4");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(own_in_tmp(&open).is_err(), "open to others");
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o700)).unwrap();
        own_in_tmp(&open).unwrap();

        let link = dir.path().join("berth-5");
        std::os::unix::fs::symlink(&open, &link).unwrap();
        assert!(own_in_tmp(&link).is_err(), "a link, even to our own");

        // Under a data directory there is nothing to check.
        Places::under(dir.path()).trusted().unwrap();
    }

    /// A server installed on a host keeps its files where it was put.
    #[test]
    fn an_installed_binary_names_its_data_directory() {
        let exe = Path::new("/home/u/.local/share/berth/bin/0.1.0+0123456789abcdef/berth-server");
        assert_eq!(
            installed_under(exe).as_deref(),
            Some(Path::new("/home/u/.local/share/berth"))
        );
        assert_eq!(
            installed_under(Path::new("/usr/local/bin/berth-server")),
            None
        );
        assert_eq!(installed_under(Path::new("/w/target/debug/berth")), None);
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

    /// A copy lives in its build's directory, and says the same of itself
    /// without being read again; anything else is named by what is in it.
    #[test]
    fn a_build_is_named_by_its_version_and_what_is_in_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("berth-server");
        std::fs::write(&exe, b"binary one").unwrap();
        let one = build_of(&exe).unwrap();
        assert!(is_build(&one), "{one}");
        assert!(one.starts_with(concat!(env!("CARGO_PKG_VERSION"), "+")));

        let bin = dir.path().join("bin");
        let copy = install_copy(&bin, &exe).unwrap();
        assert_eq!(copy, bin.join(&one).join("berth-server"));
        assert_eq!(
            build_of(&copy).unwrap(),
            one,
            "a copy is the build it copies"
        );
        assert_eq!(installed_under(&copy).as_deref(), Some(dir.path()));

        std::fs::write(&exe, b"binary two").unwrap();
        assert_ne!(build_of(&exe).unwrap(), one, "same version, another build");
    }

    /// What an older build called itself, and anything that could step out
    /// of the directory, is no build's name.
    #[test]
    fn only_a_build_name_names_a_directory() {
        assert!(is_build("0.1.0+0123456789abcdef"));
        assert!(is_build("0.2.0-rc.1+0123456789abcdef"));
        assert!(!is_build("berth 0.1.0 (berth-0.1.0-ab12cd34)"));
        assert!(!is_build("0.1.0"));
        assert!(!is_build("0.1.0+short"));
        assert!(!is_build("../x+0123456789abcdef"));
        assert!(!is_build("..+0123456789abcdef"));
        assert!(!is_build("+0123456789abcdef"));
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
        // Not a build: left alone whatever is kept.
        std::fs::write(bin.join("claude"), b"shim").unwrap();
        std::fs::create_dir(bin.join("notes")).unwrap();

        let keep: HashSet<String> = [build_of(&second).unwrap()].into_iter().collect();
        remove_unused_builds(&bin, &keep);
        assert!(!first.exists());
        assert!(second.exists());
        assert!(bin.join("claude").exists());
        assert!(bin.join("notes").exists());
    }
}
