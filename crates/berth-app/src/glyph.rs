//! The status glyph (D53, "cursor"): a session's state as one terminal cell,
//! the way a terminal already speaks.
//!
//! Working is a solid block in the program's accent, blinking like a cursor;
//! needs you is an inverted cell, a gold block with a dark "?", with a slow
//! heartbeat until its pane has been focused; done is a hollow block in the
//! accent, the shape of an unfocused cursor; an error is a rose block with a
//! dark "!"; idle is a muted underscore. Every state has its own shape, so
//! colour is never the only signal.
//!
//! What a glyph shows at a moment is worked out here as plain data
//! ([`glyph`], [`pane_map`]), so the rules are tested without a window. Only
//! the blink and the heartbeat move, both on the one timer the workspace
//! shares and all in phase; sidebar glyphs never move, so a visible sidebar
//! costs no wakeups ([`next_wake`]).

use std::time::Duration;

use gpui::{Bounds, Pixels, Rgba, div, point, prelude::*, px, size};

use crate::panes::{Node, Rect};

/// What a session's programs say they are doing, as OSC 7501 and Claude's
/// hooks name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    Working,
    NeedsYou,
    Done,
    Error,
    Idle,
}

impl State {
    #[cfg(test)]
    pub const ALL: [State; 5] = [
        State::Working,
        State::NeedsYou,
        State::Done,
        State::Error,
        State::Idle,
    ];

    /// The state for what a program reported (OSC 7501).
    pub fn of_status(state: berth_proto::v1::StatusState) -> Self {
        use berth_proto::v1::StatusState;
        match state {
            StatusState::Working => State::Working,
            StatusState::Blocked => State::NeedsYou,
            StatusState::Done => State::Done,
            StatusState::Idle => State::Idle,
            StatusState::Error => State::Error,
        }
    }

    /// The state for what Claude's hooks said: a permission prompt needs
    /// you, a finished turn is done, and a Claude idle at its prompt or gone
    /// is idle.
    pub fn of_claude(state: berth_core::claude::State) -> Self {
        use berth_core::claude::{State as Claude, Wait};
        match state {
            Claude::Working => State::Working,
            Claude::Waiting(Wait::Permission) => State::NeedsYou,
            Claude::Waiting(Wait::YourMove) => State::Done,
            Claude::Waiting(Wait::Idle) | Claude::Finished | Claude::Interrupted => State::Idle,
        }
    }
}

/// A cursor's blink: on for half the period, off for the other half.
pub const BLINK: Duration = Duration::from_millis(1060);

/// The heartbeat: two dips, then a rest, every 2.4 seconds. Each dip is
/// centred on its time and lasts twice its half-width, in seconds, and
/// takes the block down to `DIP`.
pub const HEARTBEAT: Duration = Duration::from_millis(2400);
const DIPS: [f32; 2] = [0.2, 0.62];
const DIP_HALF: f32 = 0.16;
const DIP: f32 = 0.35;

/// The shortest wait between frames while a heartbeat dips: 30 a second.
pub const FRAME: Duration = Duration::from_micros(33_334);

/// A glyph's corners, and the hollow block's stroke.
pub const RADIUS: f32 = 1.5;
pub const STROKE: f32 = 1.5;

/// A tab's glyph is a terminal cell: 8 by 14 at the default font size,
/// following the font's cell. A sidebar's is fixed at 7 by 13, and a tab's
/// pane map at 15 by 14.
pub const SIDEBAR: (f32, f32) = (7.0, 13.0);
pub const PANE_MAP: (f32, f32) = (15.0, 14.0);
const DEFAULT_CELL: (f32, f32) = (8.0, 14.0);

