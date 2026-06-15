#![no_main]

//! ANSI parser/projection fuzz target. Exercises the SGR parser plus the
//! `wrap_ansi` → `emit_ansi_row` handoff, where wrap byte ranges must remain
//! valid UTF-8 boundaries inside each style span.

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use smelt_core::buffer::{BufCreateOpts, BufId, Buffer};
use smelt_core::content::ansi::{emit_ansi_row, wrap_ansi};
use smelt_core::content::builder::render_into;
use smelt_core::theme::Theme;

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    bytes: Vec<u8>,
    width: u8,
}

fuzz_target!(|data: &[u8]| {
    let Ok(input) = FuzzInput::arbitrary(&mut Unstructured::new(data)) else {
        return;
    };

    let text = String::from_utf8_lossy(&input.bytes);
    let render_width = u16::from(input.width % 80);
    let wrap_width = usize::from(render_width);

    let spans = smelt_ansi::parse_ansi(&text);
    for span in &spans {
        assert!(!span.text.chars().any(|ch| ch.is_control() && ch != '\t'));
    }

    let (spans, ranges, boundaries) = wrap_ansi(&text, wrap_width);
    let plain: String = spans.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(boundaries.first().copied(), Some(0));
    assert_eq!(boundaries.last().copied(), Some(plain.len()));
    for &(start, end) in &ranges {
        assert!(start <= end && end <= plain.len());
        assert_eq!(smelt_buffer::text::snap(&plain, start), start);
        assert_eq!(smelt_buffer::text::snap(&plain, end), end);
    }

    let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
    let theme = Theme::default();
    render_into(&mut buf, render_width, &theme, |out| {
        for &(start, end) in &ranges {
            emit_ansi_row(out, &spans, &boundaries, start, end);
            out.newline();
        }
    });
});
