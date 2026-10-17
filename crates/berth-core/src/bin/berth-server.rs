//! Berth's server without the app: the supervisor, workers, Claude's hook and
//! the speed floor benchmark, for hosts with no window to show (D49, D50).
//! Nothing here links gpui.

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match berth_core::server::cli::run(&args) {
        Some(code) => code,
        None => {
            eprintln!(
                "usage: berth-server --version | --remove [--force] | --connect | --supervisor | --worker ... | --bench-throughput [kind ...] [--mb N]"
            );
            std::process::ExitCode::FAILURE
        }
    }
}
