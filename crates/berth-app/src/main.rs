//! M0 spike: open a GPUI window and show what Berth knows before any
//! connection exists. The SSH transport lands on top of this shell.

mod bench;
mod icons;
mod input;
mod menu;
mod metrics;
mod otter;
mod panes;
mod programs;
mod reconnect;
mod render_probe;
mod restore;
mod runtime;
mod servers;
mod session;
mod sidebar;
mod strings;
mod terminal_style;
mod terminal_view;
mod text_field;
mod theme;
mod workspace;

use gpui::{Bounds, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size};
use gpui_platform::application;

use crate::workspace::Workspace;

/// The smallest check that would have caught B5.
///
/// Berth shipped for a day rendering an empty window because `gpui_platform`
/// was built without `font-kit`, and nothing failed: `add_fonts` returned `Ok`,
/// layout returned sensible metrics, and paint reported no error. The one
/// observable that was wrong is how many font families the platform can see,
/// so that is what CI asserts. It is not a substitute for checking that pixels
/// are lit, but it is exact, needs no window server, and runs in a second.
fn self_check() {
    // Any real macOS or Windows install has hundreds. gpui's own fallback list
    // is eleven, which is what a text system with no font source reports.
    const FLOOR: usize = 30;

    application().with_assets(crate::icons::Icons).run(|cx| {
        let names = cx.text_system().all_font_names();
        let count = names.len();
        if count < FLOOR {
            eprintln!(
                "self-check FAILED: the text system sees {count} font families, \
                 which is below {FLOOR} and means it has no font source. Check that \
                 gpui_platform is built with the font-kit feature. See decisions.md B5."
            );
            std::process::exit(1);
        }
        println!("self-check ok: {count} font families visible");
        cx.quit();
    });
}

/// Open the window the pixel check photographs, and hold it open.
///
/// Prints the window's bounds so the test does not have to duplicate them, then
/// stays up until killed.
fn render_probe() {
    application().with_assets(crate::icons::Icons).run(|cx| {
        let bounds = crate::render_probe::window_bounds();
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Berth render probe".into()),
                    ..Default::default()
                }),
                inactive_frame_interval: None,
                ..Default::default()
            },
            |_, cx| cx.new(|_| crate::render_probe::RenderProbe),
        );
        if let Err(error) = window {
            eprintln!("render probe could not open a window: {error}");
            std::process::exit(1);
        }
        cx.activate(true);
        // The check reads its geometry from here rather than duplicating it,
        // so the two can never drift apart.
        println!(
            "PROBE window={},{},{},{} text={},{} control={},{}",
            crate::render_probe::WINDOW_X,
            crate::render_probe::WINDOW_Y,
            crate::render_probe::WINDOW_W,
            crate::render_probe::WINDOW_H,
            crate::render_probe::TEXT_TOP,
            crate::render_probe::TEXT_HEIGHT,
            crate::render_probe::CONTROL_TOP,
            crate::render_probe::CONTROL_HEIGHT,
        );
        use std::io::Write;
        let _ = std::io::stdout().flush();
    });
}

fn main() -> std::process::ExitCode {
    // Claude's hook reporting to a worker, on every Claude event: before
    // anything else, so nothing but the report is paid for (no logging, no
    // window, no GPU).
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--hook")
        && let Some(code) = berth_core::server::cli::run(&args)
    {
        return code;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "berth=info".into()),
        )
        .init();

    // Berth's own server (D50): the same binary, started without a window.
    if let Some(code) = berth_core::server::cli::run(&args) {
        return code;
    }

    if std::env::args().any(|a| a == "--render-probe") {
        render_probe();
        return std::process::ExitCode::SUCCESS;
    }

    if std::env::args().any(|a| a == "--self-check") {
        self_check();
        return std::process::ExitCode::SUCCESS;
    }

    application().with_assets(crate::icons::Icons).run(|cx| {
        let bounds = Bounds::centered(None, size(px(1100.0), px(720.0)), cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(strings::APP_NAME.into()),
                    ..Default::default()
                }),
                // Frame throttling for an unfocused window would show up as
                // render latency that the user would never actually see.
                inactive_frame_interval: None,
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| {
                    // A window that becomes active is drawn: the local server's
                    // sessions send again (D50).
                    cx.observe_window_activation(window, |this: &mut Workspace, window, cx| {
                        if window.is_window_active() {
                            this.window_activated(cx);
                        }
                    })
                    .detach();
                    Workspace::new(cx)
                })
            },
        );

        if let Err(error) = window {
            tracing::error!(%error, "failed to open window");
            return;
        }
        cx.activate(true);
    });

    // Zero unless a benchmark run failed its criteria. Everything else that
    // ends the process does so through `cx.quit()` and lands here.
    bench::exit_code()
}
