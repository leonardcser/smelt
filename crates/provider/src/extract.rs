use protocol::{FunctionCall, ToolCall};

/// Extract `<tool_call>` blocks from text. Some backends (vLLM) embed tool call
/// markup in `content` or `reasoning_content` instead of the `tool_calls` field.
/// Returns parsed calls and the cleaned text; cleaned is `None` if empty.
pub fn extract_tool_calls_from_text(text: Option<&str>) -> (Vec<ToolCall>, Option<String>) {
    let Some(text) = text else {
        return (vec![], None);
    };

    let mut calls = Vec::new();
    let mut cleaned = String::with_capacity(text.len());
    let mut rest = text;
    let mut idx = 0;

    while let Some(open) = rest.find("<tool_call>") {
        cleaned.push_str(&rest[..open]);
        let after_open = &rest[open + "<tool_call>".len()..];

        if let Some(close) = after_open.find("</tool_call>") {
            let raw = after_open[..close].trim();
            if let Some(tc) = parse_tool_call_json(raw, &mut idx) {
                calls.push(tc);
            }
            rest = &after_open[close + "</tool_call>".len()..];
        } else {
            let raw = after_open.trim();
            if let Some(tc) = parse_tool_call_json(raw, &mut idx) {
                calls.push(tc);
            }
            rest = "";
            break;
        }
    }
    cleaned.push_str(rest);

    let cleaned = cleaned.trim().to_string();
    let cleaned = if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    };

    (calls, cleaned)
}

fn parse_tool_call_json(raw: &str, idx: &mut usize) -> Option<ToolCall> {
    if let Some(tc) = parse_tool_call_json_inner(raw, idx) {
        return Some(tc);
    }
    if let Some(tc) = parse_tool_call_xml(raw, idx) {
        return Some(tc);
    }
    parse_tool_call_arg_kv(raw, idx)
}

fn parse_tool_call_json_inner(raw: &str, idx: &mut usize) -> Option<ToolCall> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let name = v["name"].as_str()?;
    let arguments = match &v["arguments"] {
        serde_json::Value::Null => return None,
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let id = format!("fallback-{idx}");
    *idx += 1;
    Some(ToolCall::new(
        id,
        FunctionCall {
            name: name.to_string(),
            arguments,
        },
    ))
}

fn parse_tool_call_xml(raw: &str, idx: &mut usize) -> Option<ToolCall> {
    let func_start = raw.find("<function=")?;
    let after_eq = &raw[func_start + "<function=".len()..];
    let name_end = after_eq.find('>')?;
    let name = after_eq[..name_end].trim().to_string();
    if name.is_empty() {
        return None;
    }

    let mut params = serde_json::Map::new();
    let mut rest = &after_eq[name_end + 1..];
    while let Some(param_start) = rest.find("<parameter=") {
        let after_param_eq = &rest[param_start + "<parameter=".len()..];
        let key_end = after_param_eq.find('>')?;
        let key = after_param_eq[..key_end].trim();
        let value_start = &after_param_eq[key_end + 1..];
        let value_end = value_start.find("</parameter>")?;
        let value = value_start[..value_end].to_string();
        let value = value.strip_prefix('\n').unwrap_or(&value).to_string();
        let value = value.strip_suffix('\n').unwrap_or(&value).to_string();
        params.insert(key.to_string(), serde_json::Value::String(value));
        rest = &value_start[value_end + "</parameter>".len()..];
    }

    if params.is_empty() {
        return None;
    }

    let arguments = serde_json::Value::Object(params).to_string();
    let id = format!("fallback-{idx}");
    *idx += 1;
    Some(ToolCall::new(id, FunctionCall { name, arguments }))
}

