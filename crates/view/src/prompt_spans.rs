//! Prompt text spans and styled source token detection.

use smelt_core::attachment::Attachment;
use smelt_text::ATTACHMENT_MARKER;
use smelt_transcript::content::selection::at_token_start;
pub use smelt_transcript::content::selection::{scan_at_token, try_at_ref};

pub const TAB_DISPLAY: &str = "    ";

pub enum Span {
    Plain(String),
    Tab,
    Attachment(String),
    AtRef(String),
    /// A complete, unresolved @token. Quoted tokens can cross line breaks,
    /// so their interior is not a safe incremental parser restart point.
    AtToken(String),
    /// A quoted prefix that could not form a complete token. A later closing
    /// quote can change the parse across subsequent line breaks.
    AtQuoteStart(String),
}

pub fn build_display_spans(buf: &str, attachments: &[Attachment]) -> Vec<Span> {
    let _perf = smelt_perf::perf::begin("render:display_spans");
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut att_idx = 0;

    let chars: Vec<char> = buf.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ATTACHMENT_MARKER {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            let label = attachments
                .get(att_idx)
                .map(Attachment::display_label)
                .unwrap_or_else(|| "[?]".into());
            spans.push(Span::Attachment(label));
            att_idx += 1;
            i += 1;
        } else if chars[i] == '\t' {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            spans.push(Span::Tab);
            i += 1;
        } else if let Some((token, end)) =
            try_at_ref(&chars, i).filter(|(token, _)| !token.contains(ATTACHMENT_MARKER))
        {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            spans.push(Span::AtRef(crate::display_safe_text(&token)));
            i = end;
        } else if let Some((token, _, end)) =
            scan_at_token(&chars, i).filter(|(token, _, _)| !token.contains(ATTACHMENT_MARKER))
        {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            spans.push(Span::AtToken(crate::display_safe_text(&token)));
            i = end;
        } else if at_token_start(&chars, i)
            && chars.get(i + 1) == Some(&'"')
            && chars.get(i + 2) != Some(&'"')
        {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            spans.push(Span::AtQuoteStart("@\"".into()));
            i += 2;
        } else {
            plain.push(crate::display_safe_char(chars[i]));
            i += 1;
        }
    }
    if !plain.is_empty() {
        spans.push(Span::Plain(plain));
    }
    spans
}

pub fn spans_to_string(spans: &[Span]) -> String {
    let mut s = String::new();
    for span in spans {
        match span {
            Span::Plain(t)
            | Span::Attachment(t)
            | Span::AtRef(t)
            | Span::AtToken(t)
            | Span::AtQuoteStart(t) => s.push_str(t),
            Span::Tab => s.push_str(TAB_DISPLAY),
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_tokens_do_not_consume_attachment_markers() {
        let first = Attachment::image("first.png".into(), "data:first".into());
        let second = Attachment::image("second.png".into(), "data:second".into());
        let source = format!("@\"missing{ATTACHMENT_MARKER}\" {ATTACHMENT_MARKER}");
        let spans = build_display_spans(&source, &[first, second]);
        assert_eq!(
            spans_to_string(&spans),
            "@\"missing[first.png]\" [second.png]"
        );
    }

    #[test]
    fn display_spans_sanitize_plain_controls_but_keep_newlines() {
        let spans = build_display_spans("a\0\tb\nc", &[]);
        assert_eq!(spans_to_string(&spans), "a\u{FFFD}    b\nc");
    }
}
