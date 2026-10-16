//! The terminal's own colours (D53, "cursor"): Berth is your terminal with
//! awareness added, not an app with a terminal inside. The chrome is made
//! from the terminal theme's background and foreground, and set in its
//! font; it is a second surface beside the terminal's, and the two, with
//! gaps and spacing, are all the separation there is. No hairlines, icons
//! on tabs, outlines, glass or blur. Status is the only colour on screen.

use berth_term::terminal::{Palette, Rgb};
use gpui::{Hsla, Rgba, rgb};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// The terminal's background: panes, and the selected tab.
    pub background: Rgba,
    /// The chrome: the tab bar, the sidebar, the status bar, and the pane
    /// area the panes sit on.
    pub chrome: Rgba,
    /// What floats over everything, and a raised tile: menus, dialogs,
    /// tooltips.
    pub surface: Rgba,
    pub border: Rgba,
    pub text: Rgba,
    pub text_muted: Rgba,
    /// Not a colour of its own: what is focused or chosen is told by the
    /// foreground, since status is the only colour.
    pub accent: Rgba,
    /// Applied as a top border on hosts tagged `prod`.
    pub warning: Rgba,
    /// Status colours that belong to no program: gold for needs you, rose
    /// for an error, a lighter rose for an error's title, and the dark inks
    /// a "?" and a "!" are drawn in on them.
    pub gold: Rgba,
    pub rose: Rgba,
    pub rose_text: Rgba,
    pub gold_ink: Rgba,
    pub rose_ink: Rgba,
}

/// How far the chrome is from the terminal's background towards its
/// foreground: #131315 to #161618 in the mockup (D53).
const CHROME: f32 = 0.02;

/// The least contrast a status colour has against the background (D53),
/// as WCAG asks of graphics.
pub const STATUS_CONTRAST: f32 = 3.0;

fn rgba(colour: Rgb) -> Rgba {
    rgb((u32::from(colour.r) << 16) | (u32::from(colour.g) << 8) | u32::from(colour.b))
}

impl Theme {
    /// The chrome for a terminal theme: its background and foreground, and
    /// the rest mixed from those two.
    pub fn of_terminal(palette: &Palette) -> Self {
        let background = rgba(palette.background);
        let text = rgba(palette.foreground);
        let dark = appearance(background) == Appearance::Dark;
        let (gold, rose) = if dark {
            (rgb(0xe9b949), rgb(0xf25c86))
        } else {
            (rgb(0xa87508), rgb(0xd2335f))
        };
        let rose_text = if dark {
            mix(rose, rgb(0xffffff), 0.3)
        } else {
            rose
        };
        Self {
            background,
            // A shade off the terminal's, darker on a dark theme as a window
            // frame is, so the terminal reads as the brighter surface.
            // A shade off the terminal's towards the foreground: the
            // terminal reads as the deeper surface, the chrome around it.
            chrome: mix(background, text, CHROME),
            surface: mix(background, text, 0.06),
            border: mix(background, text, 0.14),
            text,
            text_muted: mix(background, text, 0.55),
            accent: text,
            warning: rgba(palette.ansi[1]),
            gold: legible(gold, background, text, STATUS_CONTRAST),
            rose: legible(rose, background, text, STATUS_CONTRAST),
            rose_text: legible(rose_text, background, text, STATUS_CONTRAST),
            gold_ink: rgb(0x1d1605),
            rose_ink: rgb(0x2a0b16),
        }
    }

    /// A program's accent as drawn on this theme: kept at least 3:1 against
    /// the background, moved towards the foreground only as far as that
    /// needs. A program with none is drawn in the foreground.
    pub fn accent_for(&self, accent: Option<Rgba>) -> Rgba {
        match accent {
            Some(accent) => legible(accent, self.background, self.text, STATUS_CONTRAST),
            None => self.text,
        }
    }

    /// The dot beside a host in the sidebar.
    pub fn state_colour(&self, state: berth_core::connection::ConnectionState) -> Rgba {
        use berth_core::connection::ConnectionState::*;
        match state {
            Connected => self.accent,
            Degraded | Reconnecting => self.warning,
            Connecting => self.text_muted,
            Disconnected => self.border,
        }
    }
}

/// Whether a terminal theme is dark or light, by its background.
pub fn appearance(background: Rgba) -> Appearance {
    if luminance(background) < 0.4 {
        Appearance::Dark
    } else {
        Appearance::Light
    }
}

