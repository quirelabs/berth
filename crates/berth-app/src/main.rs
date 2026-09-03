//! M0 spike: open a GPUI window and show what Berth knows before any
//! connection exists. The SSH transport lands on top of this shell.

mod bench;
mod input;
mod metrics;
mod reconnect;
mod runtime;
mod session;
mod strings;
mod terminal_style;
mod terminal_view;
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

    application().run(|cx| {
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

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "berth=info".into()),
        )
        .init();

    if std::env::args().any(|a| a == "--self-check") {
        self_check();
        return;
    }

    application().run(|cx| {
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
            |_, cx| cx.new(Workspace::new),
        );

        if let Err(error) = window {
            tracing::error!(%error, "failed to open window");
            return;
        }
        cx.activate(true);
    });
}
