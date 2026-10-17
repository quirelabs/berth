//! Berth's server and a real host (D55).
//!
//! Skipped unless `BERTH_TEST_HOST` names an alias in the user's ssh config.
//! The probe changes nothing on the host. The install runs only when
//! `BERTH_TEST_SERVERS` also names a directory of server builds as CI names
//! them, and writes only under `~/berth-bench` on the host, which it removes
//! again:
//!
//!     BERTH_TEST_HOST=berth-test BERTH_TEST_SERVERS=/path/to/builds \
//!         cargo test -p berth-core --test host_live -- --nocapture

use berth_core::config::SshConfig;
use berth_core::known_hosts::TrustPolicy;
use berth_core::remote::manifest::{Manifest, Servers};
use berth_core::remote::{Next, Runs, decide, install, probe};
use berth_core::transport::Connection;

fn target() -> Option<berth_core::Host> {
    let alias = std::env::var(berth_core::env::TEST_HOST).ok()?;
    let config = SshConfig::load_default().expect("ssh config should be readable");
    Some(
        config
            .get(&alias)
            .unwrap_or_else(|| panic!("BERTH_TEST_HOST={alias} is not in ssh config"))
            .clone(),
    )
}

async fn connect() -> Option<Connection> {
    let Some(host) = target() else {
        eprintln!("skipping: BERTH_TEST_HOST is not set");
        return None;
    };
    Some(
        Connection::connect(&host, TrustPolicy::KnownHostsOnly)
            .await
            .expect("should connect with the agent"),
    )
}

/// The host says what it is, whole, and the probe leaves nothing behind.
#[tokio::test]
async fn a_real_host_answers_the_probe() {
    let Some(connection) = connect().await else {
        return;
    };
    let found = probe(&connection, None).await.expect("the probe's answer");
    eprintln!("{found:#?}");
    assert!(
        found.target.is_some(),
        "no server for {} {}",
        found.os,
        found.arch
    );
    assert!(found.data.starts_with(&found.home), "{}", found.data);
    assert!(found.hash_tool.is_some());
    assert!(found.gzip);
    // The same again: nothing it did changed what it finds.
    assert_eq!(probe(&connection, None).await.unwrap(), found);
}

/// The whole install on a real host, under `~/berth-bench` and nowhere else:
/// probed, installed from the compressed build, found again, and removed by
/// the command the dialog gives.
#[tokio::test]
async fn the_server_installs_on_a_real_host_and_is_removed() {
    let Some(dir) = std::env::var_os(berth_core::env::TEST_SERVERS) else {
        eprintln!("skipping: BERTH_TEST_SERVERS is not set");
        return;
    };
    let Some(connection) = connect().await else {
        return;
    };
    // The builds as CI made them, each with a gzip copy made here, as the
    // bundle carries them: what goes up the wire is the compressed one,
    // unpacked on the host, so that path runs against a real host too.
    let packed = tempfile::tempdir().unwrap();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("berth-server-") || name.ends_with(".sha256") {
            continue;
        }
        std::fs::copy(&path, packed.path().join(name)).unwrap();
        let gz = std::process::Command::new("gzip")
            .args(["-9", "-c"])
            .arg(&path)
            .output()
            .expect("gzip");
        assert!(gz.status.success());
        std::fs::write(packed.path().join(format!("{name}.gz")), gz.stdout).unwrap();
    }
    let servers = Servers {
        manifest: Manifest::of_dir(packed.path()).expect("the server builds"),
        dir: packed.path().into(),
    };
    let home = probe(&connection, None).await.unwrap().home;
    assert!(home.starts_with('/') && home.len() > 1, "{home}");
    let scratch = format!("{home}/berth-bench");
    let data = format!("{scratch}/data");

    let found = probe(&connection, Some(&data)).await.unwrap();
    assert!(
        found.builds.is_empty(),
        "left from an earlier run: {found:?}"
    );
    let target = found.target.expect("a server for this host");
    let plan = match decide(&found, Some(&servers.manifest), None, true) {
        Next::Ask(plan) => plan,
        other => panic!("expected to ask: {other:?}"),
    };
    assert!(plan.path.starts_with(&scratch), "{}", plan.path);
    eprintln!("{plan:#?}\nremove with: {}", plan.removal());

    let payload = servers.payload(target, found.gzip).unwrap();
    eprintln!(
        "uploading {} bytes, packed: {}",
        payload.bytes.len(),
        payload.packed
    );
    assert!(found.gzip, "the host has gzip");
    assert!(payload.packed, "the compressed build goes up");
    let started = std::time::Instant::now();
    let done = install(&connection, &plan, &payload)
        .await
        .expect("the install");
    eprintln!("installed in {:?}: {done:?}", started.elapsed());
    assert!(done.hash_checked);

    let mode = connection
        .run(
            &format!("ls -ld '{}' '{}' '{data}'", plan.path, plan.dir),
            b"",
        )
        .await
        .unwrap();
    eprintln!("{}", mode.stdout_string());
    // The owner's alone: nothing for group or others. A setgid bit a home
    // directory hands down (`drwx--S---`) gives nobody anything.
    let modes: Vec<String> = mode
        .stdout_string()
        .lines()
        .filter_map(|line| line.split_whitespace().next().map(str::to_string))
        .collect();
    assert_eq!(modes.len(), 3, "{modes:?}");
    for mode in &modes {
        assert_eq!(&mode[1..4], "rwx", "{mode}");
        assert!(!mode[4..10].contains(['r', 'w', 'x']), "{mode}");
    }

    let found = probe(&connection, Some(&data)).await.unwrap();
    assert_eq!(found.builds, std::slice::from_ref(&plan.build));
    assert!(matches!(
        decide(&found, Some(&servers.manifest), None, false),
        Next::Use(_)
    ));

    // The transport (D55): an exec channel running the relay, read past
    // whatever the shell prints, greeted by the supervisor the relay starts,
    // of the build just installed. A second relay finds it running and asks
    // it, through the core, for its sessions: none. Both channels close
    // again: the supervisor, detached, holds neither open.
    let stream = berth_core::remote::relay::open(&connection, &plan.path)
        .await
        .expect("the relay");
    let opened = berth_core::server::link::open(stream, &berth_core::server::link::hello())
        .await
        .expect("the supervisor greeted");
    assert_eq!(opened.theirs.build, plan.build);
    let second = berth_core::remote::relay::open(&connection, &plan.path)
        .await
        .expect("a second relay");
    let sessions = berth_core::server::client::core_over(second, berth_proto::core::Ask::Sessions)
        .await
        .expect("the core answered");
    assert!(sessions.is_empty(), "{sessions:?}");
    drop(opened);

    // Its own removal command, as given: the server finds its data directory
    // from where it is installed, and stops the supervisor it started.
    let removed = connection.run(&plan.removal(), b"").await.unwrap();
    eprintln!("{}{}", removed.stdout_string(), removed.stderr_string());
    assert!(removed.succeeded());
    let left = connection
        .run(&format!("rmdir '{scratch}'; ls -d '{scratch}' 2>&1"), b"")
        .await
        .unwrap();
    assert!(
        left.stdout_string().contains("No such file"),
        "something is left in {scratch}: {}",
        left.stdout_string()
    );
}
