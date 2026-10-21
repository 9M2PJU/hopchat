//! The message composer: multi-line text with a cursor, word motions and
//! soft wrapping that keeps the cursor where it is drawn.

use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct Editor {
    text: String,
    /// Byte offset into `text`, always on a char boundary.
    cursor: usize,
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.len();
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    fn prev(&self, at: usize) -> usize {
        self.text[..at].char_indices().next_back().map_or(0, |(i, _)| i)
    }

    fn next(&self, at: usize) -> usize {
        self.text[at..].chars().next().map_or(at, |c| at + c.len_utf8())
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let start = self.prev(self.cursor);
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let end = self.next(self.cursor);
            self.text.replace_range(self.cursor..end, "");
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.prev(self.cursor);
    }

    pub fn right(&mut self) {
        self.cursor = self.next(self.cursor);
    }

    fn word_start(&self) -> usize {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end_matches(|c: char| !c.is_alphanumeric());
        trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| !c.is_alphanumeric())
            .map_or(0, |(i, c)| i + c.len_utf8())
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_start();
    }

    pub fn word_right(&mut self) {
        let after = &self.text[self.cursor..];
        let skip = after
            .char_indices()
            .find(|(_, c)| c.is_alphanumeric())
            .map_or(after.len(), |(i, _)| i);
        let word = after[skip..]
            .char_indices()
            .find(|(_, c)| !c.is_alphanumeric())
            .map_or(after.len() - skip, |(i, _)| i);
        self.cursor += skip + word;
    }

    pub fn delete_word(&mut self) {
        let start = self.word_start();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i)
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    /// Deletes back to the start of the line (Ctrl-U).
    pub fn kill_line(&mut self) {
        let start = self.line_start();
        let start = if start == self.cursor { self.prev(start) } else { start };
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Moves up a line; false when already on the first.
    pub fn up(&mut self) -> bool {
        let start = self.line_start();
        if start == 0 {
            return false;
        }
        let column = self.text[start..self.cursor].chars().count();
        let prev_start = self.text[..start - 1].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = advance_chars(&self.text, prev_start, start - 1, column);
        true
    }

    /// Moves down a line; false when already on the last.
    pub fn down(&mut self) -> bool {
        let end = self.line_end();
        if end == self.text.len() {
            return false;
        }
        let column = self.text[self.line_start()..self.cursor].chars().count();
        let next_start = end + 1;
        let next_end = self.text[next_start..]
            .find('\n')
            .map_or(self.text.len(), |i| next_start + i);
        self.cursor = advance_chars(&self.text, next_start, next_end, column);
        true
    }

    /// The text soft-wrapped to `width` columns, and the cursor's (column,
    /// row) within those rows.
    pub fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(2);
        let mut rows = vec![String::new()];
        let mut col = 0;
        let mut cursor = (0, 0);
        for (i, c) in self.text.char_indices() {
            if i == self.cursor {
                cursor = (col, rows.len() - 1);
            }
            if c == '\n' {
                rows.push(String::new());
                col = 0;
                continue;
            }
            let w = c.width().unwrap_or(0);
            if col + w > width {
                rows.push(String::new());
                col = 0;
            }
            rows.last_mut().unwrap().push(c);
            col += w;
        }
        if self.cursor == self.text.len() {
            if col >= width {
                rows.push(String::new());
                col = 0;
            }
            cursor = (col, rows.len() - 1);
        }
        (rows, cursor)
    }
}

/// The byte offset `column` chars into `text[from..to]`, clamped to `to`.
fn advance_chars(text: &str, from: usize, to: usize, column: usize) -> usize {
    text[from..to]
        .char_indices()
        .nth(column)
        .map_or(to, |(i, _)| from + i)
}

#[cfg(test)]
mod tests {
    use super::Editor;

    #[test]
    fn edits_and_moves() {
        let mut e = Editor::default();
        e.insert_str("hello wörld");
        e.word_left();
        e.insert('X');
        assert_eq!(e.text(), "hello Xwörld");
        e.end();
        e.delete_word();
        assert_eq!(e.text(), "hello ");
        e.insert_str("a\nbc");
        assert!(e.up());
        assert!(!e.up());
        assert!(e.down());
        e.kill_line();
        assert_eq!(e.text(), "hello a\n");
        e.backspace();
        assert_eq!(e.take(), "hello a");
    }

    #[test]
    fn wraps_and_tracks_cursor() {
        let mut e = Editor::default();
        e.insert_str("abcdef");
        let (rows, cursor) = e.layout(4);
        assert_eq!(rows, ["abcd", "ef"]);
        assert_eq!(cursor, (2, 1));
        e.set("abcd");
        assert_eq!(e.layout(4).1, (0, 1));
    }
}
