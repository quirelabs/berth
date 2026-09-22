//! What Berth remembers between launches.
//!
//! Deliberately not `berth.toml`. That is a configuration surface with a
//! documented schema, and nobody has asked for one; this is the handful of
//! things the user changed by using the app, written back so they are still
//! true next time. The font size, and names given to hosts.
//!
//! Nothing here is fatal. A missing file is a first launch, a corrupt one is
//! somebody's text editor, and a read-only config directory is a locked-down
//! machine. All three mean the defaults, and none of them is worth refusing to
//! start over.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Smaller than this and the text is not legible; larger and a useful number
/// of columns stops fitting on any screen. The bounds exist mostly to keep a
/// hand-edited file from producing a window one cell wide.
pub const MIN_FONT_SIZE: f32 = 6.0;
pub const MAX_FONT_SIZE: f32 = 40.0;
pub const DEFAULT_FONT_SIZE: f32 = 13.0;

/// How much one press changes the size. A whole point, because half a point is
/// a keypress that appears to do nothing.
pub const FONT_SIZE_STEP: f32 = 1.0;

/// Longest name a host can be given. A sidebar row is 220 px wide; this is
/// generous for that and keeps a pasted paragraph out of the file.
pub const MAX_HOST_NAME: usize = 64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// `default` so a file written by an older Berth, or one with a line deleted,
// still loads. Every field added later has to be optional in this sense or the
// first upgrade wipes the file.
#[serde(default)]
pub struct Settings {
    pub font_size: f32,
    /// Names shown instead of a host's alias, by alias. Berth never writes to
    /// ssh config, so a host is renamed here and only here.
    pub host_names: std::collections::BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size: DEFAULT_FONT_SIZE,
            host_names: std::collections::BTreeMap::new(),
        }
    }
}

impl Settings {
    /// Read the stored settings, falling back to the defaults for anything
    /// missing, unreadable or out of range.
    pub fn load() -> Self {
        match path() {
            Some(path) => Self::load_from(&path),
            None => Self::default(),
        }
    }

    /// Write the settings back. Best effort: a failure here must never take
    /// the window down, and the user has already seen the change take effect.
    pub fn save(&self) {
        if let Some(path) = path() {
            self.save_to(&path);
        }
    }

    /// The path is a parameter so the tests never touch the user's own config
    /// directory, matching how `known_hosts` is tested.
    pub fn load_from(path: &Path) -> Self {
        let mut settings = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str::<Settings>(&text).ok())
            .unwrap_or_default();
        // Clamped on the way in as well as on the way out, because the file is
        // plain text and the obvious thing to do with it is edit it.
        settings.font_size = clamp_font_size(settings.font_size);
        let names = std::mem::take(&mut settings.host_names);
        for (alias, name) in names {
            settings.set_host_name(&alias, &name);
        }
        settings
    }

    pub fn save_to(&self, path: &Path) {
        let Ok(text) = toml::to_string_pretty(self) else {
            return;
        };
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            tracing::debug!(%error, "could not create the settings directory");
            return;
        }
        if let Err(error) = std::fs::write(path, text) {
            tracing::debug!(%error, "could not write settings, this one is not remembered");
        }
    }

    /// Change the font size, clamped. Returns whether it actually moved, so a
    /// caller at the limit does not repaint and rewrite the file for nothing.
    pub fn set_font_size(&mut self, size: f32) -> bool {
        let size = clamp_font_size(size);
        if (size - self.font_size).abs() < f32::EPSILON {
            return false;
        }
        self.font_size = size;
        true
    }
}

impl Settings {
    /// The name a host was given, if any.
    pub fn host_name(&self, alias: &str) -> Option<&str> {
        self.host_names.get(alias).map(String::as_str)
    }

    /// Give a host a name, or take it away with an empty one. Trimmed, one
    /// line, and cut at [`MAX_HOST_NAME`] characters. Returns whether anything
    /// changed, so an unchanged name is not a file write.
    pub fn set_host_name(&mut self, alias: &str, name: &str) -> bool {
        let one_line: String = name
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let cleaned: String = one_line.trim().chars().take(MAX_HOST_NAME).collect();
        let cleaned = cleaned.trim_end().to_string();
        // A name that is the alias itself is no name at all.
        if cleaned.is_empty() || cleaned == alias {
            return self.host_names.remove(alias).is_some();
        }
        if self.host_names.get(alias) == Some(&cleaned) {
            return false;
        }
        self.host_names.insert(alias.to_string(), cleaned);
        true
    }
}

/// A size that is a number and is usable, or the default.
fn clamp_font_size(size: f32) -> f32 {
    if !size.is_finite() {
        return DEFAULT_FONT_SIZE;
    }
    size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
}

