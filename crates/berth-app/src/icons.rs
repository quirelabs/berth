//! Icons, compiled into the binary.
//!
//! gpui reads SVGs through an `AssetSource`. Berth's is a fixed table rather
//! than a directory walk or an embedding crate, because there are a handful of
//! icons and a missing one should be a compile error rather than a shape that
//! silently fails to draw at runtime. `include_bytes!` gives exactly that.
//!
//! Icons are Phosphor, bold weight, MIT. See `assets/icons/LICENSE`.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

/// An agent is running in this session.
///
/// A ghost rather than a robot: the phrase for a process acting on its own in
/// someone else's session is a ghost in the machine, and every other tool in
/// this space has settled on the same bot outline or the same sparkles. It also
/// survives being 12 px in a tab, which a face full of detail does not.
pub const GHOST: &str = "icons/ghost.svg";

const TABLE: &[(&str, &[u8])] = &[(GHOST, include_bytes!("../assets/icons/ghost.svg"))];

pub struct Icons;

impl AssetSource for Icons {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(TABLE
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(TABLE
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_icon_loads() {
        for (name, _) in TABLE {
            let loaded = Icons.load(name).unwrap();
            assert!(loaded.is_some(), "{name} is named but does not load");
        }
    }

    #[test]
    fn a_missing_icon_is_absent_rather_than_an_error() {
        assert!(Icons.load("icons/not-a-real-icon.svg").unwrap().is_none());
    }

    /// gpui parses these at paint time, where a failure is an icon that quietly
    /// does not appear. Catching a truncated or mangled file here is cheaper.
    #[test]
    fn every_icon_is_a_single_path_svg_that_inherits_colour() {
        for (name, bytes) in TABLE {
            let text = std::str::from_utf8(bytes).expect("{name} is not utf-8");
            assert!(text.starts_with("<svg"), "{name}");
            assert!(text.trim_end().ends_with("</svg>"), "{name} is truncated");
            assert!(
                text.contains("viewBox=\"0 0 256 256\""),
                "{name} is not a Phosphor 256 grid"
            );
            assert!(
                text.contains("fill=\"currentColor\""),
                "{name} would ignore the theme colour"
            );
        }
    }

    #[test]
    fn listing_finds_the_icons() {
        assert!(Icons.list("icons/").unwrap().contains(&GHOST.into()));
    }
}
