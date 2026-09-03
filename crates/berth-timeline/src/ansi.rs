//! Strip escape sequences so stored output indexes as readable text.
//!
//! This is deliberately not a VT parser: it removes the sequences that would
//! otherwise pollute the search index, and leaves the printable text alone.

/// Remove CSI, OSC, and single-character escape sequences.
pub fn strip(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '\x1b' {
            // Carriage returns make a line look overwritten in the index.
            if c != '\r' {
                out.push(c);
            }
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates, then a final byte in @..~
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: runs to BEL or ST (ESC \)
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // DCS, SOS, PM, APC all terminate at ST.
            Some('P') | Some('X') | Some('^') | Some('_') => {
                while let Some(c) = chars.next() {
                    if c == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Two-character sequences such as ESC ( B; drop the charset byte.
            Some('(') | Some(')') | Some('*') | Some('+') => {
                chars.next();
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_untouched() {
        assert_eq!(strip("hello world"), "hello world");
    }

    #[test]
    fn removes_colour() {
        assert_eq!(strip("\x1b[31mred\x1b[0m"), "red");
    }

    #[test]
    fn removes_cursor_movement() {
        assert_eq!(strip("a\x1b[2Jb\x1b[1;5Hc"), "abc");
    }

    #[test]
    fn removes_osc_with_bel_terminator() {
        assert_eq!(strip("\x1b]0;title\x07text"), "text");
    }

    #[test]
    fn removes_osc_with_st_terminator() {
        assert_eq!(strip("\x1b]0;title\x1b\\text"), "text");
    }

    #[test]
    fn removes_osc_133_prompt_markers() {
        assert_eq!(strip("\x1b]133;A\x07$ ls\x1b]133;B\x07"), "$ ls");
    }

    #[test]
    fn removes_charset_selection() {
        assert_eq!(strip("\x1b(Bplain"), "plain");
    }

    #[test]
    fn drops_carriage_returns() {
        assert_eq!(strip("line\r\n"), "line\n");
    }

    #[test]
    fn keeps_newlines_so_lines_stay_separable() {
        assert_eq!(strip("a\nb\nc").lines().count(), 3);
    }

    #[test]
    fn unterminated_sequence_does_not_panic() {
        assert_eq!(strip("text\x1b["), "text");
        assert_eq!(strip("text\x1b"), "text");
        assert_eq!(strip("text\x1b]0;unterminated"), "text");
    }

    #[test]
    fn keeps_unicode_intact() {
        assert_eq!(strip("\x1b[32m✓ ünïcödé 日本\x1b[0m"), "✓ ünïcödé 日本");
    }
}
