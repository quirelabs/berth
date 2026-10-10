//! The status icon: a pixel sea otter (D53).
//!
//! One sprite system. The otter is Berth and never changes, the stone it
//! holds is the program (Claude's, Codex's, or a plain one for anything
//! else), and the pose is the state: tapping the stone while working, a
//! raised amber paw when it needs you, the stone held up when done, asleep in
//! kelp when idle, the stone dropped in the water on an error.
//!
//! Built from `assets/icons/otter.json`, the source of truth, not from an
//! image, so the colours follow the theme. Drawn at a whole number of device
//! pixels per sprite pixel and never scaled by anything else: a pixel
//! drawing scaled by a fraction is a smear.
//!
//! Only `working` and `needs_you` animate without end; `done` stops after
//! two seconds and `idle` and `error` are single frames, so a sidebar of idle
//! sessions costs nothing ([`Pose::next_change`]).

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use gpui::{Bounds, Rgba, canvas, div, fill, point, prelude::*, px, size};
use serde::Deserialize;

use crate::theme::{Appearance, Theme};

const SHEET: &str = include_str!("../assets/icons/otter.json");

/// How long `done` plays before it holds its first frame.
const DONE_PLAYS: Duration = Duration::from_secs(2);

/// What the otter is doing: the state, as OSC 7501 and Claude's hooks name
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pose {
    Working,
    NeedsYou,
    Done,
    Idle,
    Error,
}

impl Pose {
    #[cfg(test)]
    pub const ALL: [Pose; 5] = [
        Pose::Working,
        Pose::NeedsYou,
        Pose::Done,
        Pose::Idle,
        Pose::Error,
    ];

    fn key(self) -> &'static str {
        match self {
            Pose::Working => "working",
            Pose::NeedsYou => "needs_you",
            Pose::Done => "done",
            Pose::Idle => "idle",
            Pose::Error => "error",
        }
    }

    /// The pose for a state a program reported (OSC 7501).
    pub fn of_status(state: berth_proto::v1::StatusState) -> Self {
        use berth_proto::v1::StatusState;
        match state {
            StatusState::Working => Pose::Working,
            StatusState::Blocked => Pose::NeedsYou,
            StatusState::Done => Pose::Done,
            StatusState::Idle => Pose::Idle,
            StatusState::Error => Pose::Error,
        }
    }

    /// The pose for what Claude's hooks said, mapped the same way: a
    /// permission prompt needs you, a finished turn is done, and a Claude
    /// idle at its prompt or gone is idle.
    pub fn of_claude(state: berth_core::claude::State) -> Self {
        use berth_core::claude::{State, Wait};
        match state {
            State::Working => Pose::Working,
            State::Waiting(Wait::Permission) => Pose::NeedsYou,
            State::Waiting(Wait::YourMove) => Pose::Done,
            State::Waiting(Wait::Idle) | State::Finished | State::Interrupted => Pose::Idle,
        }
    }

    /// The frame to draw `shown` after this pose began, and how long until
    /// the next one, if another is coming. `None` is what lets the one timer
    /// stop: nothing to wake for.
    pub fn frame_at(self, shown: Duration, still: bool) -> (usize, Option<Duration>) {
        let Some(state) = sheet().and_then(|s| s.states.get(self.key())) else {
            return (0, None);
        };
        let frames = state.frames.len();
        if still || frames < 2 || state.frame_ms == 0 {
            return (0, None);
        }
        let every = u128::from(state.frame_ms);
        let at = shown.as_millis();
        let until_next = Duration::from_millis((every - at % every) as u64);
        let frame = (at / every) as usize % frames;
        if state.looped {
            return (frame, Some(until_next));
        }
        // Plays for a while, then rests on its first frame.
        if shown >= DONE_PLAYS {
            (0, None)
        } else {
            (frame, Some(until_next.min(DONE_PLAYS - shown)))
        }
    }
}

/// Whose stone the otter holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stone {
    Claude,
    Codex,
    /// Any program without a stone of its own.
    Plain,
}

impl Stone {
    #[cfg(test)]
    pub const ALL: [Stone; 3] = [Stone::Claude, Stone::Codex, Stone::Plain];

    fn key(self) -> &'static str {
        match self {
            Stone::Claude => "claude",
            Stone::Codex => "codex",
            Stone::Plain => "plain",
        }
    }

    /// The stone for a program by the name it reports (`app` in OSC 7501).
    pub fn of_app(app: Option<&str>) -> Self {
        let app = app.unwrap_or_default().to_ascii_lowercase();
        if app.starts_with("claude") {
            Stone::Claude
        } else if app.starts_with("codex") {
            Stone::Codex
        } else {
            Stone::Plain
        }
    }
}

#[derive(Deserialize)]
struct Sheet {
    size: (u16, u16),
    palette: HashMap<String, HashMap<String, String>>,
    providers: HashMap<String, Provider>,
    states: HashMap<String, State>,
    pixel_key: HashMap<char, String>,
}

