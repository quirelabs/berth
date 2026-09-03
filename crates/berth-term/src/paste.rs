//! Paste and mouse encoding.
//!
//! Both are cases where the remote asks for a behaviour and the client must
//! honour it exactly. Getting bracketed paste wrong is the dangerous one: a
//! paste containing a newline runs as a command if the wrapping is missing.

/// Sent before pasted text when the remote has enabled bracketed paste.
pub const PASTE_START: &[u8] = b"\x1b[200~";
pub const PASTE_END: &[u8] = b"\x1b[201~";

/// Encode text for pasting.
///
/// When `bracketed`, the payload is wrapped so the remote's line editor treats
/// it as literal input. Any embedded end marker is stripped, because leaving it
/// in would let pasted content close the bracket early and have the remainder
/// executed. That is the whole security value of bracketed paste and it is
/// trivially lost by concatenating without checking.
///
/// Carriage returns are normalised to line feeds either way: a CRLF paste from
/// a Windows clipboard otherwise submits every line twice.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let normalised = text.replace("\r\n", "\n").replace('\r', "\n");

    if !bracketed {
        return normalised.into_bytes();
    }

    let cleaned = normalised.replace("\x1b[201~", "");
    let mut out = Vec::with_capacity(cleaned.len() + PASTE_START.len() + PASTE_END.len());
    out.extend_from_slice(PASTE_START);
    out.extend_from_slice(cleaned.as_bytes());
    out.extend_from_slice(PASTE_END);
    out
}

/// A mouse button, as the SGR protocol numbers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left = 0,
    Middle = 1,
    Right = 2,
    ScrollUp = 64,
    ScrollDown = 65,
}

/// Encode a mouse event in SGR form, `CSI < b ; col ; row M` for a press and
/// `m` for a release. Coordinates are one-based, as the protocol requires.
pub fn mouse_sgr(button: MouseButton, column: u16, line: u16, pressed: bool) -> Vec<u8> {
    format!(
        "\x1b[<{};{};{}{}",
        button as u16,
        column.saturating_add(1),
        line.saturating_add(1),
        if pressed { 'M' } else { 'm' }
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unbracketed_paste_is_the_text_itself() {
        assert_eq!(paste_bytes("hello", false), b"hello");
    }

    #[test]
    fn a_bracketed_paste_is_wrapped() {
        let out = paste_bytes("hello", true);
        assert!(out.starts_with(PASTE_START));
        assert!(out.ends_with(PASTE_END));
        assert_eq!(
            &out[PASTE_START.len()..out.len() - PASTE_END.len()],
            b"hello"
        );
    }

    /// The attack bracketed paste exists to prevent: content that closes the
    /// bracket early so the rest is executed.
    #[test]
    fn an_embedded_end_marker_cannot_close_the_bracket() {
        let hostile = "safe\x1b[201~rm -rf /\n";
        let out = paste_bytes(hostile, true);
        let body = &out[PASTE_START.len()..out.len() - PASTE_END.len()];
        assert!(
            !body.windows(PASTE_END.len()).any(|w| w == PASTE_END),
            "the payload must not contain an end marker"
        );
        // Exactly one end marker overall, the one this function added.
        assert_eq!(
            out.windows(PASTE_END.len())
                .filter(|w| *w == PASTE_END)
                .count(),
            1
        );
    }

    #[test]
    fn crlf_is_normalised_so_lines_are_not_submitted_twice() {
        assert_eq!(paste_bytes("a\r\nb", false), b"a\nb");
        assert_eq!(paste_bytes("a\rb", false), b"a\nb");
        let bracketed = paste_bytes("a\r\nb", true);
        assert!(!bracketed.contains(&b'\r'));
    }

    #[test]
    fn newlines_survive_a_bracketed_paste() {
        let out = paste_bytes("one\ntwo", true);
        let body = &out[PASTE_START.len()..out.len() - PASTE_END.len()];
        assert_eq!(body, b"one\ntwo");
    }

    #[test]
    fn an_empty_paste_is_still_well_formed() {
        let out = paste_bytes("", true);
        assert_eq!(out.len(), PASTE_START.len() + PASTE_END.len());
    }

    #[test]
    fn sgr_mouse_coordinates_are_one_based() {
        assert_eq!(mouse_sgr(MouseButton::Left, 0, 0, true), b"\x1b[<0;1;1M");
        assert_eq!(mouse_sgr(MouseButton::Right, 9, 4, false), b"\x1b[<2;10;5m");
    }

    #[test]
    fn scroll_uses_the_high_button_numbers() {
        assert_eq!(
            mouse_sgr(MouseButton::ScrollUp, 0, 0, true),
            b"\x1b[<64;1;1M"
        );
        assert_eq!(
            mouse_sgr(MouseButton::ScrollDown, 0, 0, true),
            b"\x1b[<65;1;1M"
        );
    }

    #[test]
    fn extreme_coordinates_do_not_overflow() {
        let out = mouse_sgr(MouseButton::Left, u16::MAX, u16::MAX, true);
        assert!(String::from_utf8(out).unwrap().contains("65535"));
    }
}
