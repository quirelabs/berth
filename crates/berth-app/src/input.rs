//! Platform key events to Berth's key model.
//!
//! Both platform quirks live here, and they pull in opposite directions.
//!
//! Windows reports AltGr as Ctrl+Alt, so the modifiers alone cannot distinguish
//! `AltGr+e` from a real `Ctrl+Alt+e` chord. What separates them is that the
//! platform composed a character for one and not the other, so a present
//! `key_char` under Ctrl+Alt means AltGr, and that character is what to send.
//!
//! macOS composes for Option as well, but there Option is the Meta a terminal
//! needs, so the composed glyph is exactly what has to be discarded.

use gpui::Keystroke;

use berth_term::{Key, KeyEvent, Modifiers};

/// A clipboard chord, which the terminal must never receive as input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clipboard {
    Copy,
    Paste,
}

/// Recognise a clipboard chord.
///
/// Copy is Ctrl+Shift+C rather than Ctrl+C, because a bare Ctrl+C has to keep
/// reaching the remote as an interrupt; taking it for copy is the single most
/// annoying thing a terminal can do. On macOS the platform modifier is used, as
/// every other Mac application does, and Cmd is never sent to the remote
/// anyway.
/// A workspace command, as opposed to something the remote should see.
///
/// All of these are Cmd chords, which `berth_term` already refuses to send to
/// the remote, so taking them costs the shell nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    SplitRight,
    SplitDown,
    ClosePane,
    NextPane,
}

pub fn command(keystroke: &Keystroke) -> Option<Command> {
    let m = &keystroke.modifiers;
    if !m.platform || m.control || m.alt {
        return None;
    }
    match (keystroke.key.as_str(), m.shift) {
        ("d", false) => Some(Command::SplitRight),
        ("d", true) => Some(Command::SplitDown),
        // Cmd+W closes the pane, not the window, which is what it means in
        // every other terminal with splits.
        ("w", false) => Some(Command::ClosePane),
        ("]", _) => Some(Command::NextPane),
        _ => None,
    }
}

pub fn clipboard_action(keystroke: &Keystroke) -> Option<Clipboard> {
    let m = &keystroke.modifiers;
    let key = keystroke.key.as_str();

    if m.platform && !m.control && !m.alt {
        return match key {
            "c" => Some(Clipboard::Copy),
            "v" => Some(Clipboard::Paste),
            _ => None,
        };
    }
    if m.control && m.shift && !m.alt && !m.platform {
        return match key {
            "c" => Some(Clipboard::Copy),
            "v" => Some(Clipboard::Paste),
            _ => None,
        };
    }
    None
}

/// A chord that moves Berth's own view through the scrollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollKey {
    PageUp,
    PageDown,
}

/// Recognise a local scroll chord.
///
/// Shift is what makes these local, and it has to be: a bare PageUp is the key
/// `less`, `vim` and every pager on the far side are waiting for, so taking it
/// would break scrolling in exactly the programs people scroll in most.
pub fn scroll_action(keystroke: &Keystroke) -> Option<ScrollKey> {
    let m = &keystroke.modifiers;
    if !m.shift || m.control || m.alt || m.platform {
        return None;
    }
    match keystroke.key.as_str() {
        "pageup" => Some(ScrollKey::PageUp),
        "pagedown" => Some(ScrollKey::PageDown),
        _ => None,
    }
}

/// True where the platform composes a character for Option chords rather than
/// reporting them as Meta. macOS does; nothing else Berth targets does.
const OPTION_COMPOSES: bool = cfg!(target_os = "macos");

/// Translate a keystroke, or `None` if it produces no terminal input.
pub fn translate(keystroke: &Keystroke) -> Option<KeyEvent> {
    translate_with(keystroke, OPTION_COMPOSES)
}

