use smelt_core::transcript_model::{ToolState, TranscriptBlockDescriptor};

pub(crate) fn descriptor_search_text(
    descriptor: &TranscriptBlockDescriptor,
    tool_state: Option<&ToolState>,
) -> String {
    let mut text = tool_state
        .and_then(|state| state.output.as_ref())
        .map(|output| output.content.clone())
        .or_else(|| descriptor.raw_text())
        .unwrap_or_default();
    append_search_line(&mut text, thinking_summary(descriptor).as_deref());
    append_search_line(&mut text, compacted_label(descriptor));
    append_search_line(&mut text, compacted_separator(descriptor));
    append_search_line(
        &mut text,
        edit_file_search_text(descriptor, tool_state).as_deref(),
    );
    if let Some(display_count) = tool_state.and_then(display_count_search_text) {
        append_search_line(&mut text, Some(&display_count));
    }
    text
}

fn append_search_line(out: &mut String, text: Option<&str>) {
    let Some(text) = text.filter(|text| !text.is_empty()) else {
        return;
    };
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(text);
}

fn thinking_summary(descriptor: &TranscriptBlockDescriptor) -> Option<String> {
    let TranscriptBlockDescriptor::Thinking { content } = descriptor else {
        return None;
    };
    let (label, line_count) = thinking_summary_label(content);
    let collapsed_lines = if label == "thinking" {
        line_count
    } else {
        line_count.saturating_sub(1)
    };
    Some(format!(
        "{label}\n… {} …",
        pluralize(collapsed_lines, "line collapsed", "lines collapsed")
    ))
}

fn thinking_summary_label(content: &str) -> (String, usize) {
    let mut label = None;
    let mut lines = 0usize;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        lines += 1;
        if label.is_none()
            && trimmed.starts_with("**")
            && trimmed.ends_with("**")
            && trimmed.len() > 4
        {
            label = trimmed
                .strip_prefix("**")
                .and_then(|inner| inner.strip_suffix("**"))
                .map(str::trim)
                .filter(|inner| !inner.is_empty())
                .map(str::to_string);
        }
    }
    (label.unwrap_or_else(|| "thinking".to_string()), lines)
}

fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

fn compacted_label(descriptor: &TranscriptBlockDescriptor) -> Option<&'static str> {
    matches!(descriptor, TranscriptBlockDescriptor::Compacted { .. }).then_some("compacted")
}

fn compacted_separator(descriptor: &TranscriptBlockDescriptor) -> Option<&'static str> {
    matches!(descriptor, TranscriptBlockDescriptor::Compacted { .. }).then_some("─")
}

fn edit_file_search_text(
    descriptor: &TranscriptBlockDescriptor,
    tool_state: Option<&ToolState>,
) -> Option<String> {
    let args = edit_file_args(descriptor)?;
    let mut text = String::new();
    let old_string = string_field(args, "old_string").unwrap_or_default();
    let new_string = string_field(args, "new_string").unwrap_or_default();
    append_search_line(
        &mut text,
        Some(&replacement_line_detail(old_string, new_string)),
    );
    append_search_line(&mut text, string_field(args, "file_path"));

    let metadata = tool_state
        .and_then(|state| state.output.as_ref())
        .and_then(|output| output.metadata.as_ref())
        .and_then(serde_json::Value::as_object);
    let has_snapshot = metadata.is_some_and(|metadata| {
        let old_content = metadata
            .get("old_content")
            .and_then(serde_json::Value::as_str);
        let new_content = metadata
            .get("new_content")
            .and_then(serde_json::Value::as_str);
        append_search_line(&mut text, old_content);
        append_search_line(&mut text, new_content);
        old_content.is_some() || new_content.is_some()
    });
    if !has_snapshot {
        append_search_line(&mut text, Some(old_string));
        append_search_line(&mut text, Some(new_string));
    }
    (!text.is_empty()).then_some(text)
}

fn edit_file_args(
    descriptor: &TranscriptBlockDescriptor,
) -> Option<&std::collections::HashMap<String, serde_json::Value>> {
    match descriptor {
        TranscriptBlockDescriptor::ToolDraft { name, args, .. }
        | TranscriptBlockDescriptor::ToolCall { name, args, .. }
            if name == "edit_file" =>
        {
            Some(args)
        }
        _ => None,
    }
}

fn string_field<'a>(
    fields: &'a std::collections::HashMap<String, serde_json::Value>,
    key: &str,
) -> Option<&'a str> {
    fields.get(key).and_then(serde_json::Value::as_str)
}

fn replacement_line_detail(old_text: &str, new_text: &str) -> String {
    format!(
        "{}, {}",
        line_label(line_count(old_text), "old line"),
        line_label(line_count(new_text), "new line")
    )
}

fn line_count(text: &str) -> usize {
    text.lines().count()
}

fn line_label(count: usize, label: &str) -> String {
    format!("{} {}{}", count, label, if count == 1 { "" } else { "s" })
}

