//! Platform key events to Berth's key model.
//!
//! The AltGr rule lives here. Windows reports AltGr as Ctrl+Alt, so the
//! modifiers alone cannot distinguish `AltGr+e` from a real `Ctrl+Alt+e`
//! chord. What separates them is that the platform composed a character for
//! one and not the other, so a present `key_char` under Ctrl+Alt means AltGr.

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

/// Translate a keystroke, or `None` if it produces no terminal input.
pub fn translate(keystroke: &Keystroke) -> Option<KeyEvent> {
    let m = &keystroke.modifiers;
    let mods = Modifiers {
        ctrl: m.control,
        alt: m.alt,
        shift: m.shift,
        cmd: m.platform,
    };

    let composed = keystroke.key_char.as_deref().filter(|s| !s.is_empty());
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
                Key::Char(c)
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
}
