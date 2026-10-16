//! The server binaries this app carries for remote hosts (D55).
//!
//! A release has all four beside it, gzip-compressed, and their manifest
//! compiled in, so a file changed beside the app afterwards is refused
//! before it is uploaded. A build made without them carries none, and every
//! host stays on tmux.

use std::path::PathBuf;
use std::sync::OnceLock;

use berth_core::remote::{Manifest, Servers};

/// Where the server files sit: `Contents/Resources/servers` in the macOS
/// bundle, and `servers` beside the binary elsewhere.
fn beside_the_app() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    if cfg!(target_os = "macos") && dir.file_name().is_some_and(|n| n == "MacOS") {
        return Some(dir.parent()?.join("Resources").join("servers"));
    }
    Some(dir.join("servers"))
}

fn find() -> Option<Servers> {
    #[cfg(feature = "dev-server-dir")]
    if let Some(dir) = berth_core::env::server_dir() {
        return match Manifest::of_dir(&dir) {
            Ok(manifest) => {
                tracing::warn!(dir = %dir.display(), "servers come from BERTH_SERVER_DIR, unchecked");
                Some(Servers { manifest, dir })
            }
            Err(error) => {
                tracing::warn!(%error, "BERTH_SERVER_DIR could not be read");
                None
            }
        };
    }
    let text = berth_core::env::SERVER_MANIFEST?;
    match Manifest::parse(text) {
        Ok(manifest) if !manifest.is_empty() => Some(Servers {
            manifest,
            dir: beside_the_app()?,
        }),
        Ok(_) => None,
        Err(error) => {
            tracing::error!(%error, "this build's server manifest cannot be read");
            None
        }
    }
}

/// The servers this app carries, if it carries any.
pub fn carried() -> Option<&'static Servers> {
    static CARRIED: OnceLock<Option<Servers>> = OnceLock::new();
    CARRIED.get_or_init(find).as_ref()
}