/// The platform rule is a parameter so both branches are tested everywhere,
/// not only on the machine that happens to be running the suite.
fn translate_with(keystroke: &Keystroke, option_composes: bool) -> Option<KeyEvent> {
    let m = &keystroke.modifiers;
    let mods = Modifiers {
        ctrl: m.control,
        alt: m.alt,
        shift: m.shift,
        cmd: m.platform,
    };

    let mut composed = keystroke.key_char.as_deref().filter(|s| !s.is_empty());

    // Option is Meta. macOS composes a glyph for an Option chord, so Option+b
    // arrives as "b" with a key_char of "∫", and sending ESC plus that
    // glyph is neither the chord the user meant nor a character they can type.
    // A dead key is unaffected: its glyph lands on the next keystroke, which
    // carries no modifier.
    let option_as_meta = option_composes && m.alt && !m.control && !m.platform;
    if option_as_meta {
        composed = None;
    }

    // Ctrl+Alt plus a composed character is AltGr, not a chord.
    let altgr = m.control && m.alt && composed.is_some();

    let key = match keystroke.key.as_str() {
        "enter" => Key::Enter,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "escape" => Key::Escape,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "insert" => Key::Insert,
        "delete" => Key::Delete,
        "space" => Key::Char(' '),
        other => {
            if let Some(n) = other.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
                && (1..=12).contains(&n)
            {
                Key::Function(n)
            } else {
                // Prefer what the platform actually composed, so dead keys and
                // non-Latin layouts produce the character the user saw.
                let text = composed.unwrap_or(other);
                let mut chars = text.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    // Multi-character input is a paste or an IME commit, not a
                    // keystroke; those take a different path.
                    return None;
                }
                // Without the composed character there is nothing carrying the
                // case, so Shift has to apply it.
                if option_as_meta && m.shift {
                    Key::Char(c.to_ascii_uppercase())
                } else {
                    Key::Char(c)
                }
            }
        }
    };

    Some(KeyEvent { key, mods, altgr })
}

#[cfg(test)]
mod tests {
    use super::*;

    use gpui::Modifiers as GpuiModifiers;

