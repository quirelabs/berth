//! A single-line text field.
//!
//! Berth has no editor crate to borrow one from: Zed's depends on `editor`,
//! `theme`, `settings`, `ui` and `workspace` (D17), which is the whole reason
//! the terminal element was written by hand. So this is written by hand too.
//!
//! The editing model is deliberately free of gpui. It is a string and a cursor
//! and a key mapping, all of which can be tested without a window, which
//! matters because the failure modes here are the ones that only appear on
//! input nobody types by accident: a cursor landing inside a multi-byte
//! character panics, and it panics in the middle of someone renaming a session.
//!
//! The M3 search overlay needs the same widget, so this is built once.

/// What a key did, so the caller knows whether to close the thing it is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The value changed, or the cursor moved. Repaint.
    Edited,
    /// Enter. The caller decides what accepting means.
    Submit,
    /// Escape. The caller decides what cancelling means.
    Cancel,
    /// Not ours. The caller may still want it.
    Ignored,
}

#[derive(Debug, Clone)]
pub struct TextField {
    value: String,
    /// A byte index into `value`, always on a character boundary.
    cursor: usize,
}

impl TextField {
    pub fn new(initial: impl Into<String>) -> Self {
        let value = initial.into();
        Self {
            cursor: value.len(),
            value,
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    /// Where the cursor is, in bytes. Only the tests ask: the view draws from
    /// [`split`](Self::split) instead, which cannot land mid-character.
    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// The text either side of the cursor, for drawing one between them.
    pub fn split(&self) -> (&str, &str) {
        self.value.split_at(self.cursor)
    }

    pub fn insert(&mut self, text: &str) {
        self.value.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        if let Some(previous) = self.previous_boundary() {
            self.value.replace_range(previous..self.cursor, "");
            self.cursor = previous;
        }
    }

    /// Delete the character after the cursor.
    pub fn delete(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.value.replace_range(self.cursor..next, "");
        }
    }

    pub fn left(&mut self) {
        if let Some(previous) = self.previous_boundary() {
            self.cursor = previous;
        }
    }

    pub fn right(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.cursor = next;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.value.len();
    }

    /// Byte index of the character boundary before the cursor.
    ///
    /// Stepping by one byte would land inside a multi-byte character and panic
    /// on the next edit, so movement is always by character.
    fn previous_boundary(&self) -> Option<usize> {
        self.value[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.value[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
    }

    /// Apply one keystroke.
    ///
    /// `key` is gpui's key name and `text` is what it would type, which gpui
    /// has already resolved through the keyboard layout and any dead keys. The
    /// two are separate because "the key called `a`" and "the character this
    /// layout produces" are not the same thing on any layout but US QWERTY.
    ///
    /// A modifier other than shift means the keystroke is a command rather than
    /// text, and is left for the caller. Otherwise a paste would arrive here as
    /// a literal `v`.
    pub fn handle(&mut self, key: &str, text: Option<&str>, command: bool) -> Outcome {
        if command {
            return Outcome::Ignored;
        }
        match key {
            "enter" => Outcome::Submit,
            "escape" => Outcome::Cancel,
            "backspace" => {
                self.backspace();
                Outcome::Edited
            }
            "delete" => {
                self.delete();
                Outcome::Edited
            }
            "left" => {
                self.left();
                Outcome::Edited
            }
            "right" => {
                self.right();
                Outcome::Edited
            }
            "home" => {
                self.home();
                Outcome::Edited
            }
            "end" => {
                self.end();
                Outcome::Edited
            }
            _ => match text {
                // Control characters are not text, whatever gpui reports for
                // the key that produced them.
                Some(text) if !text.is_empty() && !text.chars().any(char::is_control) => {
                    self.insert(text);
                    Outcome::Edited
                }
                _ => Outcome::Ignored,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_field_puts_the_cursor_at_the_end() {
        let field = TextField::new("deploy");
        assert_eq!(field.cursor(), 6);
        assert_eq!(field.value(), "deploy");
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let mut field = TextField::new("");
        for text in ["d", "e", "v"] {
            assert_eq!(field.handle("", Some(text), false), Outcome::Edited);
        }
        assert_eq!(field.value(), "dev");
        field.home();
        field.insert("re-");
        assert_eq!(field.value(), "re-dev");
        assert_eq!(field.cursor(), 3);
    }

    #[test]
    fn backspace_at_the_start_does_nothing() {
        let mut field = TextField::new("a");
        field.home();
        field.backspace();
        assert_eq!(field.value(), "a");
        assert_eq!(field.cursor(), 0);
    }

    #[test]
    fn delete_at_the_end_does_nothing() {
        let mut field = TextField::new("a");
        field.delete();
        assert_eq!(field.value(), "a");
    }

    /// The one that panics rather than misbehaving if movement is by byte.
    #[test]
    fn movement_and_deletion_step_whole_characters() {
        let mut field = TextField::new("héllo→");
        field.left();
        assert_eq!(field.value()[field.cursor()..], *"→");
        field.backspace();
        assert_eq!(field.value(), "héll→");

        field.home();
        field.right();
        field.right();
        assert_eq!(&field.value()[..field.cursor()], "hé");
        field.delete();
        assert_eq!(field.value(), "hél→");
    }

    #[test]
    fn a_field_of_only_wide_characters_survives_being_emptied() {
        let mut field = TextField::new("日本語");
        for _ in 0..5 {
            field.backspace();
        }
        assert!(field.is_empty());
        assert_eq!(field.cursor(), 0);
    }

    #[test]
    fn enter_and_escape_are_reported_not_typed() {
        let mut field = TextField::new("x");
        assert_eq!(field.handle("enter", None, false), Outcome::Submit);
        assert_eq!(field.handle("escape", None, false), Outcome::Cancel);
        assert_eq!(field.value(), "x", "neither key is text");
    }

    /// Otherwise cmd-V arrives as a literal `v` in the middle of the name.
    #[test]
    fn a_command_keystroke_is_left_alone() {
        let mut field = TextField::new("");
        assert_eq!(field.handle("v", Some("v"), true), Outcome::Ignored);
        assert!(field.is_empty());
    }

    #[test]
    fn control_characters_are_not_text() {
        let mut field = TextField::new("");
        assert_eq!(field.handle("tab", Some("\t"), false), Outcome::Ignored);
        assert_eq!(field.handle("", Some("\u{7f}"), false), Outcome::Ignored);
        assert!(field.is_empty());
    }

    #[test]
    fn split_puts_the_halves_either_side_of_the_cursor() {
        let mut field = TextField::new("deploy");
        field.home();
        field.right();
        field.right();
        assert_eq!(field.split(), ("de", "ploy"));
    }
}
