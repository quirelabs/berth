//! Local sessions, saved so they come back after a quit or a reboot (D46).
//!
//! A local shell ends with Berth (D36). What is kept is enough to put it back
//! where it was: which tabs held local shells, how they were split, each
//! shell's directory and its last screen. The screen is only ever redrawn,
//! never read for meaning, and there is no scrollback in it.
//!
//! Like `settings`, nothing here is fatal. A missing file is nothing to
//! restore, a corrupt one is nothing to restore, and a tab that does not add up
//! is dropped on its own rather than taking the rest with it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Bumped when the shape changes incompatibly. A file from another version is
/// ignored rather than half understood.
pub const VERSION: u32 = 1;

/// Bounds for what a hand-edited or damaged file can ask for. A screen is a few
/// kilobytes; these exist so a bad file cannot make a window of a million
/// cells or hold a gigabyte in memory.
const MAX_TABS: usize = 64;
const MAX_PANES: usize = 32;
const MAX_SIDE: u16 = 1000;
const MAX_SCREEN_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    pub version: u32,
    pub tabs: Vec<SavedTab>,
}

impl Default for Saved {
    fn default() -> Self {
        Self {
            version: VERSION,
            tabs: Vec::new(),
        }
    }
}

/// One tab of local shells.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedTab {
    /// Where it sat in the tab strip.
    pub position: usize,
    /// Which pane had the keyboard, as an index into `panes`.
    pub focus: usize,
    pub layout: Layout,
    pub panes: Vec<SavedPane>,
}