    fn keystroke(key: &str, key_char: Option<&str>, modifiers: GpuiModifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.to_string(),
            key_char: key_char.map(str::to_string),
        }
    }

    fn plain() -> GpuiModifiers {
        GpuiModifiers::default()
    }

    fn ctrl() -> GpuiModifiers {
        GpuiModifiers {
            control: true,
            ..Default::default()
        }
    }

    fn ctrl_alt() -> GpuiModifiers {
        GpuiModifiers {
            control: true,
            alt: true,
            ..Default::default()
        }
    }

    fn alt() -> GpuiModifiers {
        GpuiModifiers {
            alt: true,
            ..Default::default()
        }
    }

    /// What macOS actually delivers for Option+b: the key is the unmodified
    /// letter, the composed character is the Option glyph.
    #[test]
    fn option_is_meta_and_drops_the_composed_glyph() {
        let ev = translate_with(&keystroke("b", Some("\u{222b}"), alt()), true).unwrap();
        assert_eq!(ev.key, Key::Char('b'), "Alt+b must stay a word motion");
        assert!(ev.mods.alt);
        assert!(!ev.altgr);
    }

    /// The same event on Windows, where the composed character is the point.
    #[test]
    fn alt_keeps_the_composed_character_where_option_does_not_compose() {
        let ev = translate_with(&keystroke("b", Some("\u{222b}"), alt()), false).unwrap();
        assert_eq!(ev.key, Key::Char('\u{222b}'));
    }

    #[test]
    fn shift_carries_the_case_once_the_glyph_is_gone() {
        let mods = GpuiModifiers {
            alt: true,
            shift: true,
            ..Default::default()
        };
        let ev = translate_with(&keystroke("b", Some("\u{131}"), mods), true).unwrap();
        assert_eq!(ev.key, Key::Char('B'));
    }

    /// Option is only Meta on its own. Cmd and Ctrl chords keep their meaning,
    /// and macOS reports no composed character for either.
    #[test]
    fn the_meta_rule_does_not_reach_ctrl_or_cmd_chords() {
        let ev = translate_with(&keystroke("e", Some("\u{20ac}"), ctrl_alt()), true).unwrap();
        assert!(ev.altgr, "the AltGr rule is untouched by the Option rule");
        assert_eq!(ev.key, Key::Char('\u{20ac}'));

        let cmd_alt = GpuiModifiers {
            alt: true,
            platform: true,
            ..Default::default()
        };
        let ev = translate_with(&keystroke("b", Some("\u{222b}"), cmd_alt), true).unwrap();
        assert_eq!(ev.key, Key::Char('\u{222b}'));
    }

    /// A dead key resolves on the following keystroke, which has no modifier,
    /// so the composed glyph still arrives.
    #[test]
    fn a_dead_key_sequence_survives_the_meta_rule() {
        let ev = translate_with(&keystroke("e", Some("\u{e9}"), plain()), true).unwrap();
        assert_eq!(ev.key, Key::Char('\u{e9}'));
    }

    /// Named keys never carried a composed character, on either platform.
    #[test]
    fn option_arrows_are_unaffected() {
        let ev = translate_with(&keystroke("left", None, alt()), true).unwrap();
        assert_eq!(ev.key, Key::Left);
        assert!(ev.mods.alt);
    }

    #[test]
    fn plain_letters_translate() {
        let ev = translate(&keystroke("a", Some("a"), plain())).unwrap();
        assert_eq!(ev.key, Key::Char('a'));
        assert!(!ev.altgr);
    }

    #[test]
    fn named_keys_translate() {
        for (name, expected) in [
            ("enter", Key::Enter),
            ("tab", Key::Tab),
            ("backspace", Key::Backspace),
            ("escape", Key::Escape),
            ("up", Key::Up),
            ("pagedown", Key::PageDown),
            ("delete", Key::Delete),
            ("space", Key::Char(' ')),
        ] {
            let ev = translate(&keystroke(name, None, plain())).unwrap();
            assert_eq!(ev.key, expected, "{name}");
        }
    }

    #[test]
    fn function_keys_translate() {
        assert_eq!(
            translate(&keystroke("f1", None, plain())).unwrap().key,
            Key::Function(1)
        );
        assert_eq!(
            translate(&keystroke("f12", None, plain())).unwrap().key,
            Key::Function(12)
        );
    }

    #[test]
    fn f13_is_not_a_function_key_we_encode() {
        // Falls through to the character path and produces nothing usable.
        let ev = translate(&keystroke("f13", None, plain()));
        assert!(ev.is_none() || ev.unwrap().key != Key::Function(13));
    }

    #[test]
    fn ctrl_chords_keep_their_modifier() {
        let ev = translate(&keystroke("c", None, ctrl())).unwrap();
        assert_eq!(ev.key, Key::Char('c'));
        assert!(ev.mods.ctrl);
        assert!(!ev.altgr);
    }

    /// The Windows case this module exists for.
    #[test]
    fn ctrl_alt_with_a_composed_character_is_altgr() {
        let ev = translate(&keystroke("e", Some("€"), ctrl_alt())).unwrap();
        assert!(ev.altgr, "AltGr+e must not be read as a Ctrl+Alt chord");
        assert_eq!(ev.key, Key::Char('€'));
    }

    /// The mirror case: a real chord composes nothing.
    #[test]
    fn ctrl_alt_without_a_composed_character_is_a_chord() {
        let ev = translate(&keystroke("c", None, ctrl_alt())).unwrap();
        assert!(!ev.altgr);
        assert!(ev.mods.ctrl && ev.mods.alt);
    }

    #[test]
    fn the_composed_character_wins_over_the_key_name() {
        // A dead-key sequence: the key is "e", the composed result is "é".
        let ev = translate(&keystroke("e", Some("é"), plain())).unwrap();
        assert_eq!(ev.key, Key::Char('é'));
    }

    #[test]
    fn multi_character_input_is_not_a_keystroke() {
        assert!(translate(&keystroke("x", Some("pasted text"), plain())).is_none());
    }

    #[test]
    fn empty_composed_text_falls_back_to_the_key() {
        let ev = translate(&keystroke("a", Some(""), plain())).unwrap();
        assert_eq!(ev.key, Key::Char('a'));
    }

    /// The rule that matters: Ctrl+C stays an interrupt.
    #[test]
    fn a_bare_ctrl_c_is_not_copy() {
        assert_eq!(clipboard_action(&keystroke("c", None, ctrl())), None);
        let event = translate(&keystroke("c", None, ctrl())).unwrap();
        assert!(event.mods.ctrl, "Ctrl+C must reach the remote");
    }

    #[test]
    fn ctrl_shift_c_and_v_are_clipboard_chords() {
        let ctrl_shift = GpuiModifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            clipboard_action(&keystroke("c", None, ctrl_shift)),
            Some(Clipboard::Copy)
        );
        assert_eq!(
            clipboard_action(&keystroke("v", None, ctrl_shift)),
            Some(Clipboard::Paste)
        );
        assert_eq!(clipboard_action(&keystroke("x", None, ctrl_shift)), None);
    }

    #[test]
    fn the_platform_modifier_copies_on_macos() {
        let cmd = GpuiModifiers {
            platform: true,
            ..Default::default()
        };
        assert_eq!(
            clipboard_action(&keystroke("c", Some("c"), cmd)),
            Some(Clipboard::Copy)
        );
        assert_eq!(
            clipboard_action(&keystroke("v", Some("v"), cmd)),
            Some(Clipboard::Paste)
        );
    }

    #[test]
    fn ordinary_typing_is_never_a_clipboard_chord() {
        assert_eq!(clipboard_action(&keystroke("c", Some("c"), plain())), None);
        assert_eq!(clipboard_action(&keystroke("v", Some("v"), plain())), None);
        assert_eq!(clipboard_action(&keystroke("c", None, ctrl_alt())), None);
    }

    #[test]
    fn the_platform_modifier_is_carried_through() {
        let mods = GpuiModifiers {
            platform: true,
            ..Default::default()
        };
        let ev = translate(&keystroke("k", Some("k"), mods)).unwrap();
        assert!(ev.mods.cmd, "cmd is a shortcut modifier, handled upstream");
    }

    fn cmd(key: &str, shift: bool) -> Keystroke {
        keystroke(
            key,
            None,
            GpuiModifiers {
                platform: true,
                shift,
                ..Default::default()
            },
        )
    }

    fn shift(key: &str) -> Keystroke {
        keystroke(
            key,
            None,
            GpuiModifiers {
                shift: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn shift_and_a_page_key_scrolls_berth() {
        assert_eq!(scroll_action(&shift("pageup")), Some(ScrollKey::PageUp));
        assert_eq!(scroll_action(&shift("pagedown")), Some(ScrollKey::PageDown));
    }

    /// The one that matters: a pager on the far side must keep getting its
    /// page keys, so a bare PageUp is not ours to take.
    #[test]
    fn a_bare_page_key_belongs_to_the_remote() {
        assert_eq!(scroll_action(&keystroke("pageup", None, plain())), None);
        assert_eq!(scroll_action(&keystroke("pagedown", None, plain())), None);
        assert!(translate(&keystroke("pageup", None, plain())).is_some());
    }

    #[test]
    fn a_page_key_with_another_modifier_is_not_a_scroll() {
        assert_eq!(scroll_action(&cmd("pageup", true)), None);
        let ctrl_shift = GpuiModifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(scroll_action(&keystroke("pageup", None, ctrl_shift)), None);
    }

    #[test]
    fn the_split_chords_are_distinct() {
        assert_eq!(command(&cmd("d", false)), Some(Command::SplitRight));
        assert_eq!(command(&cmd("d", true)), Some(Command::SplitDown));
        assert_eq!(command(&cmd("w", false)), Some(Command::ClosePane));
        assert_eq!(command(&cmd("]", false)), Some(Command::NextPane));
    }

    /// Every command is a Cmd chord, so nothing here can eat a key the shell
    /// needs. Ctrl+D in particular has to stay an EOF.
    #[test]
    fn a_bare_key_is_never_a_command() {
        for key in ["d", "w", "]", "c"] {
            assert_eq!(
                command(&keystroke(key, Some(key), plain())),
                None,
                "{key} was taken from the shell"
            );
            let ctrl = GpuiModifiers {
                control: true,
                ..Default::default()
            };
            assert_eq!(
                command(&keystroke(key, None, ctrl)),
                None,
                "ctrl+{key} was taken from the shell"
            );
        }
    }

    /// Copy and paste keep Cmd+C and Cmd+V; a command must not shadow them.
    #[test]
    fn commands_and_the_clipboard_do_not_overlap() {
        for key in ["c", "v"] {
            assert_eq!(command(&cmd(key, false)), None);
        }
        assert!(clipboard_action(&cmd("d", false)).is_none());
        assert!(clipboard_action(&cmd("w", false)).is_none());
    }
}