/// The size of a tab's glyph for a terminal cell of `cell`, scaled so its
/// height is 14 at the default font size and its shape is the font's.
pub fn tab_size(cell: Option<(f32, f32)>, font_size: f32) -> (f32, f32) {
    let height = DEFAULT_CELL.1 * font_size / berth_core::settings::DEFAULT_FONT_SIZE.max(1.0);
    let (w, h) = cell
        .filter(|(w, h)| *w > 0.0 && *h > 0.0)
        .unwrap_or(DEFAULT_CELL);
    (height * w / h, height)
}

/// The colours a glyph is drawn in: the program's accent, and the ones that
/// belong to no program.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Colours {
    pub accent: Rgba,
    pub gold: Rgba,
    pub rose: Rgba,
    pub ink: Rgba,
    pub muted: Rgba,
}

/// When a glyph is drawn: `clock` is the time every glyph shares, so all
/// blink together; `since` how long the state has been showing; `steady`
/// that its pane has been focused since it began to need the user; `still`
/// that it never moves, with motion reduced or in the sidebar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Moment {
    pub clock: Duration,
    pub since: Duration,
    pub steady: bool,
    pub still: bool,
}

/// A glyph's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// A solid block.
    Block,
    /// A block outline, inset by the stroke.
    Hollow,
    /// A block with a mark on it, in ink.
    Marked(char),
    /// A bar along the bottom.
    Underscore,
}

/// A glyph at a moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyph {
    pub shape: Shape,
    pub colour: Rgba,
    pub ink: Rgba,
    /// 1 drawn in full, 0 not drawn: a blink's off half, a heartbeat's dip.
    pub opacity: f32,
}

/// Whether a blink is in its on half at `clock`.
pub fn blink_on(clock: Duration) -> bool {
    clock.as_millis() % BLINK.as_millis() < BLINK.as_millis() / 2
}

/// How visible the heartbeat leaves a block `since` it began: 1 at rest,
/// dipping to `DIP` twice a cycle.
pub fn heartbeat(since: Duration) -> f32 {
    let t = since.as_secs_f32() % HEARTBEAT.as_secs_f32();
    let dip = DIPS
        .iter()
        .map(|centre| {
            let off = (t - centre).abs();
            if off < DIP_HALF {
                0.5 * (1.0 + (std::f32::consts::PI * off / DIP_HALF).cos())
            } else {
                0.0
            }
        })
        .fold(0.0, f32::max);
    1.0 - (1.0 - DIP) * dip
}

/// When the heartbeat's last dip is over, within its cycle.
fn dips_end() -> f32 {
    DIPS[1] + DIP_HALF
}

/// What a state looks like at a moment.
pub fn glyph(state: State, colours: Colours, at: Moment) -> Glyph {
    let drawn = |shape, colour| Glyph {
        shape,
        colour,
        ink: colours.ink,
        opacity: 1.0,
    };
    match state {
        State::Working => Glyph {
            opacity: if at.still || blink_on(at.clock) {
                1.0
            } else {
                0.0
            },
            ..drawn(Shape::Block, colours.accent)
        },
        State::NeedsYou => Glyph {
            opacity: if at.still || at.steady {
                1.0
            } else {
                heartbeat(at.since)
            },
            ..drawn(Shape::Marked('?'), colours.gold)
        },
        State::Done => drawn(Shape::Hollow, colours.accent),
        State::Error => drawn(Shape::Marked('!'), colours.rose),
        State::Idle => drawn(Shape::Underscore, colours.muted),
    }
}

/// How long until any glyph on screen draws differently, for the one timer:
/// `moving` is each tab glyph that may move, with how long its state has
/// shown and whether it has gone steady. `None` sets no timer: nothing
/// moves, the window is not drawn (`seen` false), or motion is reduced.
pub fn next_wake(
    moving: &[(State, Duration, bool)],
    clock: Duration,
    still: bool,
    seen: bool,
) -> Option<Duration> {
    if !seen || still {
        return None;
    }
    let half = BLINK.as_millis() / 2;
    moving
        .iter()
        .filter_map(|(state, since, steady)| match state {
            // The next edge of the blink, so every glyph flips together.
            State::Working => {
                let left = half - clock.as_millis() % half;
                Some(Duration::from_millis(left as u64).max(Duration::from_millis(1)))
            }
            State::NeedsYou if !steady => {
                let t = since.as_secs_f32() % HEARTBEAT.as_secs_f32();
                if t < dips_end() {
                    Some(FRAME)
                } else {
                    Some(Duration::from_secs_f32(HEARTBEAT.as_secs_f32() - t).max(FRAME))
                }
            }
            _ => None,
        })
        .min()
}

