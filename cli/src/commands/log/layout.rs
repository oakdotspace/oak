//! Width-specific row indexing for the log detail viewport.
//!
//! The wrapping state machine below is adapted from Ratatui 0.30.2
//! (ratatui-widgets 0.3.2, reflow.rs), with trim=false. It records borrowed
//! source ranges instead of storing or rendering every row on each frame.
/*
The MIT License (MIT)

Copyright (c) 2016-2022 Florian Dehau
Copyright (c) 2023-2025 The Ratatui Developers

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
*/

use ratatui::{buffer::CellWidth, prelude::*};
use std::{
    collections::{HashMap, VecDeque},
    ops::Range,
};

#[derive(Clone, Debug)]
struct Fragment {
    span: usize,
    bytes: Range<usize>,
}

pub(super) struct DetailLayout {
    pub width: u16,
    starts: Vec<usize>,
    wrapped: HashMap<usize, Vec<Vec<Fragment>>>,
    long_ascii: HashMap<usize, Vec<Range<usize>>>,
}

impl DetailLayout {
    pub fn new(lines: &[Line<'_>], width: u16) -> Self {
        let mut starts = Vec::with_capacity(lines.len() + 1);
        let mut wrapped = HashMap::new();
        let mut long_ascii = HashMap::new();
        starts.push(0usize);
        for (index, line) in lines.iter().enumerate() {
            let count = if width == 0
                || line.spans.iter().map(|s| s.content.len()).sum::<usize>() <= usize::from(width)
            {
                1
            } else if let Some(text) = single_ascii(line) {
                // Millions of ordinary diff lines need counts, not millions of
                // separately allocated row/fragment vectors. Only exceptionally
                // long individual lines retain compact random-access boundaries.
                let mut count = 0;
                let mut boundaries = Vec::new();
                wrap_ascii(text, usize::from(width), |range| {
                    count += 1;
                    if text.len() > 4096 {
                        boundaries.push(range);
                    }
                });
                if !boundaries.is_empty() {
                    long_ascii.insert(index, boundaries);
                }
                count
            } else if line
                .styled_graphemes(Style::default())
                .map(|g| usize::from(g.symbol.cell_width()))
                .sum::<usize>()
                <= usize::from(width)
            {
                1
            } else {
                let rows = wrap_line(line, width);
                let count = rows.len();
                wrapped.insert(index, rows);
                count
            };
            starts.push(starts.last().unwrap().saturating_add(count));
        }
        Self {
            width,
            starts,
            wrapped,
            long_ascii,
        }
    }

    pub fn row_count(&self) -> usize {
        *self.starts.last().unwrap_or(&0)
    }

    pub fn visible<'a>(
        &self,
        lines: &'a [Line<'static>],
        start: usize,
        height: usize,
    ) -> Vec<Line<'a>> {
        let end = start.saturating_add(height).min(self.row_count());
        if start >= end {
            return Vec::new();
        }
        let mut index = self
            .starts
            .partition_point(|&offset| offset <= start)
            .saturating_sub(1);
        let mut result = Vec::with_capacity(end - start);
        for row in start..end {
            while self.starts[index + 1] <= row {
                index += 1;
            }
            let source = &lines[index];
            let spans = match self.wrapped.get(&index) {
                Some(rows) => rows[row - self.starts[index]]
                    .iter()
                    .map(|fragment| {
                        let span = &source.spans[fragment.span];
                        Span::styled(&span.content[fragment.bytes.clone()], span.style)
                    })
                    .collect(),
                None if self.starts[index + 1] - self.starts[index] > 1 => {
                    let offset = row - self.starts[index];
                    let span = &source.spans[0];
                    let range = match self.long_ascii.get(&index) {
                        Some(rows) => rows[offset].clone(),
                        None => {
                            let mut wanted = 0..0;
                            let mut current = 0;
                            wrap_ascii(&span.content, usize::from(self.width), |range| {
                                if current == offset {
                                    wanted = range;
                                }
                                current += 1;
                            });
                            wanted
                        }
                    };
                    vec![Span::styled(&span.content[range], span.style)]
                }
                None => source
                    .spans
                    .iter()
                    .map(|span| Span::styled(span.content.as_ref(), span.style))
                    .collect(),
            };
            result.push(Line {
                spans,
                style: source.style,
                alignment: source.alignment,
            });
        }
        result
    }
}

