//! Key events to terminal byte sequences.
//!
//! This is the most error-prone surface on Windows, where AltGr arrives as
//! Ctrl+Alt and would otherwise be encoded as a control sequence instead of the
//! character the user actually typed. The mapping is a pure function so the
//! whole table can be asserted; see the tests at the bottom of this file.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Cmd on macOS, Super on Windows. Never encoded; reserved for shortcuts.
    pub cmd: bool,
}

impl Modifiers {
    pub const NONE: Modifiers = Modifiers {
        ctrl: false,
        alt: false,
        shift: false,
        cmd: false,
    };

    pub fn ctrl() -> Self {
        Self {
            ctrl: true,
            ..Self::NONE
        }
    }

    pub fn alt() -> Self {
        Self {
            alt: true,
            ..Self::NONE
        }
    }

    pub fn shift() -> Self {
        Self {
            shift: true,
            ..Self::NONE
        }
    }

    /// The xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
    fn xterm_param(self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl)
    }

    fn any(self) -> bool {
        self.ctrl || self.alt || self.shift
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A character the platform has already composed, including anything
    /// produced by AltGr or a dead key.
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Right,
    Left,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Function(u8),
}

/// Terminal modes that change what a key encodes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TermMode {
    /// DECCKM. Set by full-screen applications; arrows send SS3 instead of CSI.
    pub application_cursor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    pub mods: Modifiers,
    /// True when the platform reports this event as AltGr rather than a genuine
    /// Ctrl+Alt chord. On Windows the two are indistinguishable from the
    /// modifier flags alone, so the platform layer must set this.
    pub altgr: bool,
}

impl KeyEvent {
    pub fn new(key: Key, mods: Modifiers) -> Self {
        Self {
            key,
            mods,
            altgr: false,
        }
    }

    pub fn altgr(key: Key) -> Self {
        Self {
            key,
            mods: Modifiers {
                ctrl: true,
                alt: true,
                ..Modifiers::NONE
            },
            altgr: true,
        }
    }
}

/// Encode a key event. Returns `None` when the event produces no bytes, which
/// is the correct behaviour for bare modifiers and for Cmd shortcuts.
pub fn encode(event: KeyEvent, mode: TermMode) -> Option<Vec<u8>> {
    // Cmd is a shortcut modifier and is never sent to the remote.
    if event.mods.cmd {
        return None;
    }

    match event.key {
        Key::Char(c) => encode_char(c, event.mods, event.altgr),
        Key::Enter => Some(prefix_alt(b"\r".to_vec(), event.mods)),
        Key::Tab => {
            if event.mods.shift {
                Some(b"\x1b[Z".to_vec())
            } else {
                Some(prefix_alt(b"\t".to_vec(), event.mods))
            }
        }
        // DEL is the conventional Backspace; Ctrl+Backspace sends BS.
        Key::Backspace => {
            let base = if event.mods.ctrl {
                vec![0x08]
            } else {
                vec![0x7f]
            };
            Some(prefix_alt(base, event.mods))
        }
        Key::Escape => Some(vec![0x1b]),
        Key::Up => Some(cursor_key(b'A', event.mods, mode)),
        Key::Down => Some(cursor_key(b'B', event.mods, mode)),
        Key::Right => Some(cursor_key(b'C', event.mods, mode)),
        Key::Left => Some(cursor_key(b'D', event.mods, mode)),
        Key::Home => Some(cursor_key(b'H', event.mods, mode)),
        Key::End => Some(cursor_key(b'F', event.mods, mode)),
        Key::Insert => Some(tilde_key(2, event.mods)),
        Key::Delete => Some(tilde_key(3, event.mods)),
        Key::PageUp => Some(tilde_key(5, event.mods)),
        Key::PageDown => Some(tilde_key(6, event.mods)),
        Key::Function(n) => function_key(n, event.mods),
    }
}

fn encode_char(c: char, mods: Modifiers, altgr: bool) -> Option<Vec<u8>> {
    // AltGr composed this character. Send it as typed; treating the Ctrl+Alt
    // that Windows reports as a real chord is the classic bug here.
    if altgr {
        return Some(prefix_alt(char_bytes(c), Modifiers::NONE));
    }

    if mods.ctrl {
        // Ctrl only has a meaning for a small ASCII range. Outside it, fall
        // through and send the character so dead keys and non-Latin layouts
        // are not swallowed.
        if let Some(byte) = control_byte(c) {
            return Some(prefix_alt(vec![byte], mods));
        }
    }

    Some(prefix_alt(char_bytes(c), mods))
}

