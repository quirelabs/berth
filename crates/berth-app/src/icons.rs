//! Icons, compiled into the binary.
//!
//! gpui reads SVGs through an `AssetSource`. Berth's is a fixed table rather
//! than a directory walk or an embedding crate, because there are a handful of
//! icons and a missing one should be a compile error rather than a shape that
//! silently fails to draw at runtime. `include_bytes!` gives exactly that.
//!
//! The interface's own icons are Phosphor (MIT, `assets/icons/LICENSE`).
//! Program icons, which say what runs in a tab, are Phosphor for the generic
//! ones, Lobe Icons for the agents and Simple Icons for developer tools, all
//! in one colour and tinted with the theme's text colour. Sources, commits,
//! licences and the trademark note are in `assets/icons/THIRD-PARTY.md`.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

/// An agent is running in this session.
///
/// A ghost rather than a robot: the phrase for a process acting on its own in
/// someone else's session is a ghost in the machine, and every other tool in
/// this space has settled on the same bot outline or the same sparkles. It also
/// survives being 12 px in a tab, which a face full of detail does not.
pub const GHOST: &str = "icons/ghost.svg";

/// Program icons, by the name [`crate::programs`] gives them.
pub const TERMINAL: &str = "icons/programs/terminal.svg";
pub const SSH: &str = "icons/programs/ssh.svg";
pub const CLAUDE: &str = "icons/programs/claude.svg";
pub const CODEX: &str = "icons/programs/codex.svg";
pub const NODE: &str = "icons/programs/node.svg";
pub const BUN: &str = "icons/programs/bun.svg";
pub const DENO: &str = "icons/programs/deno.svg";
pub const PYTHON: &str = "icons/programs/python.svg";
pub const GIT: &str = "icons/programs/git.svg";
pub const DOCKER: &str = "icons/programs/docker.svg";
pub const VIM: &str = "icons/programs/vim.svg";
pub const NEOVIM: &str = "icons/programs/neovim.svg";

macro_rules! program {
    ($name:expr, $file:literal) => {
        (
            $name,
            include_bytes!(concat!("../assets/icons/programs/", $file)),
        )
    };
}

const TABLE: &[(&str, &[u8])] = &[
    (GHOST, include_bytes!("../assets/icons/ghost.svg")),
    program!(TERMINAL, "terminal.svg"),
    program!(SSH, "ssh.svg"),
    program!(CLAUDE, "claude.svg"),
    program!(CODEX, "codex.svg"),
    program!(NODE, "node.svg"),
    program!(BUN, "bun.svg"),
    program!(DENO, "deno.svg"),
    program!(PYTHON, "python.svg"),
    program!(GIT, "git.svg"),
    program!(DOCKER, "docker.svg"),
    program!(VIM, "vim.svg"),
    program!(NEOVIM, "neovim.svg"),
];

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
    /// Each is one colour: nothing in it may name a colour of its own, or it
    /// would not take the theme's.
    #[test]
    fn every_icon_is_a_whole_svg_in_one_colour() {
        for (name, bytes) in TABLE {
            let text = std::str::from_utf8(bytes).expect("{name} is not utf-8");
            assert!(text.starts_with("<svg"), "{name}");
            assert!(text.trim_end().ends_with("</svg>"), "{name} is truncated");
            assert!(
                text.contains("viewBox=\"0 0 256 256\"") || text.contains("viewBox=\"0 0 24 24\""),
                "{name} is on an unexpected grid"
            );
            for colour in ["fill=\"#", "stroke=\"#", "fill:#", "url(#"] {
                assert!(!text.contains(colour), "{name} names a colour: {colour}");
            }
        }
    }

    #[test]
    fn listing_finds_the_icons() {
        assert!(Icons.list("icons/").unwrap().contains(&GHOST.into()));
    }
}
