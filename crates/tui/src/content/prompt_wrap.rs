//! Byte-level source ↔ wrapped-display translation for the prompt.
//!
//! Composes two maps: source ↔ display (attachment expansion) and display ↔
//! joined-wrapped-rows (soft-wrap `\n` inserts). Used by mouse routing to
//! translate `state.cpos` (source bytes) ↔ `Window::cpos` (rows-joined bytes).

use crate::content::selection::{
    build_char_kinds, build_display_spans, spans_to_string, wrap_and_locate_cursor, Span,
};
use crate::input::{PromptState, ATTACHMENT_MARKER};

pub(crate) struct PromptWrap {
    pub(crate) rows: Vec<String>,
    pub(crate) joined: String,
    /// Byte positions in `joined` where the `\n` is a soft-wrap insert.
    pub(crate) soft_breaks: Vec<usize>,
    /// Byte positions in `joined` where the `\n` is a real source hard-break.
    pub(crate) hard_breaks: Vec<usize>,

    src_len: usize,
    src_to_wrapped_byte: Vec<usize>,
    wrapped_to_src_byte: Vec<usize>,
}

impl PromptWrap {
    pub(crate) fn build(state: &PromptState, usable: usize) -> Self {
        let src_buf = &state.win.text;
        let spans = build_display_spans(src_buf, &state.win.attachment_ids, &state.store);
        let display_buf = spans_to_string(&spans);
        let char_kinds = build_char_kinds(&spans);

        let (src_to_disp, disp_to_src) = build_src_disp_byte_maps(&spans, src_buf, &display_buf);

        let (visual_lines, _, _, _) = wrap_and_locate_cursor(&display_buf, &char_kinds, 0, usable);
        let rows: Vec<String> = visual_lines.iter().map(|(s, _)| s.clone()).collect();
        let joined = rows.join("\n");

        let mut line_starts_disp: Vec<usize> = Vec::with_capacity(rows.len());
        {
            let mut cursor = 0usize;
            for row in &rows {
                line_starts_disp.push(cursor);
                cursor += row.len();
                if cursor < display_buf.len() && display_buf.as_bytes()[cursor] == b'\n' {
                    cursor += 1;
                }
            }
        }

        let mut disp_to_joined = vec![0usize; display_buf.len() + 1];
        let mut joined_to_disp = vec![0usize; joined.len() + 1];
        let mut sum_prev_bytes = 0usize;
        let mut soft_breaks: Vec<usize> = Vec::new();
        let mut hard_breaks: Vec<usize> = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            let line_start = line_starts_disp[i];
            for off in 0..row.len() {
                let d = line_start + off;
                let j = sum_prev_bytes + i + off;
                disp_to_joined[d] = j;
                joined_to_disp[j] = d;
            }
            sum_prev_bytes += row.len();
            if i + 1 < rows.len() {
                let join_j = sum_prev_bytes + i;
                let next_disp = line_starts_disp[i + 1];
                let row_end_disp = line_start + row.len();
                let is_hard = next_disp > row_end_disp;
                if is_hard {
                    hard_breaks.push(join_j);
                    disp_to_joined[row_end_disp] = join_j;
                    joined_to_disp[join_j] = row_end_disp;
                } else {
                    soft_breaks.push(join_j);
                    joined_to_disp[join_j] = row_end_disp;
                }
            }
        }
        disp_to_joined[display_buf.len()] = joined.len();
        joined_to_disp[joined.len()] = display_buf.len();

        let src_to_wrapped_byte: Vec<usize> = src_to_disp
            .iter()
            .map(|&d| disp_to_joined[d.min(display_buf.len())])
            .collect();
        let wrapped_to_src_byte: Vec<usize> = joined_to_disp
            .iter()
            .map(|&d| disp_to_src[d.min(display_buf.len())])
            .collect();

        Self {
            rows,
            joined,
            soft_breaks,
            hard_breaks,
            src_len: src_buf.len(),
            src_to_wrapped_byte,
            wrapped_to_src_byte,
        }
    }

    pub(crate) fn src_to_wrapped(&self, src_byte: usize) -> usize {
        let i = src_byte.min(self.src_len);
        self.src_to_wrapped_byte[i]
    }

    pub(crate) fn wrapped_to_src(&self, w_byte: usize) -> usize {
        let i = w_byte.min(self.joined.len());
        self.wrapped_to_src_byte[i]
    }
}

/// Build forward/reverse byte maps: source ↔ display (attachment labels expand 1 marker → N bytes).
fn build_src_disp_byte_maps(spans: &[Span], src: &str, display: &str) -> (Vec<usize>, Vec<usize>) {
    let mut s2d = vec![0usize; src.len() + 1];
    let mut d2s = vec![0usize; display.len() + 1];

    let mut s_byte = 0usize;
    let mut d_byte = 0usize;
    for span in spans {
        match span {
            Span::Plain(t) | Span::AtRef(t) => {
                let n = t.len();
                for k in 0..n {
                    s2d[s_byte + k] = d_byte + k;
                    d2s[d_byte + k] = s_byte + k;
                }
                s_byte += n;
                d_byte += n;
            }
            Span::Attachment(label) => {
                let marker_len = ATTACHMENT_MARKER.len_utf8();
                let label_len = label.len();
                for k in 0..marker_len {
                    s2d[s_byte + k] = d_byte;
                }
                for k in 0..label_len {
                    d2s[d_byte + k] = s_byte;
                }
                s_byte += marker_len;
                d_byte += label_len;
            }
        }
    }
    s2d[src.len()] = display.len();
    d2s[display.len()] = src.len();
    (s2d, d2s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::PromptState;

    #[test]
    fn translates_plain_buffer_identity() {
        let mut state = PromptState::new();
        state.win.text = "hello world".to_string();
        let w = PromptWrap::build(&state, 80);
        assert_eq!(w.rows, vec!["hello world".to_string()]);
        assert_eq!(w.src_to_wrapped(0), 0);
        assert_eq!(w.src_to_wrapped(5), 5);
        assert_eq!(w.src_to_wrapped(11), 11);
        assert_eq!(w.wrapped_to_src(0), 0);
        assert_eq!(w.wrapped_to_src(11), 11);
    }

    #[test]
    fn translates_hard_break() {
        let mut state = PromptState::new();
        state.win.text = "abc\ndef".to_string();
        let w = PromptWrap::build(&state, 80);
        assert_eq!(w.rows, vec!["abc".to_string(), "def".to_string()]);
        assert_eq!(w.joined, "abc\ndef");
        assert_eq!(w.hard_breaks, vec![3]);
        assert!(w.soft_breaks.is_empty());
        assert_eq!(w.src_to_wrapped(4), 4);
        assert_eq!(w.wrapped_to_src(4), 4);
    }

    #[test]
    fn translates_soft_break() {
        let mut state = PromptState::new();
        state.win.text = "hello world foo".to_string();
        let w = PromptWrap::build(&state, 8);
        assert!(!w.soft_breaks.is_empty());
        assert!(w.hard_breaks.is_empty());
        let wrapped_w = w.src_to_wrapped(6);
        assert_eq!(w.wrapped_to_src(wrapped_w), 6);
    }
}
