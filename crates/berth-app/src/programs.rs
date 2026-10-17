//! What runs in a session, by name: one table from a foreground process name
//! to its name, its sidebar tile and the accent its status is drawn in
//! (D53).
//!
//! The name is the process's own, as the machine reports it: `claude`,
//! `python3.12`, `-zsh`, `/usr/bin/nvim`. Anything not in the table, and any
//! shell, gets the generic terminal icon and no accent of its own: its
//! status is drawn in the theme's muted foreground. `berth.toml` can give
//! any program an accent, over the table's.
//!
//! The icons of other projects are their marks. They say which program runs
//! in a tab and nothing more, and are for dogfooding until each is cleared
//! for release (`assets/icons/THIRD-PARTY.md`).

use std::collections::BTreeMap;

use crate::icons;

/// A program Berth knows by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Program {
    /// How it is named in a title.
    pub name: &'static str,
    pub icon: &'static str,
    /// The colour its status is drawn in, as `0xrrggbb`.
    pub accent: Option<u32>,
    /// Its tile in the sidebar.
    pub tile: Tile,
}

/// A program's tile in the sidebar (D53): an agent's mark in white on its
/// brand's colour, or dark on a light tile for one without a brand colour;
/// anything else a Phosphor glyph on the raised surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tile {
    Brand(u32),
    Light,
    Neutral,
}

/// What an unknown program and a plain shell look like.
pub const GENERIC: Program = Program {
    name: "",
    icon: icons::TERMINAL,
    accent: None,
    tile: Tile::Neutral,
};

const fn tool(name: &'static str, icon: &'static str) -> Program {
    Program {
        name,
        icon,
        accent: None,
        tile: Tile::Neutral,
    }
}

const fn agent(name: &'static str, icon: &'static str, accent: u32) -> Program {
    Program {
        name,
        icon,
        accent: Some(accent),
        tile: Tile::Brand(accent),
    }
}

/// An agent with no brand colour: a light tile, and the foreground for its
/// status unless it has an accent of its own.
const fn plain_agent(name: &'static str, icon: &'static str, accent: Option<u32>) -> Program {
    Program {
        name,
        icon,
        accent,
        tile: Tile::Light,
    }
}

/// Process names, lower case, to programs. A name with a version on its end
/// (`python3.12`, `node22`) is looked up without it.
const TABLE: &[(&str, Program)] = &[
    // The agents, each in its brand's own colour (D53). Clashes between
    // them are accepted; gold and rose are kept apart from all of them.
    // Cursor has no brand colour and is drawn in the foreground.
    ("claude", agent("claude", icons::CLAUDE, 0xd97757)),
    ("codex", plain_agent("codex", icons::CODEX, Some(0x2bc49b))),
    ("copilot", agent("copilot", icons::COPILOT, 0x8534f3)),
    ("mistral", agent("mistral", icons::MISTRAL, 0xfa520f)),
    ("qwen", agent("qwen", icons::QWEN, 0x615ced)),
    ("gemini", agent("gemini", icons::GEMINI, 0x4285f4)),
    ("cursor-agent", plain_agent("cursor", icons::CURSOR, None)),
    ("node", tool("node", icons::NODE)),
    ("nodejs", tool("node", icons::NODE)),
    ("npm", tool("npm", icons::NODE)),
    ("npx", tool("npx", icons::NODE)),
    ("bun", tool("bun", icons::BUN)),
    ("bunx", tool("bunx", icons::BUN)),
    ("deno", tool("deno", icons::DENO)),
    ("python", tool("python", icons::PYTHON)),
    ("ipython", tool("ipython", icons::PYTHON)),
    ("git", tool("git", icons::GIT)),
    ("lazygit", tool("lazygit", icons::GIT)),
    ("tig", tool("tig", icons::GIT)),
    ("docker", tool("docker", icons::DOCKER)),
    ("docker-compose", tool("docker-compose", icons::DOCKER)),
    ("vim", tool("vim", icons::VIM)),
    ("vi", tool("vi", icons::VIM)),
    ("nvim", tool("nvim", icons::NEOVIM)),
    ("ssh", tool("ssh", icons::SSH)),
    ("mosh", tool("mosh", icons::SSH)),
    ("mosh-client", tool("mosh", icons::SSH)),
    // The shells: named, so a title can say which, with the generic icon.
    ("sh", tool("sh", icons::TERMINAL)),
    ("bash", tool("bash", icons::TERMINAL)),
    ("zsh", tool("zsh", icons::TERMINAL)),
    ("fish", tool("fish", icons::TERMINAL)),
    ("dash", tool("dash", icons::TERMINAL)),
    ("ksh", tool("ksh", icons::TERMINAL)),
    ("nu", tool("nu", icons::TERMINAL)),
    ("pwsh", tool("pwsh", icons::TERMINAL)),
    ("powershell", tool("powershell", icons::TERMINAL)),
    ("cmd", tool("cmd", icons::TERMINAL)),
];

