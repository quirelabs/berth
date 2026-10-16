//! The server's command-line modes, shared by the app binary (one binary to
//! sign on macOS, D50) and `berth-server`, the binary without a window that
//! Linux hosts run (D49).

use std::process::ExitCode;

/// Run a server mode if `args` (after the program name) asks for one.
/// `None` means none was asked for.
pub fn run(args: &[String]) -> Option<ExitCode> {
    let mode = args.first()?.as_str();
    if mode == "--hook" {
        super::hook::run_process(args);
        return Some(ExitCode::SUCCESS);
    }
    if mode == "--version" {
        // The build, as the handshake names it: what an install is checked
        // to be once it is on a host (D55).
        println!("{}", super::build());
        return Some(ExitCode::SUCCESS);
    }
    if mode == "--remove" {
        return Some(if super::remove::run_process(args) {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    if !matches!(
        mode,
        "--supervisor" | "--worker" | "--bench-throughput" | "--bench-engine"
    ) {
        return None;
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "could not start the server's runtime");
            return Some(ExitCode::FAILURE);
        }
    };
    let ok = runtime.block_on(async {
        match mode {
            "--supervisor" => super::supervisor::run_process().await.map(|()| true),
            "--worker" => super::worker_host::run_process(args).await.map(|()| true),
            "--bench-engine" => super::bench::run_engine(args).await,
            _ => super::bench::run_process(args).await,
        }
    });
    Some(match ok {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            tracing::error!(%error, "the server stopped");
            ExitCode::FAILURE
        }
    })
}
