/* -------------------------------------------------------------------------- */
/*                               render/code.rs                               */
/* -------------------------------------------------------------------------- */

use textwrap::core::display_width;

use super::highlight::Highlighter;
use super::theme;

/// Lines of one file highlighted in its language, or left plain when it
/// has none or highlighting is off.
pub struct Code(Option<Highlighter>);

impl Code {
    pub fn new(path: Option<&str>, on: bool) -> Self {
        Self(
            path.filter(|_| on)
                .and_then(|path| Highlighter::for_path(theme::current(), path)),
        )
    }

    /// Whether lines are highlighted.
    pub fn is_on(&self) -> bool {
        self.0.is_some()
    }

    /// The next line wrapped into rows of at most `width` columns, tabs as
    /// four spaces, each row highlighted.
    pub fn wrapped(&mut self, line: &str, width: usize) -> Vec<String> {
        let line = line.replace('\t', "    ").replace('\r', "");
        let rows = super::wrap::rows(&line, width);
        let Some(highlighter) = &mut self.0 else {
            return rows
                .iter()
                .map(|row| line[super::wrap::trimmed(&line, row)].to_owned())
                .collect();
        };
        let text = format!("{line}\n");
        let spans = highlighter.line(&text);
        rows.iter()
            .map(|row| {
                let row = super::wrap::trimmed(&line, row);
                spans
                    .iter()
                    .filter_map(|(color, range)| {
                        let range = range.start.max(row.start)..range.end.min(row.end);
                        (range.start < range.end).then(|| color.paint(&text[range]).to_string())
                    })
                    .collect()
            })
            .collect()
    }

    /// [`Self::row`], and the columns it takes.
    pub fn sized(&mut self, line: &str, width: usize) -> (String, usize) {
        let line = line.replace('\t', "    ").replace('\r', "");
        let cut = if display_width(&line) > width {
            let mut used = 0;
            let end = line
                .char_indices()
                .find(|&(_, c)| {
                    used += display_width(c.encode_utf8(&mut [0; 4]));
                    used >= width
                })
                .map_or(line.len(), |(at, _)| at);
            Some(end)
        } else {
            None
        };
        let shown = &line[..cut.unwrap_or(line.len())];
        let ellipsis = if cut.is_some() { "…" } else { "" };
        let Some(highlighter) = &mut self.0 else {
            return (
                format!("{shown}{ellipsis}"),
                display_width(shown) + cut.map_or(0, |_| 1),
            );
        };
        let text = format!("{line}\n");
        let painted: String = highlighter
            .line(&text)
            .into_iter()
            .filter_map(|(color, range)| {
                let range = range.start..range.end.min(shown.len());
                (range.start < range.end).then(|| color.paint(&text[range]).to_string())
            })
            .collect();
        (
            format!("{painted}{ellipsis}"),
            display_width(shown) + cut.map_or(0, |_| 1),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::read_lines;

    #[test]
    fn read_lines_drop_trailing_notes() {
        let text = "a\n[b]\n[3 more lines; read on with offset 3]\n";
        assert_eq!(read_lines(text), "a\n[b]\n");
        assert_eq!(read_lines("a\nb\n"), "a\nb");
        assert_eq!(read_lines("[empty file]\n"), "");
    }
}
