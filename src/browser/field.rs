/* -------------------------------------------------------------------------- */
/*                              browser/field.rs                              */
/* -------------------------------------------------------------------------- */

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;
use yansi::Paint;

/// A line of text being typed, with a cursor that moves a grapheme at a time.
pub struct Field {
    text: String,
    /// A byte offset into `text`, always on a grapheme boundary.
    cursor: usize,
}

impl Field {
    /// Holds `text`, with the cursor after it.
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            cursor: text.len(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Home => self.cursor = 0,
            KeyCode::Char('a') if control => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            KeyCode::Char('e') if control => self.cursor = self.text.len(),
            KeyCode::Left => self.cursor = self.previous(),
            KeyCode::Right => self.cursor = self.next(),
            KeyCode::Backspace => self.erase_back(self.previous()),
            KeyCode::Char('w') if control => self.erase_back(self.word_start()),
            KeyCode::Char('u') if control => self.erase_back(0),
            KeyCode::Delete => _ = self.text.drain(self.cursor..self.next()),
            KeyCode::Char(_) if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {}
            KeyCode::Char(c) => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
            _ => {}
        }
    }

    /// The text in `width` columns, scrolled to keep the cursor in view and
    /// drawn as an inverted grapheme.
    pub fn view(&self, width: usize) -> String {
        let (before, under, after) = self.window(width);
        format!("{before}{}{after}", under.invert())
    }

    /// What shows in `width` columns: before the cursor, under it (a space
    /// at the end), and after it.
    fn window(&self, width: usize) -> (String, &str, String) {
        let width = width.max(1);
        let graphemes: Vec<&str> = self.text.graphemes(true).collect();
        let at = self.text[..self.cursor].graphemes(true).count();
        let start = (at + 1).saturating_sub(width);
        let end = graphemes.len().min(start + width);
        let after = graphemes.get(at + 1..end).map(<[&str]>::concat).unwrap_or_default();
        (graphemes[start..at].concat(), graphemes.get(at).copied().unwrap_or(" "), after)
    }

    fn erase_back(&mut self, to: usize) {
        self.text.drain(to..self.cursor);
        self.cursor = to;
    }

    fn previous(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(at, _)| at)
    }

    fn next(&self) -> usize {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |grapheme| self.cursor + grapheme.len())
    }

    /// Where the word before the cursor starts, past any spaces after it.
    fn word_start(&self) -> usize {
        let before = self.text[..self.cursor].trim_end();
        before
            .char_indices()
            .rfind(|(_, c)| c.is_whitespace())
            .map_or(0, |(at, c)| at + c.len_utf8())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(field: &mut Field, keys: &[KeyCode]) {
        for &code in keys {
            field.key(KeyEvent::from(code));
        }
    }

    #[test]
    fn edits_by_grapheme_and_word() {
        let mut field = Field::new("café crème 🇵🇹");
        typed(&mut field, &[KeyCode::Left, KeyCode::Backspace]);
        assert_eq!(field.text(), "café crème🇵🇹");
        typed(&mut field, &[KeyCode::Right, KeyCode::Char('!')]);
        assert_eq!(field.text(), "café crème🇵🇹!");
        field.key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(field.text(), "café ");
        typed(&mut field, &[KeyCode::Home, KeyCode::Delete, KeyCode::Char('C')]);
        assert_eq!(field.text(), "Café ");
    }

    #[test]
    fn window_keeps_the_cursor_in_sight() {
        let mut field = Field::new("abcdef");
        assert_eq!(field.window(4), ("def".into(), " ", String::new()));
        typed(&mut field, &[KeyCode::Left, KeyCode::Left]);
        assert_eq!(field.window(4), ("bcd".into(), "e", String::new()));
        typed(&mut field, &[KeyCode::Home]);
        assert_eq!(field.window(4), (String::new(), "a", "bcd".into()));
    }
}
