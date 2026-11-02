//! Shared configuration for dev fixtures and benchmark commands.

#[derive(Clone, Copy)]
pub enum LayoutKind {
    Mixed,
    Markdown,
    ToolOutput,
    TinyBlocks,
    HugeBlocks,
}

pub const LAYOUT_WORKLOADS: &[(&str, usize, LayoutKind)] = &[
    ("mixed_10mib", 10 * 1024 * 1024, LayoutKind::Mixed),
    ("mixed_50mib", 50 * 1024 * 1024, LayoutKind::Mixed),
    ("markdown_4mib", 4 * 1024 * 1024, LayoutKind::Markdown),
    ("tool_output_4mib", 4 * 1024 * 1024, LayoutKind::ToolOutput),
    ("tiny_blocks_1mib", 1024 * 1024, LayoutKind::TinyBlocks),
    ("huge_blocks_4mib", 4 * 1024 * 1024, LayoutKind::HugeBlocks),
];

pub const HOT_PATH_OPERATIONS: &[&str] = &[
    "noop_save",
    "request_append",
    "history_appended",
    "turn_complete",
    "rewind_delete_suffix",
    "provider_history_read",
    "provider_history_uncheckpointed_read",
    "engine_request_materialization",
    "submit_enter",
    "submit_first_render",
];

pub fn named_list(
    value: &str,
    allowed: impl IntoIterator<Item = &'static str>,
) -> Result<Vec<&str>, String> {
    let allowed: Vec<_> = allowed.into_iter().collect();
    value
        .split(',')
        .map(str::trim)
        .map(|name| {
            if allowed.contains(&name) {
                Ok(name)
            } else {
                Err(format!(
                    "unknown selection {name:?}; expected one of {}",
                    allowed.join(",")
                ))
            }
        })
        .collect()
}

pub fn scaling_block_counts(value: &str) -> Result<Vec<usize>, String> {
    value
        .split(',')
        .map(|count| {
            count
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|count| *count >= 64)
                .ok_or_else(|| format!("invalid block count {count:?}; expected an integer >= 64"))
        })
        .collect()
}
