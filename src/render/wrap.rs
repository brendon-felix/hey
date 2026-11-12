/* -------------------------------------------------------------------------- */
/*                               render/wrap.rs                               */
/* -------------------------------------------------------------------------- */

use std::ops::Range;

use textwrap::core::display_width;

/// Splits `line` into rows no wider than `width`, after spaces, or inside a
/// word too wide for a row of its own. Breaking at the first row that
/// overflows, rather than balancing rows, means text appended to `line` can
/// only change its last row, so the others can be printed before it ends.
/// Spaces at a break may run past `width`; [`trimmed`] drops them.
pub fn rows(line: &str, width: usize) -> Vec<Range<usize>> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let (mut start, mut used, mut at) = (0, 0, 0);
    for piece in line.split_inclusive(' ') {
        let word = piece.trim_end_matches(' ');
        if used > 0 && used + display_width(word) > width {
            rows.push(start..at);
            (start, used) = (at, 0);
        }
        for (offset, char) in word.char_indices() {
            let char_width = display_width(char.encode_utf8(&mut [0; 4]));
            if used > 0 && used + char_width > width {
                rows.push(start..at + offset);
                (start, used) = (at + offset, 0);
            }
            used += char_width;
        }
        used += piece.len() - word.len();
        at += piece.len();
    }
    rows.push(start..line.len());
    rows
}

/// `row` without the whitespace that ends it.
pub fn trimmed(line: &str, row: &Range<usize>) -> Range<usize> {
    row.start..row.start + line[row.clone()].trim_end().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(line: &str, width: usize) -> Vec<&str> {
        rows(line, width)
            .iter()
            .map(|row| &line[trimmed(line, row)])
            .collect()
    }

    #[test]
    fn breaks_after_words_and_inside_long_ones() {
        assert_eq!(texts("the quick brown fox", 10), ["the quick", "brown fox"]);
        assert_eq!(texts("a abcdefghijkl b", 5), ["a", "abcde", "fghij", "kl b"]);
        assert_eq!(texts("日本語の文章", 5), ["日本", "語の", "文章"]);
        assert_eq!(texts("", 5), [""]);
    }

    #[test]
    fn rows_before_the_last_never_change_as_the_line_grows() {
        let line = "Streaming replies wrap  as they arrive, even through averyveryverylongword.";
        for width in 1..20 {
            let full = rows(line, width);
            for end in (0..=line.len()).filter(|&end| line.is_char_boundary(end)) {
                let partial = rows(&line[..end], width);
                let settled = &partial[..partial.len() - 1];
                assert_eq!(settled, &full[..settled.len()], "width {width}, prefix {end}");
            }
        }
    }
}
