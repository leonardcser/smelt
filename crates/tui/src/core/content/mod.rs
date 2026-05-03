pub(crate) mod context;
pub(crate) mod display;
pub(crate) mod stream_parser;
pub(crate) mod transcript;

pub(crate) use context::LayoutContext;
pub(crate) use display::DisplayBlock;

pub(crate) const SPINNER_FRAMES: &[&str] = &["✿", "❀", "✾", "❁"];
/// Frame duration for every animated spinner in the app. Callers in
/// the status bar, transcript tool previews, and the Lua `smelt.ui.
/// spinner` API all read this so every on-screen spinner stays in
/// lockstep.
pub(crate) const SPINNER_FRAME_MS: u64 = 150;

/// Current spinner frame glyph, sampled from a process-wide monotonic
/// clock. All call sites that animate the same pill should use this
/// helper rather than reimplementing the `elapsed / SPINNER_FRAME_MS
/// % len` modulo so frames stay coherent across the status bar,
/// transcript, and Lua-driven dialogs.
pub(crate) fn spinner_frame_index(elapsed: std::time::Duration) -> usize {
    ((elapsed.as_millis() / SPINNER_FRAME_MS as u128) as usize) % SPINNER_FRAMES.len()
}

/// Time-based current glyph. Uses a process-start epoch so every
/// caller converges on the same frame without threading an explicit
/// `Instant` around.
pub(crate) fn spinner_glyph() -> &'static str {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    SPINNER_FRAMES[spinner_frame_index(epoch.elapsed())]
}

/// A markdown table separator line (e.g. `|---|---|`).
pub(crate) fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && t.chars()
            .all(|c| c == '-' || c == '|' || c == ':' || c == ' ')
}