fn single_ascii<'a>(line: &'a Line<'_>) -> Option<&'a str> {
    let [span] = line.spans.as_slice() else {
        return None;
    };
    let text = span.content.as_ref();
    (text.is_ascii() && text.bytes().all(|b| (b' '..=b'~').contains(&b))).then_some(text)
}

/// Allocation-free specialization of the same trim=false word wrapper for a
/// single printable ASCII span. Ranges are contiguous because every retained
/// symbol is one byte/cell; spaces discarded at wraps remain outside the ranges.
fn wrap_ascii(text: &str, width: usize, mut emit: impl FnMut(Range<usize>)) {
    debug_assert!(width > 0);
    let mut line = 0..0;
    let mut word = 0..0;
    let mut spaces = 0..0;
    let mut previous_word = false;
    let mut emitted = false;
    fn append(to: &mut Range<usize>, part: &Range<usize>) {
        if part.start != part.end {
            if to.start == to.end {
                to.start = part.start;
            }
            to.end = part.end;
        }
    }
    for (i, byte) in text.bytes().enumerate() {
        let whitespace = byte == b' ';
        if (previous_word && whitespace)
            || ((line.start == line.end)
                && (word.end - word.start) + (spaces.end - spaces.start) + 1 > width)
        {
            append(&mut line, &spaces);
            append(&mut line, &word);
            spaces = i..i;
            word = i..i;
        }
        if (line.end - line.start) >= width
            || (line.end - line.start) + (spaces.end - spaces.start) + (word.end - word.start)
                >= width
        {
            let remaining = width.saturating_sub(line.end - line.start);
            emit(line.clone());
            emitted = true;
            line = i..i;
            spaces.start += remaining.min(spaces.end - spaces.start);
            if whitespace && (spaces.start == spaces.end) {
                continue;
            }
        }
        let pending = if whitespace { &mut spaces } else { &mut word };
        if pending.start == pending.end {
            pending.start = i;
        }
        pending.end = i + 1;
        previous_word = !whitespace;
    }
    append(&mut line, &spaces);
    append(&mut line, &word);
    if line.start != line.end || !emitted {
        emit(line);
    }
}

struct Glyph<'a> {
    symbol: &'a str,
    fragment: Fragment,
    whitespace: bool,
}

fn finish_row(glyphs: &mut Vec<Glyph<'_>>, rows: &mut Vec<Vec<Fragment>>, width: u16) {
    let mut used = 0u16;
    let mut fragments: Vec<Fragment> = Vec::new();
    for glyph in glyphs.drain(..) {
        let cells = glyph.symbol.cell_width().min(width);
        if used.saturating_add(cells) > width {
            rows.push(std::mem::take(&mut fragments));
            used = 0;
        }
        used += cells;
        if let Some(last) = fragments.last_mut() {
            if last.span == glyph.fragment.span && last.bytes.end == glyph.fragment.bytes.start {
                last.bytes.end = glyph.fragment.bytes.end;
                continue;
            }
        }
        fragments.push(glyph.fragment);
    }
    rows.push(fragments);
}

