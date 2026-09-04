//! A window whose only job is to be photographed.
//!
//! B5 shipped for a day: Berth built, opened a window, and drew no text at all,
//! because `gpui_platform` was built without `font-kit`. Nothing errored, and a
//! compile check cannot see it. The font-count self-check would have caught that
//! particular cause, but it only proves the text system can *see* fonts, not
//! that a glyph reaches the screen. This proves the second thing the only way it
//! can be proved: by reading the pixels back.
//!
//! The geometry is fixed and deliberately generous. The text band sits well
//! below any titlebar so the check never depends on a decoration height, and
//! the control band below it must come back empty, which is what catches a
//! capture that photographed the wrong window.

use gpui::{
    Bounds, Context, IntoElement, ParentElement, Pixels, Render, Styled, Window, div, point, px,
    rgb, size,
};

/// Drawn at a size where a missing glyph cannot hide in antialiasing.
pub const PROBE_TEXT: &str = "BERTH RENDER PROBE";
pub const FONT_SIZE: f32 = 40.0;

pub const WINDOW_W: f32 = 600.0;
pub const WINDOW_H: f32 = 300.0;
pub const WINDOW_X: f32 = 120.0;
pub const WINDOW_Y: f32 = 120.0;

/// Offsets from the window's top-left, in points.
pub const TEXT_TOP: f32 = 150.0;
pub const TEXT_HEIGHT: f32 = 60.0;
/// Must photograph as pure background. A capture of the wrong window will not
/// be uniform here, and a window that drew nothing will pass the text band by
/// being empty too, so both bands are asserted together.
pub const CONTROL_TOP: f32 = 230.0;
pub const CONTROL_HEIGHT: f32 = 60.0;

/// Pure black and pure white so the threshold needs no tuning.
pub const BACKGROUND: u32 = 0x000000;
pub const FOREGROUND: u32 = 0xffffff;

pub fn window_bounds() -> Bounds<Pixels> {
    Bounds {
        origin: point(px(WINDOW_X), px(WINDOW_Y)),
        size: size(px(WINDOW_W), px(WINDOW_H)),
    }
}

pub struct RenderProbe;

impl Render for RenderProbe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(rgb(BACKGROUND)).child(
            div()
                .absolute()
                .top(px(TEXT_TOP))
                .left(px(20.0))
                .h(px(TEXT_HEIGHT))
                .text_size(px(FONT_SIZE))
                .text_color(rgb(FOREGROUND))
                .child(PROBE_TEXT),
        )
    }
}

// Enforced at compile time rather than by a test, because these are facts
// about constants and a broken band should never reach a runner.
//
// The bands must not overlap, or the control band would be asserted empty
// while holding the very text the other band is asserted to find. And both sit
// below any plausible titlebar, so the check never has to know how tall a
// window decoration is on either platform.
const _: () = {
    assert!(TEXT_TOP + TEXT_HEIGHT <= CONTROL_TOP);
    assert!(CONTROL_TOP + CONTROL_HEIGHT <= WINDOW_H);
    assert!(TEXT_TOP >= 60.0);
};
