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

    fn tool_state(metadata: serde_json::Value) -> ToolState {
        ToolState {
            status: ToolStatus::Ok,
            elapsed: None,
            output: Some(Box::new(ToolOutput {
                content: "output".to_string(),
                is_error: false,
                metadata: Some(metadata),
            })),
            user_message: None,
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
}