/// The C0 code for Ctrl+<c>, if one exists.
fn control_byte(c: char) -> Option<u8> {
    match c {
        ' ' | '@' => Some(0x00),
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' | '?' => Some(0x1f),
        _ => None,
    }
}

fn char_bytes(c: char) -> Vec<u8> {
    let mut buf = [0u8; 4];
    c.encode_utf8(&mut buf).as_bytes().to_vec()
}

/// Alt is encoded as an ESC prefix, the convention every unix shell expects.
fn prefix_alt(mut bytes: Vec<u8>, mods: Modifiers) -> Vec<u8> {
    if mods.alt {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(0x1b);
        out.append(&mut bytes);
        out
    } else {
        bytes
    }
}

/// Arrows, Home and End. Modified keys always use the CSI form with a
/// parameter, even in application cursor mode, which is what xterm does.
fn cursor_key(final_byte: u8, mods: Modifiers, mode: TermMode) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[1;{}{}", mods.xterm_param(), final_byte as char).into_bytes()
    } else if mode.application_cursor {
        vec![0x1b, b'O', final_byte]
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

/// The `CSI n ~` family: Insert, Delete, PageUp, PageDown.
fn tilde_key(n: u8, mods: Modifiers) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[{n};{}~", mods.xterm_param()).into_bytes()
    } else {
        format!("\x1b[{n}~").into_bytes()
    }
}

