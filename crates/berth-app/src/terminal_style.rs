//! Every visual constant of the terminal, in one place.
//!
//! The element below holds no literals: padding, metrics, colours, cursor and
//! scrollbar all come from here. The visual direction is expected to be revised
//! later against principles from another project, and the point of this seam is
//! that such a revision touches one struct rather than a paint routine. No
//! shared crate is planned; this is deliberately local to `berth-app`.

use gpui::{Font, FontFallbacks, FontFeatures, FontStyle, FontWeight, Hsla, Pixels, px};

use berth_term::terminal::{Palette, Rgb};

/// How the cursor is drawn.
///
/// Only `Block` is selected by default; the others are configuration this
/// struct exists to expose, and berth.toml will choose between them.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorStyle {
    pub shape: CursorShape,
    pub blink: bool,
    /// Seconds for a full on-off cycle. Only read when `blink` is set.
    pub blink_period: f32,
    pub colour: Rgb,
    /// Thickness of the bar and underline shapes, in pixels.
    pub thickness: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollbarStyle {
    pub width: f32,
    pub min_thumb_height: f32,
    pub thumb: Rgb,
    /// Hidden entirely when false; the grid is the content, not a document.
    pub visible: bool,
}

/// How cell metrics are rounded before use.
///
/// Rounding to whole pixels keeps the grid on device pixels and avoids glyphs
/// being resampled differently per column, at the cost of a slightly wrong
/// advance for some fonts. `None` keeps the font's exact advance.
/// Only `None` is the default; the others are configuration this seam exists
/// to expose, selectable once berth.toml carries terminal settings.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellRounding {
    None,
    Round,
    Floor,
}

impl CellRounding {
    pub fn apply(self, value: f32) -> f32 {
        match self {
            CellRounding::None => value,
            CellRounding::Round => value.round(),
            CellRounding::Floor => value.floor(),
        }
        .max(1.0)
    }
}

#[derive(Debug, Clone)]
pub struct TerminalStyle {
    /// Preferred family. Falls back through `font_fallbacks` when absent.
    pub font_family: String,
    pub font_fallbacks: Vec<String>,
    pub font_size: f32,
    /// Multiplied by the font size to give the row height.
    pub line_height: f32,
    /// Horizontal breathing room inside the pane, in pixels.
    pub padding_x: f32,
    pub padding_y: f32,
    /// Used when the font has no `m` glyph to measure.
    pub fallback_advance: f32,
    pub cell_rounding: CellRounding,
    pub background: Rgb,
    pub palette: Palette,
    pub cursor: CursorStyle,
    pub selection: Rgb,
    /// Alpha applied to the selection fill, so text under it stays readable.
    pub selection_alpha: f32,
    /// Not painted yet: the scrollbar arrives with scrollback in M1, and its
    /// constants live here so that work is a change to this struct.
    #[allow(dead_code)]
    pub scrollbar: ScrollbarStyle,
    pub underline_thickness: f32,
    pub strikethrough_thickness: f32,
}

impl TerminalStyle {
    /// Replace the requested family with the first one in the chain that the
    /// platform actually has installed.
    ///
    /// gpui's `FontFallbacks` does not do this. It is glyph-level: it covers a
    /// character the chosen face lacks, not a face that is not there at all. A
    /// family that fails to resolve falls through to the platform's UI font,
    /// which is proportional, and a terminal grid built on a proportional face
    /// has the wrong width for every cell. Nothing errors; the text merely
    /// comes out unevenly spaced, which is how this survived until the
    /// benchmark header started comparing the advance of `m` against `i`.
    pub fn resolve_family(&mut self, installed: &[String]) {
        let has = |name: &str| installed.iter().any(|f| f.eq_ignore_ascii_case(name));
        if has(&self.font_family) {
            return;
        }
        if let Some(found) = self
            .font_fallbacks
            .iter()
            .find(|name| has(name))
            .map(|name| name.to_string())
        {
            self.font_family = found;
        }
        // Nothing in the chain is installed, so the request stands and the
        // platform substitutes. Better than silently picking something worse.
    }
}

impl Default for TerminalStyle {
    fn default() -> Self {
        Self {
            font_family: "JetBrains Mono".to_string(),
            // Walked by `resolve_family`, which has to be called before this
            // style is used. gpui will not walk it: see that method.
            font_fallbacks: vec![
                "Cascadia Mono".to_string(),
                "SF Mono".to_string(),
                "Menlo".to_string(),
                "Consolas".to_string(),
                "DejaVu Sans Mono".to_string(),
                "monospace".to_string(),
            ],
            font_size: 13.0,
            line_height: 1.2,
            padding_x: 8.0,
            padding_y: 0.0,
            fallback_advance: 8.0,
            cell_rounding: CellRounding::None,
            background: Rgb::new(0x10, 0x10, 0x12),
            palette: Palette::default(),
            cursor: CursorStyle {
                shape: CursorShape::Block,
                blink: false,
                blink_period: 1.0,
                colour: Rgb::new(0x4c, 0x8d, 0xff),
                thickness: 2.0,
            },
            selection: Rgb::new(0x4c, 0x8d, 0xff),
            selection_alpha: 0.30,
            scrollbar: ScrollbarStyle {
                width: 6.0,
                min_thumb_height: 24.0,
                thumb: Rgb::new(0x52, 0x52, 0x5b),
                visible: false,
            },
            underline_thickness: 1.0,
            strikethrough_thickness: 1.0,
        }
    }
}