/// Where the settings live.
///
/// `BERTH_SETTINGS` first, so a test never writes into the user's own config
/// directory. Otherwise the platform's config directory, which is
/// `~/Library/Application Support` on macOS and `%APPDATA%` on Windows.
pub fn path() -> Option<PathBuf> {
    if let Some(override_path) = crate::env::settings() {
        return Some(override_path);
    }
    Some(dirs::config_dir()?.join("berth").join("settings.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_outside_the_bounds_is_pulled_back() {
        assert_eq!(clamp_font_size(1.0), MIN_FONT_SIZE);
        assert_eq!(clamp_font_size(900.0), MAX_FONT_SIZE);
        assert_eq!(clamp_font_size(14.0), 14.0);
    }

    /// A hand-edited file is the likely source of these, and a grid sized from
    /// a NaN is a window with no cells in it.
    #[test]
    fn a_size_that_is_not_a_number_falls_back() {
        assert_eq!(clamp_font_size(f32::NAN), DEFAULT_FONT_SIZE);
        assert_eq!(clamp_font_size(f32::INFINITY), DEFAULT_FONT_SIZE);
    }

    #[test]
    fn setting_the_same_size_is_not_a_change() {
        let mut settings = Settings::default();
        assert!(!settings.set_font_size(DEFAULT_FONT_SIZE));
        assert!(settings.set_font_size(DEFAULT_FONT_SIZE + 1.0));
    }

    /// At the limit every further press is a no-op, rather than a repaint and
    /// a file write that change nothing.
    #[test]
    fn pressing_past_the_limit_changes_nothing() {
        let mut settings = Settings::default();
        assert!(settings.set_font_size(MAX_FONT_SIZE));
        assert!(!settings.set_font_size(MAX_FONT_SIZE + 10.0));
    }

    #[test]
    fn settings_survive_a_round_trip_through_a_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        // Through a directory that does not exist yet, which is every first
        // launch.
        let path = dir.path().join("berth").join("settings.toml");

        let mut settings = Settings::default();
        settings.set_font_size(17.0);
        settings.save_to(&path);

        assert_eq!(Settings::load_from(&path), settings);
    }

    #[test]
    fn a_file_that_is_not_there_is_the_defaults() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert_eq!(
            Settings::load_from(&dir.path().join("nothing.toml")),
            Settings::default()
        );
    }

    /// Someone opened it in a text editor and broke it. That is not a reason
    /// to refuse to start.
    #[test]
    fn a_corrupt_file_is_the_defaults() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "this is not toml {{{").expect("write");
        assert_eq!(Settings::load_from(&path), Settings::default());
    }

    /// The hand-edited case the clamp exists for, through the real file.
    #[test]
    fn an_absurd_stored_size_is_pulled_back_on_load() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "font_size = 900.0").expect("write");
        assert_eq!(Settings::load_from(&path).font_size, MAX_FONT_SIZE);
    }

    /// The upgrade case: a file that predates a field still loads.
    #[test]
    fn an_empty_file_is_the_defaults() {
        assert_eq!(
            toml::from_str::<Settings>("").expect("an empty file is valid toml"),
            Settings::default()
        );
    }

    #[test]
    fn a_host_can_be_named_and_unnamed() {
        let mut settings = Settings::default();
        assert!(settings.set_host_name("web-1", "  Production web  "));
        assert_eq!(settings.host_name("web-1"), Some("Production web"));
        assert!(!settings.set_host_name("web-1", "Production web"));
        assert!(settings.set_host_name("web-1", "   "));
        assert_eq!(settings.host_name("web-1"), None);
        assert!(!settings.set_host_name("web-1", ""));
    }

    /// Naming a host after its own alias is the same as not naming it.
    #[test]
    fn the_alias_itself_is_no_name() {
        let mut settings = Settings::default();
        settings.set_host_name("web-1", "Web");
        assert!(settings.set_host_name("web-1", "web-1"));
        assert_eq!(settings.host_name("web-1"), None);
    }

    #[test]
    fn a_name_is_one_short_line() {
        let mut settings = Settings::default();
        settings.set_host_name("a", "two\nlines");
        assert_eq!(settings.host_name("a"), Some("two lines"));
        settings.set_host_name("b", &"x".repeat(500));
        assert_eq!(settings.host_name("b").map(str::len), Some(MAX_HOST_NAME));
    }

    /// A hand-edited file gets the same cleaning as a rename.
    #[test]
    fn names_are_cleaned_on_load() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.toml");
        std::fs::write(
            &path,
            "font_size = 13.0\n[host_names]\nweb = \"  Web  \"\nblank = \"  \"\n",
        )
        .expect("write");
        let settings = Settings::load_from(&path);
        assert_eq!(settings.host_name("web"), Some("Web"));
        assert_eq!(settings.host_name("blank"), None);
    }

    #[test]
    fn names_survive_a_round_trip_through_a_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.toml");
        let mut settings = Settings::default();
        settings.set_host_name("web-1", "Production web");
        settings.save_to(&path);
        assert_eq!(Settings::load_from(&path), settings);
    }
}