#[derive(Deserialize)]
struct Provider {
    /// Per theme, like the palette: `light` and `dark`, each with `base`,
    /// `hi` and `shade`.
    colors: HashMap<String, HashMap<String, String>>,
    shape: Vec<String>,
}

#[derive(Deserialize)]
struct State {
    frame_ms: u32,
    #[serde(rename = "loop")]
    looped: bool,
    frames: Vec<Frame>,
}

#[derive(Deserialize)]
struct Frame {
    pixels: Vec<String>,
    stone: Place,
}

#[derive(Deserialize)]
struct Place {
    row: u16,
    col: u16,
    sunk: bool,
}

fn sheet() -> Option<&'static Sheet> {
    static PARSED: OnceLock<Option<Sheet>> = OnceLock::new();
    PARSED
        .get_or_init(|| match serde_json::from_str(SHEET) {
            Ok(sheet) => Some(sheet),
            Err(error) => {
                tracing::warn!(%error, "the otter sprite sheet did not parse");
                None
            }
        })
        .as_ref()
}

/// The sprite's size in sprite pixels: 20 by 14.
pub fn size_in_pixels() -> (u16, u16) {
    sheet().map_or((20, 14), |s| s.size)
}

/// A run of same-coloured pixels on one row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Run {
    pub x: u16,
    pub y: u16,
    pub len: u16,
    pub colour: Rgba,
}

fn hex(colour: &str) -> Option<Rgba> {
    let value = u32::from_str_radix(colour.strip_prefix('#')?, 16).ok()?;
    (colour.len() == 7).then(|| gpui::rgb(value))
}

/// One frame with its stone, as runs of pixels in the theme's colours.
/// `None` if the sheet does not have it, which a test rules out.
pub fn runs(pose: Pose, frame: usize, stone: Stone, appearance: Appearance) -> Option<Vec<Run>> {
    let sheet = sheet()?;
    let (width, height) = sheet.size;
    let theme = match appearance {
        Appearance::Dark => "dark",
        Appearance::Light => "light",
    };
    let palette = sheet.palette.get(theme)?;
    let frame = sheet.states.get(pose.key())?.frames.get(frame)?;
    let mut grid: Vec<Vec<Option<Rgba>>> = vec![vec![None; width as usize]; height as usize];
    for (y, row) in frame.pixels.iter().enumerate().take(height as usize) {
        for (x, key) in row.chars().enumerate().take(width as usize) {
            if key == '.' {
                continue;
            }
            let token = sheet.pixel_key.get(&key)?;
            grid[y][x] = Some(hex(palette.get(token)?)?);
        }
    }
    // The stone goes on after the pixels. Sunk, it is all in its shade and
    // cut off at the bottom edge.
    let provider = sheet.providers.get(stone.key())?;
    let stone_colours = provider.colors.get(theme)?;
    for (dy, row) in provider.shape.iter().enumerate() {
        for (dx, key) in row.chars().enumerate() {
            let name = match (key, frame.stone.sunk) {
                ('.', _) => continue,
                (_, true) => "shade",
                ('h', false) => "hi",
                (_, false) => "base",
            };
            let (x, y) = (frame.stone.col as usize + dx, frame.stone.row as usize + dy);
            if y < height as usize && x < width as usize {
                grid[y][x] = Some(hex(stone_colours.get(name)?)?);
            }
        }
    }
    let mut runs: Vec<Run> = Vec::new();
    for (y, row) in grid.iter().enumerate() {
        let mut x = 0;
        while x < row.len() {
            let Some(colour) = row[x] else {
                x += 1;
                continue;
            };
            let start = x;
            while x < row.len() && row[x] == Some(colour) {
                x += 1;
            }
            runs.push(Run {
                x: start as u16,
                y: y as u16,
                len: (x - start) as u16,
                colour,
            });
        }
    }
    Some(runs)
}