fn wrap_line(line: &Line<'_>, width: u16) -> Vec<Vec<Fragment>> {
    let mut rows = Vec::new();
    let mut pending_line = Vec::new();
    let mut pending_word = Vec::new();
    let mut pending_whitespace: VecDeque<Glyph<'_>> = VecDeque::new();
    let mut line_width = 0usize;
    let mut word_width = 0usize;
    let mut whitespace_width = 0usize;
    let mut non_whitespace_previous = false;
    for (span_index, span) in line.spans.iter().enumerate() {
        let mut byte_offset = 0;
        for grapheme in span.styled_graphemes(line.style) {
            // styled_graphemes omits controls. Locate each retained slice after
            // the previous one, so byte ranges preserve that omission as well.
            let start = byte_offset + span.content[byte_offset..].find(grapheme.symbol).unwrap();
            byte_offset = start + grapheme.symbol.len();
            let glyph = Glyph {
                symbol: grapheme.symbol,
                fragment: Fragment {
                    span: span_index,
                    bytes: start..byte_offset,
                },
                whitespace: grapheme.is_whitespace(),
            };
            // A wider-than-viewport glyph retains its source range; rendering
            // shows a replacement cell until a resize can display it fully.
            let symbol_width = usize::from(glyph.symbol.cell_width().min(width));
            let is_whitespace = glyph.whitespace;
            let word_found = non_whitespace_previous && is_whitespace;
            let overflow = pending_line.is_empty()
                && word_width + whitespace_width + symbol_width > usize::from(width);
            if word_found || overflow {
                pending_line.extend(pending_whitespace.drain(..));
                line_width += whitespace_width;
                pending_line.append(&mut pending_word);
                line_width += word_width;
                whitespace_width = 0;
                word_width = 0;
            }
            let line_full = line_width >= usize::from(width);
            let pending_word_overflow = symbol_width > 0
                && line_width + whitespace_width + word_width >= usize::from(width);
            if line_full || pending_word_overflow {
                let mut remaining_width = usize::from(width).saturating_sub(line_width);
                finish_row(&mut pending_line, &mut rows, width);
                line_width = 0;
                while let Some(first) = pending_whitespace.front() {
                    let cell_width = usize::from(first.symbol.cell_width().min(width));
                    if cell_width > remaining_width {
                        break;
                    }
                    whitespace_width -= cell_width;
                    remaining_width -= cell_width;
                    pending_whitespace.pop_front();
                }
                if is_whitespace && pending_whitespace.is_empty() {
                    continue;
                }
            }
            if is_whitespace {
                whitespace_width += symbol_width;
                pending_whitespace.push_back(glyph);
            } else {
                word_width += symbol_width;
                pending_word.push(glyph);
            }
            non_whitespace_previous = !is_whitespace;
        }
    }
    pending_line.extend(pending_whitespace);
    pending_line.append(&mut pending_word);
    if !pending_line.is_empty() || rows.is_empty() {
        finish_row(&mut pending_line, &mut rows, width);
    }
    rows
}