/// The split tree, with leaves as indexes into the tab's panes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Layout {
    Pane(usize),
    Split {
        /// Stacked rather than side by side.
        down: bool,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

impl Layout {
    fn leaves(&self, out: &mut Vec<usize>) {
        match self {
            Layout::Pane(index) => out.push(*index),
            Layout::Split { first, second, .. } => {
                first.leaves(out);
                second.leaves(out);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedPane {
    /// `None` where the platform would not say (Windows), and the shell then
    /// starts at home.
    pub cwd: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
    /// Escape sequences that redraw the last screen.
    pub screen: String,
}

impl SavedTab {
    /// Whether the layout and the panes agree: every pane placed exactly once,
    /// and focus pointing at one of them.
    fn is_sound(&self) -> bool {
        if self.panes.is_empty() || self.panes.len() > MAX_PANES {
            return false;
        }
        let mut leaves = Vec::new();
        self.layout.leaves(&mut leaves);
        leaves.sort_unstable();
        let expected: Vec<usize> = (0..self.panes.len()).collect();
        leaves == expected
            && self.focus < self.panes.len()
            && self.panes.iter().all(|p| {
                (1..=MAX_SIDE).contains(&p.cols)
                    && (1..=MAX_SIDE).contains(&p.rows)
                    && p.screen.len() <= MAX_SCREEN_BYTES
            })
    }
}

impl Saved {
    /// What the last run left, or nothing.
    pub fn load() -> Self {
        match path() {
            Some(path) => Self::load_from(&path),
            None => Self::default(),
        }
    }

    /// Best effort. A failure costs the next launch its restored tabs and
    /// nothing else, so it is logged and never raised.
    pub fn save(&self) {
        if let Some(path) = path() {
            self.save_to(&path);
        }
    }

    /// The path is a parameter so the tests never touch the user's own data
    /// directory.
    pub fn load_from(path: &Path) -> Self {
        let Some(mut saved) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str::<Saved>(&text).ok())
        else {
            return Self::default();
        };
        if saved.version != VERSION {
            return Self::default();
        }
        saved.tabs.retain(SavedTab::is_sound);
        saved.tabs.truncate(MAX_TABS);
        saved
    }

    /// Written beside the target and renamed over it, so a quit that lands
    /// mid-write leaves the last good file rather than half of a new one.
    pub fn save_to(&self, path: &Path) {
        let Ok(text) = toml::to_string(self) else {
            tracing::debug!("could not serialise local sessions");
            return;
        };
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            tracing::debug!(%error, "could not create the directory for local sessions");
            return;
        }
        let partial = path.with_extension("toml.partial");
        let written = std::fs::write(&partial, text).and_then(|()| std::fs::rename(&partial, path));
        if let Err(error) = written {
            tracing::debug!(%error, "could not save local sessions");
            let _ = std::fs::remove_file(&partial);
        }
    }
}

/// Where local sessions are kept.
///
/// `BERTH_RESTORE` first, so a test never writes into the user's own data
/// directory. Otherwise the platform's data directory: state Berth wrote, not
/// something anyone configures.
pub fn path() -> Option<PathBuf> {
    if let Some(override_path) = crate::env::restore() {
        return Some(override_path);
    }
    Some(dirs::data_dir()?.join("berth").join("local-sessions.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(cwd: &str) -> SavedPane {
        SavedPane {
            cwd: Some(PathBuf::from(cwd)),
            cols: 80,
            rows: 24,
            screen: "\x1b[0m\x1b[H\x1b[2J\x1b[1;1H$ ls\r\n\"quoted\" \\ back".into(),
        }
    }

    fn split_tab() -> SavedTab {
        SavedTab {
            position: 2,
            focus: 1,
            layout: Layout::Split {
                down: false,
                first: Box::new(Layout::Pane(0)),
                second: Box::new(Layout::Split {
                    down: true,
                    first: Box::new(Layout::Pane(1)),
                    second: Box::new(Layout::Pane(2)),
                }),
            },
            panes: vec![pane("/a"), pane("/b"), pane("/c")],
        }
    }

    #[test]
    fn a_nested_layout_survives_the_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("local-sessions.toml");
        let saved = Saved {
            version: VERSION,
            tabs: vec![split_tab(), {
                let mut single = split_tab();
                single.position = 0;
                single.focus = 0;
                single.layout = Layout::Pane(0);
                single.panes.truncate(1);
                single.panes[0].cwd = None;
                single
            }],
        };
        saved.save_to(&path);
        assert_eq!(Saved::load_from(&path), saved);
    }

    #[test]
    fn nothing_saved_is_nothing_to_restore() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert_eq!(
            Saved::load_from(&dir.path().join("missing.toml")),
            Saved::default()
        );
    }

    #[test]
    fn a_corrupt_file_is_nothing_to_restore() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("local-sessions.toml");
        std::fs::write(&path, "not toml {{{").expect("written");
        assert_eq!(Saved::load_from(&path), Saved::default());
    }

    /// A file from a later Berth is not guessed at.
    #[test]
    fn another_version_is_ignored() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("local-sessions.toml");
        Saved {
            version: VERSION + 1,
            tabs: vec![split_tab()],
        }
        .save_to(&path);
        assert!(Saved::load_from(&path).tabs.is_empty());
    }

    /// One bad tab does not cost the others.
    #[test]
    fn a_tab_that_does_not_add_up_is_dropped_alone() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("local-sessions.toml");

        let mut missing_pane = split_tab();
        missing_pane.panes.pop();
        let mut twice = split_tab();
        twice.layout = Layout::Split {
            down: false,
            first: Box::new(Layout::Pane(0)),
            second: Box::new(Layout::Pane(0)),
        };
        let mut lost_focus = split_tab();
        lost_focus.focus = 9;
        let mut huge = split_tab();
        huge.panes[0].cols = 60_000;
        let mut empty = split_tab();
        empty.panes.clear();
        empty.layout = Layout::Pane(0);

        Saved {
            version: VERSION,
            tabs: vec![missing_pane, twice, split_tab(), lost_focus, huge, empty],
        }
        .save_to(&path);
        assert_eq!(Saved::load_from(&path).tabs, vec![split_tab()]);
    }

    /// Saving leaves no partial file behind, and replaces the old one whole.
    #[test]
    fn saving_replaces_the_file_whole() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("local-sessions.toml");
        Saved {
            version: VERSION,
            tabs: vec![split_tab()],
        }
        .save_to(&path);
        Saved::default().save_to(&path);
        assert!(Saved::load_from(&path).tabs.is_empty());
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("listed")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["local-sessions.toml"]);
    }
}