/// A colour's relative luminance, as WCAG defines it.
pub fn luminance(colour: Rgba) -> f32 {
    let channel = |c: f32| {
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(colour.r) + 0.7152 * channel(colour.g) + 0.0722 * channel(colour.b)
}

/// The contrast ratio between two colours, from 1 to 21.
pub fn contrast(a: Rgba, b: Rgba) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// A colour in OKLab: lightness and two opponent axes.
#[cfg(test)]
fn oklab(colour: Rgba) -> [f32; 3] {
    let linear = |c: f32| {
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (linear(colour.r), linear(colour.g), linear(colour.b));
    let l = (0.412_221_47 * r + 0.536_332_54 * g + 0.051_445_99 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

/// How far apart two colours look: the distance in OKLab, times 100.
#[cfg(test)]
pub fn delta_e(a: Rgba, b: Rgba) -> f32 {
    let (a, b) = (oklab(a), oklab(b));
    100.0
        * a.iter()
            .zip(b)
            .map(|(x, y)| (x - y) * (x - y))
            .sum::<f32>()
            .sqrt()
}

/// `a` moved a share `t` of the way to `b`.
pub fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let t = t.clamp(0.0, 1.0);
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.0,
    }
}

/// `colour`, or the least step from it towards `toward` that has `least`
/// contrast against `background`. A colour someone chose is kept whenever
/// it is legible as it is.
pub fn legible(colour: Rgba, background: Rgba, toward: Rgba, least: f32) -> Rgba {
    (0..=20)
        .map(|step| mix(colour, toward, step as f32 / 20.0))
        .find(|c| contrast(*c, background) >= least)
        .unwrap_or(toward)
}

pub fn hsla(colour: Rgba) -> Hsla {
    colour.into()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use berth_core::connection::ConnectionState;

    /// The themes Berth has, and a light one, since a user's terminal theme
    /// may be either.
    pub(crate) fn themes() -> Vec<Theme> {
        let light = Palette {
            foreground: Rgb::new(0x18, 0x18, 0x1b),
            background: Rgb::new(0xfb, 0xfb, 0xfc),
            ..Palette::default()
        };
        vec![
            Theme::of_terminal(&crate::terminal_style::TerminalStyle::default().palette),
            Theme::of_terminal(&light),
        ]
    }

    #[test]
    fn the_chrome_is_the_terminals() {
        let style = crate::terminal_style::TerminalStyle::default();
        let theme = Theme::of_terminal(&style.palette);
        assert_eq!(theme.background, rgba(style.palette.background));
        assert_eq!(theme.text, rgba(style.palette.foreground));
        assert_eq!(appearance(theme.background), Appearance::Dark);
        assert_eq!(appearance(themes()[1].background), Appearance::Light);
    }

    /// Every status colour stands out from the background by at least 3:1
    /// in every theme, and the brand accents are moved only where a theme
    /// needs it: never on the dark theme Berth ships.
    #[test]
    fn status_colours_have_contrast_in_every_theme() {
        let accents = crate::programs::accents();
        assert!(accents.len() >= 6);
        for theme in themes() {
            let bg = theme.background;
            for colour in [theme.gold, theme.rose, theme.rose_text, theme.text_muted] {
                assert!(contrast(colour, bg) >= STATUS_CONTRAST);
            }
            for (name, accent) in &accents {
                let drawn = theme.accent_for(Some(*accent));
                assert!(contrast(drawn, bg) >= STATUS_CONTRAST, "{name}");
            }
            assert_eq!(theme.accent_for(None), theme.text);
        }
        let shipped = &themes()[0];
        for (name, accent) in &accents {
            assert_eq!(shipped.accent_for(Some(*accent)), *accent, "{name}");
        }
    }

    /// Gold and rose never pass for a program's colour: at least ΔE 8 in
    /// OKLab from every shipped accent, as drawn on each theme.
    #[test]
    fn gold_and_rose_stay_apart_from_every_accent() {
        for theme in themes() {
            for (name, accent) in crate::programs::accents() {
                let drawn = theme.accent_for(Some(accent));
                assert!(delta_e(drawn, theme.gold) >= 8.0, "{name} against gold");
                assert!(delta_e(drawn, theme.rose) >= 8.0, "{name} against rose");
            }
        }
    }

    /// The "?" and "!" are legible on their blocks.
    #[test]
    fn the_marks_are_legible_on_gold_and_rose() {
        for theme in themes() {
            assert!(contrast(theme.gold_ink, theme.gold) >= STATUS_CONTRAST);
            assert!(contrast(theme.rose_ink, theme.rose) >= STATUS_CONTRAST);
        }
    }

    /// A colour that is legible already is left alone; one that is not is
    /// moved only as far as it has to be.
    #[test]
    fn an_illegible_accent_is_moved_just_enough() {
        let bg = rgb(0x101012);
        let fg = rgb(0xe4e4e7);
        assert_eq!(legible(rgb(0xd97757), bg, fg, 3.0), rgb(0xd97757));
        let dark = legible(rgb(0x202022), bg, fg, 3.0);
        assert!(contrast(dark, bg) >= 3.0);
        assert!(
            contrast(dark, bg) < 4.0,
            "not all the way to the foreground"
        );
        assert!((contrast(rgb(0x000000), rgb(0xffffff)) - 21.0).abs() < 0.01);
    }

    #[test]
    fn connected_and_degraded_are_distinguishable() {
        for t in themes() {
            let connected = t.state_colour(ConnectionState::Connected);
            let degraded = t.state_colour(ConnectionState::Degraded);
            assert_ne!(
                (connected.r, connected.g, connected.b),
                (degraded.r, degraded.g, degraded.b)
            );
        }
    }
}
