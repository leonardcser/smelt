//! Prompt text spans and styled source token detection.

use crate::input::ATTACHMENT_MARKER;
use smelt_core::attachment::{AttachmentId, AttachmentStore};
pub(crate) use smelt_core::content::selection::{scan_at_token, try_at_ref};

pub(crate) enum Span {
    Plain(String),
    Attachment(String),
    AtRef(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum SpanKind {
    Plain,
    Attachment,
    AtRef,
}

pub(crate) fn build_char_kinds(spans: &[Span]) -> Vec<SpanKind> {
    let mut kinds = Vec::new();
    for span in spans {
        let (text, kind) = match span {
            Span::Plain(t) => (t.as_str(), SpanKind::Plain),
            Span::Attachment(t) => (t.as_str(), SpanKind::Attachment),
            Span::AtRef(t) => (t.as_str(), SpanKind::AtRef),
        };
        kinds.extend(std::iter::repeat_n(kind, text.chars().count()));
    }
    kinds
}

pub(crate) fn build_display_spans(
    buf: &str,
    att_ids: &[AttachmentId],
    store: &AttachmentStore,
) -> Vec<Span> {
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
            let label = att_ids
                .get(att_idx)
                .map(|&id| store.display_label(id))
                .unwrap_or_else(|| "[?]".into());
            spans.push(Span::Attachment(label));
            att_idx += 1;
            i += 1;
        } else if let Some((token, end)) = try_at_ref(&chars, i) {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            spans.push(Span::AtRef(token));
            i = end;
        } else if let Some((token, _, end)) = scan_at_token(&chars, i) {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            spans.push(Span::Plain(token));
            i = end;
        } else {
            plain.push(chars[i]);
            i += 1;
        }
    }
    if !plain.is_empty() {
        spans.push(Span::Plain(plain));
    }
    spans
}

pub(crate) fn spans_to_string(spans: &[Span]) -> String {
    let mut s = String::new();
    for span in spans {
        match span {
            Span::Plain(t) | Span::Attachment(t) | Span::AtRef(t) => s.push_str(t),
        }
    }
    s
}