/// Parse `<function>name</function><arg_key>k</arg_key><arg_value>v</arg_value>` format.
/// The `thought` key is stripped.
fn parse_tool_call_arg_kv(raw: &str, idx: &mut usize) -> Option<ToolCall> {
    let func_start = raw.find("<function>")?;
    let after_tag = &raw[func_start + "<function>".len()..];
    let func_end = after_tag.find("</function>")?;
    let name = after_tag[..func_end].trim().to_string();
    if name.is_empty() {
        return None;
    }

    let mut params = serde_json::Map::new();
    let mut rest = &after_tag[func_end + "</function>".len()..];
    while let Some(key_start) = rest.find("<arg_key>") {
        let after_key_tag = &rest[key_start + "<arg_key>".len()..];
        let key_end = after_key_tag.find("</arg_key>")?;
        let key = after_key_tag[..key_end].trim().to_string();
        rest = &after_key_tag[key_end + "</arg_key>".len()..];

        let val_start = rest.find("<arg_value>")?;
        let after_val_tag = &rest[val_start + "<arg_value>".len()..];
        let val_end = after_val_tag.find("</arg_value>")?;
        let value = after_val_tag[..val_end].to_string();
        let value = value.strip_prefix('\n').unwrap_or(&value).to_string();
        let value = value.strip_suffix('\n').unwrap_or(&value).to_string();
        rest = &after_val_tag[val_end + "</arg_value>".len()..];

        if key != "thought" {
            params.insert(key, serde_json::Value::String(value));
        }
    }

    if params.is_empty() {
        return None;
    }

    let arguments = serde_json::Value::Object(params).to_string();
    let id = format!("fallback-{idx}");
    *idx += 1;
    Some(ToolCall::new(id, FunctionCall { name, arguments }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_tool_calls_from_content() {
        let text = r#"Let me search for that.
<tool_call>
{"name": "search", "arguments": {"query": "rust async"}}
</tool_call>"#;

        let (calls, cleaned) = extract_tool_calls_from_text(Some(text));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "search");
        assert_eq!(calls[0].function.arguments, r#"{"query":"rust async"}"#);
        assert_eq!(cleaned.unwrap(), "Let me search for that.");
    }

    #[test]
    fn extract_tool_calls_from_reasoning() {
        let text = r#"I need to call the tool
<tool_call>
{"name": "bash", "arguments": {"command": "ls"}}
</tool_call>
</think>"#;

        let (calls, cleaned) = extract_tool_calls_from_text(Some(text));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "bash");
        assert_eq!(cleaned.unwrap(), "I need to call the tool\n\n</think>");
    }

    #[test]
    fn extract_multiple_tool_calls() {
        let text = r#"<tool_call>
{"name": "read", "arguments": {"path": "a.rs"}}
</tool_call>
<tool_call>
{"name": "read", "arguments": {"path": "b.rs"}}
</tool_call>"#;

        let (calls, cleaned) = extract_tool_calls_from_text(Some(text));
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "read");
        assert_eq!(calls[1].function.name, "read");
        assert_eq!(calls[0].id, "fallback-0");
        assert_eq!(calls[1].id, "fallback-1");
        assert!(cleaned.is_none() || cleaned.as_deref() == Some(""));
    }

    #[test]
    fn no_tool_calls_passthrough() {
        let text = "Just regular content";
        let (calls, cleaned) = extract_tool_calls_from_text(Some(text));
        assert!(calls.is_empty());
        assert_eq!(cleaned.unwrap(), "Just regular content");
    }

    #[test]
    fn none_input() {
        let (calls, cleaned) = extract_tool_calls_from_text(None);
        assert!(calls.is_empty());
        assert!(cleaned.is_none());
    }

    #[test]
    fn extract_xml_format_tool_call() {
        let text = r#"
<tool_call>
<function=replace_in_file>
<parameter=file_path>
/foo.rs
</parameter>
<parameter=old_string>
hello
</parameter>
<parameter=new_string>
world
</parameter>
</function>
</tool_call>"#;

        let (calls, cleaned) = extract_tool_calls_from_text(Some(text));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "replace_in_file");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["file_path"], "/foo.rs");
        assert_eq!(args["old_string"], "hello");
        assert_eq!(args["new_string"], "world");
        assert!(cleaned.is_none());
    }

    #[test]
    fn extract_arg_key_value_format_tool_call() {
        let text = r#"<tool_call>
<function>search_files</function>
<arg_key>query</arg_key>
<arg_value>
needle
</arg_value>
<arg_key>thought</arg_key>
<arg_value>ignore me</arg_value>
</tool_call>"#;

        let (calls, _) = extract_tool_calls_from_text(Some(text));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "search_files");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["query"], "needle");
        assert!(args.get("thought").is_none());
    }
}
