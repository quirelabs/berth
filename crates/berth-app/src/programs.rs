//! What runs in a session, by name: one table from a foreground process name
//! to the icon a tab shows and the stone the otter holds (D53).
//!
//! The name is the process's own, as the machine reports it: `claude`,
//! `python3.12`, `-zsh`, `/usr/bin/nvim`. Anything not in the table, and any
//! shell, gets the generic terminal icon and the plain stone.
//!
//! The icons of other projects are their marks. They say which program runs
//! in a tab and nothing more, and are for dogfooding until each is cleared
//! for release (`assets/icons/THIRD-PARTY.md`).

use crate::icons;
use crate::otter::Stone;

/// A program Berth knows by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Program {
    /// How it is named in a title.
    pub name: &'static str,
    pub icon: &'static str,
    pub stone: Stone,
}

/// What an unknown program and a plain shell look like.
pub const GENERIC: Program = Program {
    name: "",
    icon: icons::TERMINAL,
    stone: Stone::Plain,
};

const fn tool(name: &'static str, icon: &'static str) -> Program {
    Program {
        name,
        icon,
        stone: Stone::Plain,
    }
}

/// Process names, lower case, to programs. A name with a version on its end
/// (`python3.12`, `node22`) is looked up without it.
const TABLE: &[(&str, Program)] = &[
    (
        "claude",
        Program {
            name: "claude",
            icon: icons::CLAUDE,
            stone: Stone::Claude,
        },
    ),
    (
        "codex",
        Program {
            name: "codex",
            icon: icons::CODEX,
            stone: Stone::Codex,
        },
    ),
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
    fn known_names_have_their_icon_and_stone() {
        assert_eq!(lookup("claude").icon, icons::CLAUDE);
        assert_eq!(lookup("claude").stone, Stone::Claude);
        assert_eq!(lookup("codex").stone, Stone::Codex);
        assert_eq!(lookup("node").icon, icons::NODE);
        assert_eq!(lookup("bun").icon, icons::BUN);
        assert_eq!(lookup("deno").icon, icons::DENO);
        assert_eq!(lookup("git").icon, icons::GIT);
        assert_eq!(lookup("lazygit").icon, icons::GIT);
        assert_eq!(lookup("docker").icon, icons::DOCKER);
        assert_eq!(lookup("vim").icon, icons::VIM);
        assert_eq!(lookup("nvim").icon, icons::NEOVIM);
        assert_eq!(lookup("ssh").icon, icons::SSH);
        assert_eq!(lookup("node").stone, Stone::Plain);
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
        assert_eq!(lookup("  claude\n").stone, Stone::Claude);
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
            assert_eq!(program.stone, Stone::Plain, "{shell}");
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