/// The bare name of a process: no directory, no login shell's leading dash,
/// no `.exe`, lower case.
fn bare(command: &str) -> String {
    let name = command
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim_start_matches('-')
        .to_ascii_lowercase();
    name.strip_suffix(".exe").unwrap_or(&name).to_string()
}

/// The program a process name stands for. Unknown names, and no name at
/// all, are [`GENERIC`].
pub fn lookup(command: &str) -> Program {
    let name = bare(command);
    let find = |name: &str| {
        TABLE
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, program)| *program)
    };
    find(&name)
        .or_else(|| {
            // `python3.12`, `node22`: the name without the version on its end.
            let stem = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
            (stem != name && !stem.is_empty())
                .then(|| find(stem))
                .flatten()
        })
        .unwrap_or(GENERIC)
}

/// The accent a program's status is drawn in: what `berth.toml` gives it
/// (`overrides`, by name), else the table's, else none.
pub fn accent(command: &str, overrides: &BTreeMap<String, [u8; 3]>) -> Option<gpui::Rgba> {
    let name = display_name(command);
    overrides
        .get(&name)
        .map(|[r, g, b]| (u32::from(*r) << 16) | (u32::from(*g) << 8) | u32::from(*b))
        .or(lookup(command).accent)
        .map(gpui::rgb)
}

/// Every accent the table has, by name.
#[cfg(test)]
pub fn accents() -> Vec<(&'static str, gpui::Rgba)> {
    TABLE
        .iter()
        .filter_map(|(name, program)| Some((*name, gpui::rgb(program.accent?))))
        .collect()
}

/// Which name a session is known by (D53): the `app` its program reports
/// (OSC 7501) over the process that has the terminal, since many agents are
/// `node` or `python` to the machine. Untrusted either way, and used only to
/// pick a name, a tile and an accent.
pub fn shown<'a>(app: Option<&'a str>, process: Option<&'a str>) -> Option<&'a str> {
    app.map(str::trim).filter(|app| !app.is_empty()).or(process)
}

