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
    pub appearance: Appearance,
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
                appearance,
                background: rgb(0x101012),
                surface: rgb(0x161619),
                border: rgb(0x26262b),
                text: rgb(0xe4e4e7),
                text_muted: rgb(0x71717a),
                accent: rgb(0x4c8dff),
                warning: rgb(0xd4553d),
            },
            Appearance::Light => Self {
                appearance,
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

    /// A stable colour for a host, so a tab is told apart by more than a name.
    ///
    /// Derived from the alias rather than configured. A colour nobody has to
    /// set is a colour that is always there, and the only two properties that
    /// matter are that one host never changes colour and that neighbours
    /// rarely collide.
    ///
    /// A prod host is always the warning colour, whatever its name hashes to.
    /// "This is production" outranks "this is host number seven", and a user
    /// who has learned one colour for prod should never see it move.
    pub fn host_colour(&self, alias: &str, is_prod: bool) -> Hsla {
        if is_prod {
            return self.warning.into();
        }
        let (saturation, lightness) = match self.appearance {
            Appearance::Dark => (0.52, 0.62),
            Appearance::Light => (0.55, 0.42),
        };
        gpui::hsla(hue(alias), saturation, lightness, 1.0)
    }
}

/// FNV-1a, for a hue that is stable across runs and machines.
///
/// `DefaultHasher` would do the same job but is explicitly not guaranteed
/// stable between Rust releases, and a host silently changing colour after a
/// toolchain bump is exactly the property this must not have.
fn hue(alias: &str) -> f32 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in alias.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    (hash % 360) as f32 / 360.0
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
    fn a_host_keeps_its_colour() {
        let t = Theme::for_appearance(Appearance::Dark);
        assert_eq!(
            t.host_colour("berth-test", false),
            t.host_colour("berth-test", false)
        );
    }

    #[test]
    fn different_hosts_rarely_collide() {
        let t = Theme::for_appearance(Appearance::Dark);
        let aliases = [
            "berth-test",
            "berth-local",
            "prod-eu",
            "staging",
            "db-1",
            "db-2",
            "web-1",
            "web-2",
            "jump",
            "backup",
        ];
        let mut hues: Vec<u32> = aliases
            .iter()
            .map(|a| (t.host_colour(a, false).h * 360.0) as u32)
            .collect();
        hues.sort_unstable();
        hues.dedup();
        assert_eq!(hues.len(), aliases.len(), "two of these share a hue");
    }

    /// Learning one colour for prod and then having it move would be worse
    /// than never having had it.
    #[test]
    fn prod_is_always_the_warning_colour() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let t = Theme::for_appearance(appearance);
            let warning: gpui::Hsla = t.warning.into();
            assert_eq!(t.host_colour("anything", true), warning);
            assert_eq!(t.host_colour("something-else", true), warning);
        }
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