/// One cell of a tab's pane map: where it sits in the 15 by 14 map, and its
/// pane's glyph.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapCell {
    pub rect: Rect,
    pub glyph: Glyph,
}

/// A tab's pane map, from the real split geometry: one cell per pane, each
/// drawn as its pane's state is. Up to four panes show the layout as it is;
/// five or more show the first three as strips. Cells are kept a pixel
/// apart where panes meet.
pub fn pane_map(root: &Node, glyph_of: impl Fn(u64) -> Glyph) -> Vec<MapCell> {
    let (w, h) = PANE_MAP;
    let panes = root.panes();
    let rects: Vec<(u64, Rect)> = if panes.len() <= 4 {
        root.rects((0.0, 0.0, w, h))
    } else {
        let strip = w / 3.0;
        panes
            .iter()
            .take(3)
            .enumerate()
            .map(|(i, id)| (*id, (strip * i as f32, 0.0, strip, h)))
            .collect()
    };
    rects
        .into_iter()
        .map(|(id, (x, y, cw, ch))| {
            let right = if x + cw < w - 0.01 { 1.0 } else { 0.0 };
            let bottom = if y + ch < h - 0.01 { 1.0 } else { 0.0 };
            MapCell {
                rect: (x, y, (cw - right).max(1.0), (ch - bottom).max(1.0)),
                glyph: glyph_of(id),
            }
        })
        .collect()
}

/// A glyph, drawn `w` by `h`.
pub fn render(glyph: Glyph, (w, h): (f32, f32)) -> gpui::AnyElement {
    let cell = div()
        .flex_shrink_0()
        .w(px(w))
        .h(px(h))
        .opacity(glyph.opacity);
    match glyph.shape {
        Shape::Block => cell.rounded(px(RADIUS)).bg(glyph.colour).into_any_element(),
        Shape::Marked(mark) => cell
            .rounded(px(RADIUS))
            .bg(glyph.colour)
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(h * 0.8))
            .line_height(px(h))
            .font_weight(gpui::FontWeight::BOLD)
            .text_color(glyph.ink)
            .child(mark.to_string())
            .into_any_element(),
        Shape::Hollow => cell
            .child(
                gpui::canvas(
                    |_, _, _| {},
                    move |bounds, (), window, _| paint_hollow(bounds, glyph.colour, STROKE, window),
                )
                .size_full(),
            )
            .into_any_element(),
        Shape::Underscore => cell
            .flex()
            .flex_col()
            .justify_end()
            .child(div().w_full().h(px(STROKE)).bg(glyph.colour))
            .into_any_element(),
    }
}

fn paint_hollow(bounds: Bounds<Pixels>, colour: Rgba, stroke: f32, window: &mut gpui::Window) {
    window.paint_quad(gpui::quad(
        bounds,
        px(RADIUS),
        gpui::transparent_black(),
        px(stroke),
        colour,
        gpui::BorderStyle::Solid,
    ));
}