/// Render only indexed rows, with explicit cell bounds. Ratatui's Paragraph
/// renderer can write past a narrow row on some wide-grapheme word boundaries.
/// A glyph wider than the entire viewport is represented visibly, not removed
/// from the source; widening the viewport restores the original glyph.
pub(super) fn render_rows(lines: &[Line<'_>], area: Rect, buffer: &mut Buffer) {
    if area.width == 0 {
        return;
    }
    for (y, line) in lines.iter().take(usize::from(area.height)).enumerate() {
        let used: usize = line
            .styled_graphemes(Style::default())
            .map(|g| usize::from(g.symbol.cell_width().min(area.width)))
            .sum();
        let used = used.min(usize::from(area.width)) as u16;
        let mut x = match line.alignment.unwrap_or(Alignment::Left) {
            Alignment::Left => 0,
            Alignment::Center => (area.width / 2).saturating_sub(used / 2),
            Alignment::Right => area.width.saturating_sub(used),
        };
        for glyph in line.styled_graphemes(Style::default()) {
            let width = glyph.symbol.cell_width();
            if width == 0 {
                continue;
            }
            let (symbol, width) = if width > area.width {
                ("�", 1)
            } else {
                (glyph.symbol, width)
            };
            if x.saturating_add(width) > area.width {
                break;
            }
            buffer[(area.x + x, area.y + y as u16)]
                .set_symbol(symbol)
                .set_style(glyph.style);
            x += width;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        buffer::Buffer,
        widgets::{Paragraph, Widget, Wrap},
    };

    fn compare(lines: &[Line<'static>], width: u16) {
        let layout = DetailLayout::new(lines, width);
        let area = Rect::new(0, 0, width, 7);
        for start in 0..layout.row_count().saturating_add(2).min(70) {
            let mut expected = Buffer::empty(area);
            Paragraph::new(lines.to_vec())
                .wrap(Wrap { trim: false })
                .scroll((start as u16, 0))
                .render(area, &mut expected);
            let mut actual = Buffer::empty(area);
            render_rows(&layout.visible(lines, start, 7), area, &mut actual);
            assert_eq!(
                actual, expected,
                "width={width}, start={start}, lines={lines:?}"
            );
        }
    }

    #[test]
    fn viewport_matches_ratatui_wrapping_unicode_whitespace_and_styles() {
        let lines = vec![
            Line::from(""),
            Line::from("  leading   spaces and trailing   "),
            Line::from("averylongwordwithoutsplittingpoints"),
            Line::from("printable\u{7f}DEL\tcontrol  wraps correctly"),
            Line::from(vec![
                Span::styled("red ée\u{301}", Style::new().fg(Color::Red)),
                Span::styled(" \u{a0}blue é \tcontrol\n", Style::new().bg(Color::Blue)),
            ])
            .style(Style::new().bold()),
            Line::from("centered words wrap too").centered(),
            Line::from("right aligned").right_aligned(),
        ];
        for width in 1..25 {
            compare(&lines, width);
        }
    }

    #[test]
    fn viewport_matches_ratatui_across_deterministic_mixed_text() {
        let symbols = ["a", " ", "ñ", "e\u{301}", "\u{a0}", "é", "  ", "\t", "b"];
        let mut seed = 37u64;
        for _ in 0..80 {
            let mut text = String::new();
            for _ in 0..45 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                text.push_str(symbols[(seed >> 32) as usize % symbols.len()]);
            }
            let lines = vec![Line::from(text)];
            for width in [1, 2, 3, 5, 13] {
                compare(&lines, width);
            }
        }
    }

    #[test]
    fn ascii_count_only_layout_matches_ratatui_with_spaces_and_long_words() {
        let mut seed = 19u64;
        for _ in 0..120 {
            let mut text = String::new();
            for _ in 0..80 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                text.push(if (seed >> 32).is_multiple_of(3) {
                    ' '
                } else {
                    'x'
                });
            }
            let lines = vec![Line::from(text)];
            for width in [1, 2, 3, 7, 17] {
                compare(&lines, width);
            }
        }
        let lines =
            vec![Line::from("long ordinary words without per-row allocations".to_string()); 1000];
        let layout = DetailLayout::new(&lines, 12);
        assert!(layout.wrapped.is_empty());
        assert!(layout.long_ascii.is_empty());
        assert_eq!(layout.starts.len(), lines.len() + 1);
    }

    #[test]
    fn one_long_source_line_can_scroll_beyond_u16_rows() {
        let lines = vec![Line::from(format!("{}Z", "x".repeat(70_000))).red()];
        let layout = DetailLayout::new(&lines, 1);
        assert_eq!(layout.row_count(), 70_001);
        let visible = layout.visible(&lines, 70_000, 1);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].to_string(), "Z");
        assert_eq!(visible[0].style.fg, Some(Color::Red));
        assert!(matches!(
            visible[0].spans[0].content,
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn narrow_wide_graphemes_never_overflow_or_disappear_from_layout() {
        let symbols = ["a", " ", "界", "e\u{301}", "\u{a0}", "👩‍💻", "  ", "\t", "b"];
        let mut seed = 37u64;
        for _ in 0..80 {
            let mut text = String::new();
            for _ in 0..45 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                text.push_str(symbols[(seed >> 32) as usize % symbols.len()]);
            }
            let lines = vec![Line::from(text.clone()).red()];
            for width in [1, 2, 3, 5, 13] {
                let layout = DetailLayout::new(&lines, width);
                let visible = layout.visible(&lines, 0, layout.row_count());
                let retained: String = visible.iter().map(ToString::to_string).collect();
                let content = |s: &str| {
                    s.chars()
                        .filter(|c| !c.is_whitespace() && !c.is_control())
                        .collect::<String>()
                };
                assert_eq!(
                    content(&retained),
                    content(&text),
                    "source glyphs must survive wrapping"
                );
                let area = Rect::new(0, 0, width, visible.len() as u16);
                let mut buffer = Buffer::empty(area);
                render_rows(&visible, area, &mut buffer);
                for y in 0..area.height {
                    for x in 0..width {
                        assert!(x + buffer[(x, y)].symbol().cell_width() <= width);
                    }
                }
            }
        }
        let lines = vec![Line::from("界Z")];
        let small = DetailLayout::new(&lines, 1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 2));
        render_rows(&small.visible(&lines, 0, 2), buffer.area, &mut buffer);
        assert_eq!(buffer[(0, 0)].symbol(), "�");
        assert_eq!(buffer[(0, 1)].symbol(), "Z");
        let wide = DetailLayout::new(&lines, 3);
        assert_eq!(wide.visible(&lines, 0, 1)[0].to_string(), "界Z");
    }

    #[test]
    fn empty_and_zero_width_layouts_are_bounded() {
        let layout = DetailLayout::new(&[], 0);
        assert!(layout.visible(&[], usize::MAX, usize::MAX).is_empty());
        let lines = vec![Line::from("content")];
        let layout = DetailLayout::new(&lines, 0);
        assert_eq!(layout.row_count(), 1);
        assert_eq!(layout.visible(&lines, 0, usize::MAX).len(), 1);
    }
}