/// What to call a process in a title: the registry's name for it, else its
/// own bare name, else nothing.
pub fn display_name(command: &str) -> String {
    let program = lookup(command);
    if program.name.is_empty() {
        bare(command)
    } else {
        program.name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_names_have_their_icon_and_accent() {
        assert_eq!(lookup("claude").icon, icons::CLAUDE);
        assert_eq!(lookup("claude").accent, Some(0xd97757));
        assert_eq!(lookup("codex").accent, Some(0x2bc49b));
        assert_eq!(lookup("gemini").accent, Some(0x4285f4));
        assert_eq!(lookup("copilot").accent, Some(0x8534f3));
        assert_eq!(lookup("cursor-agent").accent, None);
        assert_eq!(lookup("claude").tile, Tile::Brand(0xd97757));
        assert_eq!(lookup("codex").tile, Tile::Light);
        assert_eq!(lookup("cursor-agent").tile, Tile::Light);
        assert_eq!(lookup("zsh").tile, Tile::Neutral);
        assert_eq!(lookup("node").icon, icons::NODE);
        assert_eq!(lookup("bun").icon, icons::BUN);
        assert_eq!(lookup("deno").icon, icons::DENO);
        assert_eq!(lookup("git").icon, icons::GIT);
        assert_eq!(lookup("lazygit").icon, icons::GIT);
        assert_eq!(lookup("docker").icon, icons::DOCKER);
        assert_eq!(lookup("vim").icon, icons::VIM);
        assert_eq!(lookup("nvim").icon, icons::NEOVIM);
        assert_eq!(lookup("ssh").icon, icons::SSH);
        assert_eq!(lookup("node").accent, None);
    }

    /// What a program reports itself as wins over the process that runs
    /// it: an agent running under node is the agent.
    #[test]
    fn the_reported_app_comes_before_the_process() {
        assert_eq!(shown(Some("claude"), Some("node")), Some("claude"));
        assert_eq!(shown(None, Some("node")), Some("node"));
        assert_eq!(shown(Some("  "), Some("node")), Some("node"));
        assert_eq!(shown(Some("gemini"), None), Some("gemini"));
        assert_eq!(shown(None, None), None);
        let program = lookup(shown(Some("claude"), Some("node")).unwrap_or_default());
        assert_eq!(program.tile, Tile::Brand(0xd97757));
        assert_eq!(program.accent, Some(0xd97757));
    }

    /// `berth.toml` wins over the table, for a program in it or not.
    #[test]
    fn an_accent_from_berth_toml_comes_first() {
        let none = BTreeMap::new();
        assert_eq!(accent("claude", &none), Some(gpui::rgb(0xd97757)));
        assert_eq!(accent("make", &none), None);
        let mut given = BTreeMap::new();
        given.insert("claude".to_string(), [1, 2, 3]);
        given.insert("make".to_string(), [0xaa, 0xbb, 0xcc]);
        assert_eq!(accent("/usr/bin/claude", &given), Some(gpui::rgb(0x010203)));
        assert_eq!(accent("make", &given), Some(gpui::rgb(0xaabbcc)));
    }

    #[test]
    fn names_are_read_as_the_machine_gives_them() {
        assert_eq!(lookup("/usr/local/bin/nvim").icon, icons::NEOVIM);
        assert_eq!(lookup("python3.12").icon, icons::PYTHON);
        assert_eq!(lookup("Python3").icon, icons::PYTHON);
        assert_eq!(lookup("node22").icon, icons::NODE);
        assert_eq!(
            lookup("C:\\Program Files\\Git\\cmd\\git.exe").icon,
            icons::GIT
        );
        assert_eq!(lookup("  claude\n").name, "claude");
    }

    #[test]
    fn an_unknown_name_is_the_generic_terminal() {
        for unknown in ["htop", "cargo", "", "   ", "my-own-tool", "3.12"] {
            assert_eq!(lookup(unknown), GENERIC, "{unknown:?}");
        }
        assert_eq!(display_name("htop"), "htop");
        assert_eq!(display_name("/usr/bin/cargo"), "cargo");
    }

    #[test]
    fn shells_fall_back_to_the_terminal_icon_and_keep_their_name() {
        for shell in ["zsh", "-zsh", "bash", "/bin/sh", "fish", "pwsh.exe"] {
            let program = lookup(shell);
            assert_eq!(program.icon, icons::TERMINAL, "{shell}");
            assert_eq!(program.accent, None, "{shell}");
        }
        assert_eq!(display_name("-zsh"), "zsh");
    }

    /// Every icon the table names is one the binary carries.
    #[test]
    fn every_icon_in_the_table_loads() {
        use gpui::AssetSource;
        for (name, program) in TABLE {
            let loaded = icons::Icons.load(program.icon).unwrap();
            assert!(loaded.is_some(), "{name} names a missing icon");
        }
    }
}