/// A tab's pane map, drawn: plain filled and stroked rects, no text.
pub fn render_map(cells: Vec<MapCell>) -> gpui::AnyElement {
    let (w, h) = PANE_MAP;
    div()
        .flex_shrink_0()
        .w(px(w))
        .h(px(h))
        .child(
            gpui::canvas(
                |_, _, _| {},
                move |bounds, (), window, _| {
                    for cell in &cells {
                        let (x, y, cw, ch) = cell.rect;
                        let at = Bounds::new(
                            point(bounds.origin.x + px(x), bounds.origin.y + px(y)),
                            size(px(cw), px(ch)),
                        );
                        let colour = Rgba {
                            a: cell.glyph.colour.a * cell.glyph.opacity,
                            ..cell.glyph.colour
                        };
                        match cell.glyph.shape {
                            Shape::Block | Shape::Marked(_) => {
                                window.paint_quad(gpui::fill(at, colour).corner_radii(px(1.0)))
                            }
                            Shape::Hollow | Shape::Underscore => {
                                paint_hollow(at, colour, 1.0, window)
                            }
                        }
                    }
                },
            )
            .size_full(),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panes::Axis;

    fn colours() -> Colours {
        Colours {
            accent: gpui::rgb(0xd97757),
            gold: gpui::rgb(0xe9b949),
            rose: gpui::rgb(0xf25c86),
            ink: gpui::rgb(0x111114),
            muted: gpui::rgb(0x7a7a7c),
        }
    }

    fn at(clock_ms: u64, since_ms: u64) -> Moment {
        Moment {
            clock: Duration::from_millis(clock_ms),
            since: Duration::from_millis(since_ms),
            steady: false,
            still: false,
        }
    }

    /// Each state has a shape of its own, so colour is never the only
    /// signal.
    #[test]
    fn every_state_has_its_own_shape() {
        let shapes: Vec<Shape> = State::ALL
            .iter()
            .map(|s| glyph(*s, colours(), at(0, 0)).shape)
            .collect();
        assert_eq!(
            shapes,
            [
                Shape::Block,
                Shape::Marked('?'),
                Shape::Hollow,
                Shape::Marked('!'),
                Shape::Underscore
            ]
        );
    }

    /// The accent is only on working and done, gold only on needs you,
    /// rose only on an error.
    #[test]
    fn colours_belong_to_their_states() {
        let c = colours();
        for state in State::ALL {
            let g = glyph(state, c, at(0, 0));
            assert_eq!(
                g.colour == c.accent,
                matches!(state, State::Working | State::Done),
                "{state:?}"
            );
            assert_eq!(g.colour == c.gold, state == State::NeedsYou, "{state:?}");
            assert_eq!(g.colour == c.rose, state == State::Error, "{state:?}");
        }
    }

    /// Working blinks in hard steps every 1.06 s, in phase with every other
    /// glyph; still, it stays on.
    #[test]
    fn working_blinks_like_a_cursor() {
        let c = colours();
        let op = |ms| glyph(State::Working, c, at(ms, 0)).opacity;
        assert_eq!(op(0), 1.0);
        assert_eq!(op(529), 1.0);
        assert_eq!(op(530), 0.0);
        assert_eq!(op(1059), 0.0);
        assert_eq!(op(1060), 1.0);
        // A glyph that began a moment ago blinks with the rest.
        assert_eq!(glyph(State::Working, c, at(600, 3)).opacity, 0.0);
        let still = Moment {
            still: true,
            ..at(600, 0)
        };
        assert_eq!(glyph(State::Working, c, still).opacity, 1.0);
    }

    /// Two dips, then a rest, every 2.4 s, and never more than three flashes
    /// in any second; steady once the pane has been focused.
    #[test]
    fn the_heartbeat_dips_twice_and_goes_steady() {
        let ms = Duration::from_millis;
        let samples: Vec<f32> = (0..10_000).map(|t| heartbeat(ms(t))).collect();
        let dips: Vec<usize> = samples
            .windows(2)
            .enumerate()
            .filter(|(_, w)| w[0] > 0.7 && w[1] <= 0.7)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(dips.iter().filter(|d| **d < 2400).count(), 2);
        for start in 0..9_000 {
            let flashes = dips
                .iter()
                .filter(|d| (start..start + 1000).contains(*d))
                .count();
            assert!(flashes < 3, "{flashes} from {start} ms");
        }
        assert!((900..2400).all(|t| heartbeat(ms(t)) == 1.0), "a real rest");
        assert!(samples.iter().all(|v| (DIP - 1e-4..=1.0).contains(v)));

        let c = colours();
        assert!(glyph(State::NeedsYou, c, at(0, 200)).opacity < 0.5);
        let steady = Moment {
            steady: true,
            ..at(0, 200)
        };
        assert_eq!(glyph(State::NeedsYou, c, steady).opacity, 1.0);
    }

    /// Nothing moving, hidden or motion reduced: no timer. A blink wakes at
    /// its next edge, a dipping heartbeat at 30 a second, a resting one at
    /// its next beat; done, error and idle never.
    #[test]
    fn only_what_moves_wakes_the_timer() {
        let ms = Duration::from_millis;
        let quiet = [
            (State::Idle, ms(0), false),
            (State::Done, ms(0), false),
            (State::Error, ms(0), false),
            (State::NeedsYou, ms(100), true),
        ];
        assert_eq!(next_wake(&quiet, ms(10), false, true), None);
        assert_eq!(next_wake(&[], ms(10), false, true), None);
        let working = [(State::Working, ms(0), false)];
        assert_eq!(next_wake(&working, ms(100), false, true), Some(ms(430)));
        assert_eq!(next_wake(&working, ms(600), false, true), Some(ms(460)));
        let dipping = [(State::NeedsYou, ms(100), false)];
        assert_eq!(next_wake(&dipping, ms(0), false, true), Some(FRAME));
        let resting = next_wake(&[(State::NeedsYou, ms(1400), false)], ms(0), false, true).unwrap();
        assert!((resting.as_secs_f32() - 1.0).abs() < 1e-3, "{resting:?}");
        assert_eq!(next_wake(&working, ms(0), false, false), None, "hidden");
        assert_eq!(next_wake(&working, ms(0), true, true), None, "reduced");
    }

    #[test]
    fn a_tab_glyph_follows_the_font_cell() {
        let default = berth_core::settings::DEFAULT_FONT_SIZE;
        assert_eq!(tab_size(None, default), (8.0, 14.0));
        assert_eq!(tab_size(Some((7.8, 15.6)), default), (7.0, 14.0));
        let (w, h) = tab_size(Some((8.0, 14.0)), default * 2.0);
        assert!((w - 16.0).abs() < 1e-4 && (h - 28.0).abs() < 1e-4);
    }

    /// The map is the real layout up to four panes, each cell its pane's
    /// glyph; five or more are the first three as strips.
    #[test]
    fn the_pane_map_follows_the_split_geometry() {
        let c = colours();
        let mut root = Node::leaf(1);
        root.split(1, Axis::Horizontal, 2);
        root.split(2, Axis::Vertical, 3);
        let map = pane_map(&root, |id| {
            let state = if id == 3 {
                State::NeedsYou
            } else {
                State::Done
            };
            glyph(state, c, at(0, 0))
        });
        assert_eq!(map.len(), 3);
        assert_eq!(map[0].rect, (0.0, 0.0, 6.5, 14.0));
        assert_eq!(map[1].rect, (7.5, 0.0, 7.5, 6.0));
        assert_eq!(map[2].rect, (7.5, 7.0, 7.5, 7.0));
        assert_eq!(map[2].glyph.colour, c.gold);
        assert_eq!(map[0].glyph.shape, Shape::Hollow);

        root.split(3, Axis::Horizontal, 4);
        root.split(4, Axis::Horizontal, 5);
        let map = pane_map(&root, |_| glyph(State::Idle, c, at(0, 0)));
        assert_eq!(map.len(), 3, "the first three");
        assert!(
            map.iter()
                .all(|cell| cell.rect.1 == 0.0 && cell.rect.3 == 14.0)
        );
    }
}