fn display_count_search_text(state: &ToolState) -> Option<String> {
    let metadata = state.output.as_ref()?.metadata.as_ref()?;
    let display_count = metadata.get("display_count")?.as_object()?;
    let (count, count_text) = display_count_count_text(display_count.get("value"));
    let unit = display_count
        .get("unit")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("item");
    let plural = display_count
        .get("plural")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            if unit == "match" {
                "matches".to_string()
            } else {
                format!("{unit}s")
            }
        });
    let label = if count == 1.0 { unit } else { &plural };
    Some(format!("{count_text} {label}"))
}

fn display_count_count_text(value: Option<&serde_json::Value>) -> (f64, String) {
    let count = value.and_then(display_count_count).unwrap_or(0.0);
    (count, format_lua_number(count))
}

fn display_count_count(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(text) => text.parse::<f64>().ok(),
        _ => None,
    }
    .filter(|value| value.is_finite())
}

fn format_lua_number(value: f64) -> String {
    if value.fract() == 0.0 {
        (value as i64).to_string()
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::transcript_model::{ToolOutput, ToolStatus};

    fn tool_state_with_content(content: &str, metadata: serde_json::Value) -> ToolState {
        ToolState {
            status: ToolStatus::Ok,
            elapsed: None,
            output: Some(Box::new(ToolOutput {
                content: content.to_string(),
                is_error: false,
                metadata: Some(metadata),
            })),
            user_message: None,
            preview_output: None,
        }
    }

    fn tool_state(metadata: serde_json::Value) -> ToolState {
        tool_state_with_content("output", metadata)
    }

    fn edit_file_descriptor(old_string: &str, new_string: &str) -> TranscriptBlockDescriptor {
        let mut args = std::collections::HashMap::new();
        args.insert(
            "file_path".to_string(),
            serde_json::Value::String("/tmp/example.rs".to_string()),
        );
        args.insert(
            "old_string".to_string(),
            serde_json::Value::String(old_string.to_string()),
        );
        args.insert(
            "new_string".to_string(),
            serde_json::Value::String(new_string.to_string()),
        );
        TranscriptBlockDescriptor::ToolCall {
            call_id: "call-1".to_string(),
            name: "edit_file".to_string(),
            summary: protocol::StyledLines::from_plain("example.rs"),
            args,
        }
    }

    #[test]
    fn display_count_text_matches_default_lua_plural_rules() {
        let state = tool_state(serde_json::json!({
            "display_count": { "value": 2, "unit": "match" }
        }));
        assert_eq!(
            display_count_search_text(&state).as_deref(),
            Some("2 matches")
        );

        let state = tool_state(serde_json::json!({
            "display_count": { "value": 1, "unit": "file", "plural": "files" }
        }));
        assert_eq!(display_count_search_text(&state).as_deref(), Some("1 file"));
    }

    #[test]
    fn descriptor_search_text_includes_thinking_summary_chrome() {
        let descriptor = TranscriptBlockDescriptor::Thinking {
            content: "**Analyzing the bug**\n\nChecking files\nReviewing output".to_string(),
        };
        assert_eq!(
            descriptor_search_text(&descriptor, None),
            "**Analyzing the bug**\n\nChecking files\nReviewing output\nAnalyzing the bug\n… 2 lines collapsed …"
        );

        let descriptor = TranscriptBlockDescriptor::Thinking {
            content: "Checking files\nReviewing output".to_string(),
        };
        assert_eq!(
            descriptor_search_text(&descriptor, None),
            "Checking files\nReviewing output\nthinking\n… 2 lines collapsed …"
        );
    }

    #[test]
    fn descriptor_search_text_includes_default_compacted_chrome() {
        let descriptor = TranscriptBlockDescriptor::Compacted {
            summary: "archived".to_string(),
        };
        assert_eq!(
            descriptor_search_text(&descriptor, None),
            "archived\ncompacted\n─"
        );
    }

    #[test]
    fn descriptor_search_text_includes_edit_file_snapshot_metadata() {
        let descriptor = edit_file_descriptor("old needle", "new needle");
        let state = tool_state_with_content(
            "edited example.rs",
            serde_json::json!({
                "path": "/tmp/example.rs",
                "old_content": "fn old_snapshot() {}\n",
                "new_content": "fn new_snapshot() {}\n",
            }),
        );

        assert_eq!(
            descriptor_search_text(&descriptor, Some(&state)),
            "edited example.rs\n1 old line, 1 new line\n/tmp/example.rs\nfn old_snapshot() {}\nfn new_snapshot() {}\n"
        );
    }

    #[test]
    fn descriptor_search_text_includes_edit_file_planned_strings_without_snapshot() {
        let descriptor = edit_file_descriptor("alpha\nbeta", "gamma");

        assert_eq!(
            descriptor_search_text(&descriptor, None),
            "2 old lines, 1 new line\n/tmp/example.rs\nalpha\nbeta\ngamma"
        );
    }
}
