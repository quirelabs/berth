//! The server installed for real, on this machine standing in for a host:
//! the probe's script, the upload through gzip, the hash check, the rename
//! and the first run are the commands a host would be sent, run through
//! `sh` (D55). Everything happens in a scratch directory.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use berth_core::remote::install::{self, Installed};
use berth_core::remote::manifest::{Manifest, Servers, Target};
use berth_core::remote::{Choice, Next, ThisMachine, decide, install as put, probe};

const SERVER: &str = env!("CARGO_BIN_EXE_berth-server");

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The servers an app would carry, with this machine's own build in them.
fn servers(dir: &Path, target: Target, packed: bool) -> Servers {
    std::fs::create_dir_all(dir).unwrap();
    let plain = dir.join(target.file());
    std::fs::copy(SERVER, &plain).unwrap();
    if packed {
        let gz = std::fs::File::create(dir.join(format!("{}.gz", target.file()))).unwrap();
        let status = std::process::Command::new("gzip")
            .args(["-9", "-n", "-c"])
            .arg(&plain)
            .stdout(gz)
            .status()
            .unwrap();
        assert!(status.success());
    }
    Servers {
        manifest: Manifest::of_dir(dir).unwrap(),
        dir: dir.to_path_buf(),
    }
}

#[tokio::test]
async fn a_host_is_probed_asked_about_installed_on_and_cleaned() {
    let scratch = tempfile::tempdir().unwrap();
    // A space in it, as macOS's own data directory has.
    let data = scratch.path().join("Application Support").join("Berth");
    let data_text = data.to_str().unwrap();

    let found = probe(&ThisMachine, Some(data_text)).await.unwrap();
    let target = found.target.expect("a target for this machine");
    assert!(found.builds.is_empty());
    assert!(!data.exists(), "the probe made nothing");

    let servers = servers(&scratch.path().join("servers"), target, true);
    let plan = match decide(&found, Some(&servers.manifest), None, true) {
        Next::Ask(plan) => plan,
        other => panic!("expected to ask: {other:?}"),
    };
    assert_eq!(
        plan.path,
        format!("{data_text}/bin/{}/berth-server", plan.build)
    );

    let payload = servers.payload(target, found.gzip).unwrap();
    assert!(payload.packed, "the compressed file is what is uploaded");
    assert!(payload.bytes.len() < plan.size as usize);
    let done = put(&ThisMachine, &plan, &payload).await.unwrap();
    assert_eq!(done, Installed { hash_checked: true });

    // The user's alone, all the way down, and nothing temporary left.
    let binary = Path::new(&plan.path);
    assert_eq!(mode(binary), 0o700);
    assert_eq!(mode(Path::new(&plan.dir)), 0o700);
    assert_eq!(mode(&data.join("bin")), 0o700);
    assert_eq!(mode(&data), 0o700);
    assert_eq!(std::fs::read_dir(&plan.dir).unwrap().count(), 1);
    assert_eq!(
        std::fs::read(binary).unwrap(),
        std::fs::read(SERVER).unwrap()
    );

    // Probed again, it is there and is used; the choice is not asked again.
    let found = probe(&ThisMachine, Some(data_text)).await.unwrap();
    assert_eq!(found.builds, std::slice::from_ref(&plan.build));
    for asking in [true, false] {
        let next = decide(
            &found,
            Some(&servers.manifest),
            Some(Choice::Install),
            asking,
        );
        assert!(matches!(next, Next::Use(_)), "{next:?}");
    }

    // Removed with its own command, as the dialog gives it.
    let removed = std::process::Command::new("sh")
        .args(["-c", &plan.removal()])
        .env("BERTH_DATA", &data)
        .output()
        .unwrap();
    assert!(removed.status.success(), "{removed:?}");
    assert!(!data.exists(), "nothing of Berth's is left");
    let found = probe(&ThisMachine, Some(data_text)).await.unwrap();
    assert!(found.builds.is_empty());
    let next = decide(&found, Some(&servers.manifest), Some(Choice::Install), true);
    assert!(
        matches!(next, Next::Ask(_)),
        "removed, so asked again: {next:?}"
    );
}

/// Without gzip on the host the binary is uploaded as it is, and arrives the
/// same.
#[tokio::test]
async fn the_plain_binary_installs_where_there_is_no_gzip() {
    let scratch = tempfile::tempdir().unwrap();
    let data = scratch.path().join("berth");
    let mut found = probe(&ThisMachine, data.to_str()).await.unwrap();
    found.gzip = false;
    let target = found.target.unwrap();
    let servers = servers(&scratch.path().join("servers"), target, true);
    let Next::Ask(plan) = decide(&found, Some(&servers.manifest), None, true) else {
        panic!("expected to ask");
    };
    let payload = servers.payload(target, found.gzip).unwrap();
    assert!(!payload.packed);
    put(&ThisMachine, &plan, &payload).await.unwrap();
    assert_eq!(
        std::fs::read(&plan.path).unwrap(),
        std::fs::read(SERVER).unwrap()
    );
}

/// A file that is not what the plan says is caught on the host by its hash,
/// and the directory made for it is gone again.
#[tokio::test]
async fn a_changed_file_is_caught_on_the_host_and_nothing_is_left() {
    let scratch = tempfile::tempdir().unwrap();
    let data = scratch.path().join("berth");
    let found = probe(&ThisMachine, data.to_str()).await.unwrap();
    let target = found.target.unwrap();
    let servers = servers(&scratch.path().join("servers"), target, false);
    let Next::Ask(plan) = decide(&found, Some(&servers.manifest), None, true) else {
        panic!("expected to ask");
    };
    let mut payload = servers.payload(target, false).unwrap();
    payload.bytes.push(0);
    let error = put(&ThisMachine, &plan, &payload).await.unwrap_err();
    assert!(
        matches!(error, install::Error::HashMismatch { .. }),
        "{error:?}"
    );
    assert!(!Path::new(&plan.dir).exists());
}