impl TerminalStyle {
    pub fn font(&self) -> Font {
        Font {
            family: self.font_family.clone().into(),
            features: FontFeatures::default(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            fallbacks: Some(FontFallbacks::from_fonts(self.font_fallbacks.clone())),
        }
    }

    pub fn font_size_px(&self) -> Pixels {
        px(self.font_size)
    }

    pub fn line_height_px(&self) -> Pixels {
        px(self.font_size * self.line_height)
    }

    pub fn colour(&self, rgb: Rgb) -> Hsla {
        gpui::Rgba {
            r: rgb.r as f32 / 255.0,
            g: rgb.g as f32 / 255.0,
            b: rgb.b as f32 / 255.0,
            a: 1.0,
        }
        .into()
    }

    pub fn selection_colour(&self) -> Hsla {
        gpui::Rgba {
            r: self.selection.r as f32 / 255.0,
            g: self.selection.g as f32 / 255.0,
            b: self.selection.b as f32 / 255.0,
            a: self.selection_alpha.clamp(0.0, 1.0),
        }
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_defaults_hold() {
        let s = TerminalStyle::default();
        assert_eq!(s.padding_x, 8.0);
        assert_eq!(s.line_height, 1.2);
        assert_eq!(s.font_family, "JetBrains Mono");
        assert_eq!(s.font_size, 13.0);
    }

    /// The point of the fallback list: JetBrains Mono is not installed
    /// everywhere, and the chain must end at something that always resolves.
    #[test]
    fn the_requested_family_is_kept_when_it_is_installed() {
        let mut s = TerminalStyle::default();
        s.resolve_family(&["Menlo".to_string(), "JetBrains Mono".to_string()]);
        assert_eq!(s.font_family, "JetBrains Mono");
    }

    /// The case that was silently wrong: the requested family is absent, and
    /// gpui would have substituted the proportional UI font rather than
    /// walking the chain.
    #[test]
    fn the_first_installed_fallback_is_taken_when_it_is_not() {
        let mut s = TerminalStyle::default();
        s.resolve_family(&["Helvetica".to_string(), "Menlo".to_string()]);
        assert_eq!(s.font_family, "Menlo");
    }

    #[test]
    fn the_chain_is_walked_in_order() {
        let mut s = TerminalStyle::default();
        s.resolve_family(&["Menlo".to_string(), "SF Mono".to_string()]);
        assert_eq!(s.font_family, "SF Mono", "SF Mono comes first in the chain");
    }

    #[test]
    fn an_empty_font_list_leaves_the_request_alone() {
        let mut s = TerminalStyle::default();
        s.resolve_family(&[]);
        assert_eq!(s.font_family, "JetBrains Mono");
    }

    #[test]
    fn family_matching_ignores_case() {
        let mut s = TerminalStyle::default();
        s.resolve_family(&["menlo".to_string()]);
        assert_eq!(s.font_family, "Menlo");
    }

    #[test]
    fn the_fallback_chain_ends_at_generic_monospace() {
        let s = TerminalStyle::default();
        assert_eq!(s.font_fallbacks.last().unwrap(), "monospace");
        assert!(!s.font_fallbacks.contains(&s.font_family));
    }

    #[test]
    fn the_font_carries_its_fallbacks() {
        let font = TerminalStyle::default().font();
        let fallbacks = font.fallbacks.expect("fallbacks must be set");
        assert!(fallbacks.fallback_list().contains(&"monospace".to_string()));
    }

    #[test]
    fn line_height_is_derived_from_the_font_size() {
        let s = TerminalStyle::default();
        assert_eq!(f32::from(s.line_height_px()), 13.0 * 1.2);
    }

    #[test]
    fn cell_rounding_never_returns_a_degenerate_cell() {
        for rounding in [CellRounding::None, CellRounding::Round, CellRounding::Floor] {
            assert!(rounding.apply(0.0) >= 1.0);
            assert!(rounding.apply(0.4) >= 1.0);
        }
    }

    #[test]
    fn cell_rounding_modes_differ_where_it_matters() {
        assert_eq!(CellRounding::None.apply(7.6), 7.6);
        assert_eq!(CellRounding::Round.apply(7.6), 8.0);
        assert_eq!(CellRounding::Floor.apply(7.6), 7.0);
    }

    #[test]
    fn the_selection_is_translucent_so_text_stays_readable() {
        let s = TerminalStyle::default();
        assert!(s.selection_alpha > 0.0 && s.selection_alpha < 1.0);
        let colour = s.selection_colour();
        assert!(colour.a > 0.0 && colour.a < 1.0);
    }

    #[test]
    fn opaque_colours_are_fully_opaque() {
        let s = TerminalStyle::default();
        assert_eq!(s.colour(s.background).a, 1.0);
    }

    /// Berth's direction is quiet and dense; a scrollbar is off by default.
    #[test]
    fn the_scrollbar_is_hidden_by_default() {
        assert!(!TerminalStyle::default().scrollbar.visible);
    }
}
