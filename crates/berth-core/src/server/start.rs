//! Starting the supervisor, once (D50, D55).
//!
//! Whatever needs one, the app on this machine or a relay on a host, comes
//! here: if something answers at the supervisor's endpoint that is used,
//! and otherwise one is started under a lock in the runtime directory, so
//! two arriving together start one between them and the second uses it.

use std::path::Path;
use std::time::Duration;

use super::paths::Places;
use crate::error::{Error, Result};

/// How long a supervisor has to answer once started.
pub const WITHIN: Duration = Duration::from_secs(10);

/// Make sure a supervisor answers at `places`, starting `exe --supervisor`
/// if none does. Returns once one answers.
pub async fn ensure_running(places: &Places, exe: &Path) -> Result<()> {
    if answers(places).await {
        return Ok(());
    }
    places.prepare()?;
    let lock_path = places.start_lock();
    // Taken off the async threads: it waits for whoever holds it.
    let lock = tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        file.lock()?;
        Ok(file)
    })
    .await
    .map_err(|_| Error::Server("the start lock was not taken".into()))??;
    // Whoever held it may have started one meanwhile.
    if !answers(places).await {
        super::supervisor::spawn_detached(exe, &["--supervisor".to_string()], &[])?;
        let deadline = tokio::time::Instant::now() + WITHIN;
        while !answers(places).await {
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Server("the supervisor did not start in time".into()));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    drop(lock);
    Ok(())
}

/// Whether anything answers at the supervisor's endpoint, in a directory
/// that is this user's.
async fn answers(places: &Places) -> bool {
    places.trusted().is_ok()
        && super::transport::connect(&places.supervisor())
            .await
            .is_ok()
}
