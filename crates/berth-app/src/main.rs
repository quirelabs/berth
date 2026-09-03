//! M0 spike: open a GPUI window and show what Berth knows before any
//! connection exists. The SSH transport lands on top of this shell.

mod bench;
mod input;
mod metrics;
mod runtime;
mod session;
mod strings;
mod terminal_view;
mod theme;
mod workspace;

use gpui::{Bounds, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size};
use gpui_platform::application;

use crate::workspace::Workspace;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "berth=info".into()),
        )
        .init();

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
