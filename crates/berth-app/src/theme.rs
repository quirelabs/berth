//! Quiet, dense, monochrome with one accent. No gradients, no cards, and no
//! corner radius above 4 px.

use gpui::{Hsla, Rgba, WindowAppearance, rgb};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Light,
    Dark,
}

impl From<WindowAppearance> for Appearance {
    fn from(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Appearance::Dark,
            WindowAppearance::Light | WindowAppearance::VibrantLight => Appearance::Light,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub background: Rgba,
    pub surface: Rgba,
    pub border: Rgba,
    pub text: Rgba,
    pub text_muted: Rgba,
    pub accent: Rgba,
    /// Applied as a top border on hosts tagged `prod`.
    pub warning: Rgba,
}

impl Theme {
    pub fn for_appearance(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Dark => Self {
                background: rgb(0x101012),
                surface: rgb(0x161619),
                border: rgb(0x26262b),
                text: rgb(0xe4e4e7),
                text_muted: rgb(0x71717a),
                accent: rgb(0x4c8dff),
                warning: rgb(0xd4553d),
            },
            Appearance::Light => Self {
                background: rgb(0xfbfbfc),
                surface: rgb(0xf2f2f4),
                border: rgb(0xdededf),
                text: rgb(0x18181b),
                text_muted: rgb(0x8a8a92),
                accent: rgb(0x2563d4),
                warning: rgb(0xb03824),
            },
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

pub fn hsla(colour: Rgba) -> Hsla {
    colour.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use berth_core::connection::ConnectionState;

    #[test]
    fn both_appearances_are_defined() {
        let dark = Theme::for_appearance(Appearance::Dark);
        let light = Theme::for_appearance(Appearance::Light);
        assert_ne!(dark.background.r, light.background.r);
    }

    #[test]
    fn connected_and_degraded_are_distinguishable() {
        let t = Theme::for_appearance(Appearance::Dark);
        let connected = t.state_colour(ConnectionState::Connected);
        let degraded = t.state_colour(ConnectionState::Degraded);
        assert_ne!(
            (connected.r, connected.g, connected.b),
            (degraded.r, degraded.g, degraded.b)
        );
    }
}