fn function_key(n: u8, mods: Modifiers) -> Option<Vec<u8>> {
    // F1-F4 are SS3 keys; F5 and up use the CSI tilde form with a gap in the
    // numbering that xterm inherited from the VT220.
    let csi_number = match n {
        1..=4 => {
            let final_byte = b'P' + (n - 1);
            return Some(if mods.any() {
                format!("\x1b[1;{}{}", mods.xterm_param(), final_byte as char).into_bytes()
            } else {
                vec![0x1b, b'O', final_byte]
            });
        }
        5 => 15,
        6..=10 => 17 + (n - 6),
        11 => 23,
        12 => 24,
        _ => return None,
    };
    Some(tilde_key(csi_number, mods))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(key: Key, mods: Modifiers) -> Vec<u8> {
        encode(KeyEvent::new(key, mods), TermMode::default()).unwrap()
    }

    fn enc_app(key: Key, mods: Modifiers) -> Vec<u8> {
        encode(
            KeyEvent::new(key, mods),
            TermMode {
                application_cursor: true,
            },
        )
        .unwrap()
    }

    #[test]
    fn plain_characters_pass_through() {
        assert_eq!(enc(Key::Char('a'), Modifiers::NONE), b"a");
        assert_eq!(enc(Key::Char('Z'), Modifiers::shift()), b"Z");
    }

    #[test]
    fn control_letters_map_to_c0() {
        assert_eq!(enc(Key::Char('a'), Modifiers::ctrl()), vec![0x01]);
        assert_eq!(enc(Key::Char('c'), Modifiers::ctrl()), vec![0x03]);
        assert_eq!(enc(Key::Char('d'), Modifiers::ctrl()), vec![0x04]);
        assert_eq!(enc(Key::Char('z'), Modifiers::ctrl()), vec![0x1a]);
        // Case must not change the control code.
        assert_eq!(enc(Key::Char('C'), Modifiers::ctrl()), vec![0x03]);
    }

    #[test]
    fn control_punctuation() {
        assert_eq!(enc(Key::Char(' '), Modifiers::ctrl()), vec![0x00]);
        assert_eq!(enc(Key::Char('['), Modifiers::ctrl()), vec![0x1b]);
        assert_eq!(enc(Key::Char('\\'), Modifiers::ctrl()), vec![0x1c]);
        assert_eq!(enc(Key::Char(']'), Modifiers::ctrl()), vec![0x1d]);
        assert_eq!(enc(Key::Char('_'), Modifiers::ctrl()), vec![0x1f]);
    }

    #[test]
    fn ctrl_with_no_control_code_sends_the_character() {
        // Ctrl+1 has no C0 equivalent; swallowing it would lose the keystroke.
        assert_eq!(enc(Key::Char('1'), Modifiers::ctrl()), b"1");
        assert_eq!(enc(Key::Char('é'), Modifiers::ctrl()), "é".as_bytes());
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(enc(Key::Char('b'), Modifiers::alt()), vec![0x1b, b'b']);
        assert_eq!(enc(Key::Char('f'), Modifiers::alt()), vec![0x1b, b'f']);
    }

    #[test]
    fn ctrl_alt_combines_escape_and_c0() {
        let mods = Modifiers {
            ctrl: true,
            alt: true,
            ..Modifiers::NONE
        };
        assert_eq!(enc(Key::Char('c'), mods), vec![0x1b, 0x03]);
    }

    /// The Windows bug this table exists to prevent: AltGr arrives with both
    /// Ctrl and Alt set, but must send the composed character.
    #[test]
    fn altgr_sends_the_composed_character_not_a_control_chord() {
        let ev = KeyEvent::altgr(Key::Char('@'));
        assert_eq!(encode(ev, TermMode::default()).unwrap(), b"@");

        let ev = KeyEvent::altgr(Key::Char('e'));
        assert_eq!(
            encode(ev, TermMode::default()).unwrap(),
            b"e",
            "AltGr+e must not become Ctrl+E"
        );

        let ev = KeyEvent::altgr(Key::Char('€'));
        assert_eq!(encode(ev, TermMode::default()).unwrap(), "€".as_bytes());
    }

    #[test]
    fn arrows_in_normal_mode() {
        assert_eq!(enc(Key::Up, Modifiers::NONE), b"\x1b[A");
        assert_eq!(enc(Key::Down, Modifiers::NONE), b"\x1b[B");
        assert_eq!(enc(Key::Right, Modifiers::NONE), b"\x1b[C");
        assert_eq!(enc(Key::Left, Modifiers::NONE), b"\x1b[D");
    }

    #[test]
    fn arrows_in_application_mode() {
        assert_eq!(enc_app(Key::Up, Modifiers::NONE), b"\x1bOA");
        assert_eq!(enc_app(Key::Left, Modifiers::NONE), b"\x1bOD");
    }

    #[test]
    fn modified_arrows_use_the_csi_parameter_form() {
        assert_eq!(enc(Key::Right, Modifiers::ctrl()), b"\x1b[1;5C");
        assert_eq!(enc(Key::Left, Modifiers::alt()), b"\x1b[1;3D");
        assert_eq!(enc(Key::Up, Modifiers::shift()), b"\x1b[1;2A");
        // Modified arrows keep the CSI form even in application cursor mode.
        assert_eq!(enc_app(Key::Right, Modifiers::ctrl()), b"\x1b[1;5C");
    }

    #[test]
    fn home_and_end() {
        assert_eq!(enc(Key::Home, Modifiers::NONE), b"\x1b[H");
        assert_eq!(enc(Key::End, Modifiers::NONE), b"\x1b[F");
        assert_eq!(enc_app(Key::Home, Modifiers::NONE), b"\x1bOH");
    }

    #[test]
    fn editing_keys() {
        assert_eq!(enc(Key::Insert, Modifiers::NONE), b"\x1b[2~");
        assert_eq!(enc(Key::Delete, Modifiers::NONE), b"\x1b[3~");
        assert_eq!(enc(Key::PageUp, Modifiers::NONE), b"\x1b[5~");
        assert_eq!(enc(Key::PageDown, Modifiers::NONE), b"\x1b[6~");
        assert_eq!(enc(Key::Delete, Modifiers::ctrl()), b"\x1b[3;5~");
    }

    #[test]
    fn backspace_variants() {
        assert_eq!(enc(Key::Backspace, Modifiers::NONE), vec![0x7f]);
        assert_eq!(enc(Key::Backspace, Modifiers::ctrl()), vec![0x08]);
        assert_eq!(enc(Key::Backspace, Modifiers::alt()), vec![0x1b, 0x7f]);
    }

    #[test]
    fn enter_and_tab() {
        assert_eq!(enc(Key::Enter, Modifiers::NONE), b"\r");
        assert_eq!(enc(Key::Tab, Modifiers::NONE), b"\t");
        assert_eq!(enc(Key::Tab, Modifiers::shift()), b"\x1b[Z");
        assert_eq!(enc(Key::Escape, Modifiers::NONE), vec![0x1b]);
    }

    #[test]
    fn function_keys() {
        assert_eq!(enc(Key::Function(1), Modifiers::NONE), b"\x1bOP");
        assert_eq!(enc(Key::Function(4), Modifiers::NONE), b"\x1bOS");
        assert_eq!(enc(Key::Function(5), Modifiers::NONE), b"\x1b[15~");
        assert_eq!(enc(Key::Function(6), Modifiers::NONE), b"\x1b[17~");
        assert_eq!(enc(Key::Function(10), Modifiers::NONE), b"\x1b[21~");
        assert_eq!(enc(Key::Function(11), Modifiers::NONE), b"\x1b[23~");
        assert_eq!(enc(Key::Function(12), Modifiers::NONE), b"\x1b[24~");
        assert_eq!(enc(Key::Function(1), Modifiers::ctrl()), b"\x1b[1;5P");
    }

    #[test]
    fn unknown_function_key_produces_nothing() {
        assert!(
            encode(
                KeyEvent::new(Key::Function(13), Modifiers::NONE),
                TermMode::default()
            )
            .is_none()
        );
    }

    #[test]
    fn cmd_is_never_sent_to_the_remote() {
        let mods = Modifiers {
            cmd: true,
            ..Modifiers::NONE
        };
        assert!(encode(KeyEvent::new(Key::Char('k'), mods), TermMode::default()).is_none());
    }

    #[test]
    fn multibyte_characters_are_utf8_encoded() {
        assert_eq!(enc(Key::Char('日'), Modifiers::NONE), "日".as_bytes());
        assert_eq!(enc(Key::Char('🚀'), Modifiers::NONE), "🚀".as_bytes());
    }

    #[test]
    fn xterm_modifier_parameters_match_the_specification() {
        assert_eq!(Modifiers::shift().xterm_param(), 2);
        assert_eq!(Modifiers::alt().xterm_param(), 3);
        assert_eq!(Modifiers::ctrl().xterm_param(), 5);
        let all = Modifiers {
            ctrl: true,
            alt: true,
            shift: true,
            cmd: false,
        };
        assert_eq!(all.xterm_param(), 8);
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// Encoding never panics and never returns an empty sequence.
            #[test]
            fn encoding_is_total(
                c in any::<char>(),
                ctrl in any::<bool>(),
                alt in any::<bool>(),
                shift in any::<bool>(),
                app in any::<bool>(),
            ) {
                let mods = Modifiers { ctrl, alt, shift, cmd: false };
                let mode = TermMode { application_cursor: app };
                let out = encode(KeyEvent::new(Key::Char(c), mods), mode);
                prop_assert!(out.as_ref().is_some_and(|b| !b.is_empty()));
            }

            /// Alt always adds exactly one leading ESC to the unmodified form.
            #[test]
            fn alt_is_an_escape_prefix(c in any::<char>()) {
                let plain = encode(
                    KeyEvent::new(Key::Char(c), Modifiers::NONE),
                    TermMode::default(),
                ).unwrap();
                let alted = encode(
                    KeyEvent::new(Key::Char(c), Modifiers::alt()),
                    TermMode::default(),
                ).unwrap();
                prop_assert_eq!(alted.len(), plain.len() + 1);
                prop_assert_eq!(alted[0], 0x1b);
                prop_assert_eq!(&alted[1..], &plain[..]);
            }

            /// AltGr output never contains a C0 control byte, whatever the
            /// character; this is the regression guard for the Windows bug.
            #[test]
            fn altgr_never_emits_a_control_byte(c in any::<char>()) {
                prop_assume!(!c.is_control());
                let out = encode(KeyEvent::altgr(Key::Char(c)), TermMode::default()).unwrap();
                prop_assert!(out.iter().all(|b| *b >= 0x20 || *b == 0x7f) || out == c.to_string().into_bytes());
            }
        }
    }
}