/// The icon: one frame, painted pixel for pixel. It takes the sprite's size
/// in layout pixels and draws at a whole number of device pixels per sprite
/// pixel, from a device pixel boundary, whatever the display's scale.
pub fn icon(pose: Pose, frame: usize, stone: Stone, theme: Theme) -> Option<gpui::Div> {
    let runs = runs(pose, frame, stone, theme.appearance)?;
    let (width, height) = size_in_pixels();
    Some(
        div()
            .flex_shrink_0()
            .w(px(f32::from(width)))
            .h(px(f32::from(height)))
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, (), window, _| {
                        let scale = window.scale_factor();
                        // Device pixels per sprite pixel: 1 at 1x, 2 on
                        // Retina, and the whole part at anything between.
                        let each = scale.floor().max(1.0);
                        let step = px(each / scale);
                        let snap = |v: gpui::Pixels| px((f32::from(v) * scale).round() / scale);
                        let origin = point(snap(bounds.origin.x), snap(bounds.origin.y));
                        for run in &runs {
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(
                                        origin.x + step * f32::from(run.x),
                                        origin.y + step * f32::from(run.y),
                                    ),
                                    size(step * f32::from(run.len), step),
                                ),
                                run.colour,
                            ));
                        }
                    },
                )
                .size_full(),
            ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pose, frame and stone in both themes builds: a missing colour
    /// or a malformed row in the sheet fails here, not as an icon that
    /// quietly does not draw.
    #[test]
    fn every_frame_builds_in_both_themes() {
        let sheet = sheet().expect("the sheet parses");
        assert_eq!(sheet.size, (20, 14));
        for pose in Pose::ALL {
            let state = &sheet.states[pose.key()];
            assert!(!state.frames.is_empty(), "{pose:?}");
            for (index, frame) in state.frames.iter().enumerate() {
                assert_eq!(frame.pixels.len(), 14, "{pose:?} {index}");
                assert!(frame.pixels.iter().all(|row| row.chars().count() == 20));
                for stone in Stone::ALL {
                    for appearance in [Appearance::Light, Appearance::Dark] {
                        let runs = runs(pose, index, stone, appearance)
                            .unwrap_or_else(|| panic!("{pose:?} {index} {stone:?}"));
                        assert!(runs.iter().all(|r| r.x + r.len <= 20 && r.y < 14));
                        assert!(runs.len() > 10);
                    }
                }
            }
        }
    }

    #[test]
    fn the_colours_follow_the_theme() {
        let light = runs(Pose::Idle, 0, Stone::Plain, Appearance::Light).unwrap();
        let dark = runs(Pose::Idle, 0, Stone::Plain, Appearance::Dark).unwrap();
        assert_eq!(light.len(), dark.len());
        assert_ne!(light[0].colour, dark[0].colour);
    }

    /// The stone is the only thing that differs between programs, and a sunk
    /// stone is drawn in one colour and cut at the water's edge.
    #[test]
    fn the_stone_is_the_program() {
        let claude = runs(Pose::Working, 0, Stone::Claude, Appearance::Light).unwrap();
        let codex = runs(Pose::Working, 0, Stone::Codex, Appearance::Light).unwrap();
        assert_ne!(claude, codex);
        let sunk = runs(Pose::Error, 0, Stone::Codex, Appearance::Light).unwrap();
        let shade = hex("#085041").unwrap();
        let stone: Vec<&Run> = sunk.iter().filter(|r| r.colour == shade).collect();
        // Rows 11 to 13 of a three-row block: all three fit, none below 13.
        assert_eq!(stone.len(), 3);
        assert!(stone.iter().all(|r| r.len == 4 && r.y >= 11));
    }

    /// Only working and needs-you ask to be woken for ever; done stops after
    /// two seconds; idle and error never ask.
    #[test]
    fn only_what_moves_asks_for_a_wakeup() {
        let ms = Duration::from_millis;
        assert_eq!(Pose::Working.frame_at(ms(0), false), (0, Some(ms(250))));
        assert_eq!(Pose::Working.frame_at(ms(300), false), (1, Some(ms(200))));
        assert_eq!(Pose::Working.frame_at(ms(500), false).0, 0);
        assert_eq!(Pose::NeedsYou.frame_at(ms(600), false), (1, Some(ms(400))));
        assert_eq!(Pose::Done.frame_at(ms(450), false), (1, Some(ms(350))));
        assert_eq!(Pose::Done.frame_at(ms(1900), false), (0, Some(ms(100))));
        assert_eq!(Pose::Done.frame_at(ms(2000), false), (0, None));
        assert_eq!(Pose::Idle.frame_at(ms(123), false), (0, None));
        assert_eq!(Pose::Error.frame_at(ms(123), false), (0, None));
        // Reduced motion: frame 0 of everything, and no wakeups.
        for pose in Pose::ALL {
            assert_eq!(pose.frame_at(ms(300), true), (0, None), "{pose:?}");
        }
    }

    #[test]
    fn states_and_names_map_as_the_sheet_says() {
        use berth_core::claude::{State, Wait};
        use berth_proto::v1::StatusState;
        assert_eq!(Pose::of_status(StatusState::Blocked), Pose::NeedsYou);
        assert_eq!(Pose::of_status(StatusState::Done), Pose::Done);
        assert_eq!(
            Pose::of_claude(State::Waiting(Wait::Permission)),
            Pose::NeedsYou
        );
        assert_eq!(Pose::of_claude(State::Waiting(Wait::YourMove)), Pose::Done);
        assert_eq!(Stone::of_app(Some("claude")), Stone::Claude);
        assert_eq!(Stone::of_app(Some("Codex-CLI")), Stone::Codex);
        assert_eq!(Stone::of_app(Some("make")), Stone::Plain);
        assert_eq!(Stone::of_app(None), Stone::Plain);
    }
}
