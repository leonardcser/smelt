use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    display_path, hash_content, str_arg, FileHashes, Tool, ToolContext, ToolFuture, ToolResult,
};

/// Check whether a file path looks like a Jupyter notebook.
pub fn is_notebook(path: &str) -> bool {
    path.to_lowercase().ends_with(".ipynb")
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// Render a notebook's cells as human-readable text with line numbers.
pub fn read_notebook(path: &str, offset: usize, limit: usize) -> ToolResult {
    let raw = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            return ToolResult {
                content: e.to_string(),
                is_error: true,
            }
        }
    };

    let nb: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            return ToolResult {
                content: format!("failed to parse notebook JSON: {e}"),
                is_error: true,
            }
        }
    };

    let cells = match nb.get("cells").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => {
            return ToolResult {
                content: "notebook has no cells array".into(),
                is_error: false,
            }
        }
    };

    if cells.is_empty() {
        return ToolResult {
            content: "notebook is empty (0 cells)".into(),
            is_error: false,
        };
    }

    let mut lines: Vec<String> = Vec::new();

    for (i, cell) in cells.iter().enumerate() {
        let cell_type = cell
            .get("cell_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let cell_id = cell.get("id").and_then(|v| v.as_str()).unwrap_or("");

        let id_display = if cell_id.is_empty() {
            String::new()
        } else {
            format!(" id={cell_id}")
        };

        lines.push(format!("--- Cell {i} [{cell_type}]{id_display} ---"));

        // Source
        let source = join_string_or_array(cell.get("source"));
        for line in source.lines() {
            lines.push(line.to_string());
        }
        if source.is_empty() {
            lines.push(String::new());
        }

        // Outputs (code cells only)
        if cell_type == "code" {
            if let Some(outputs) = cell.get("outputs").and_then(|o| o.as_array()) {
                for output in outputs {
                    render_output(output, &mut lines);
                }
            }
        }

        lines.push(String::new()); // blank separator
    }

    // Apply offset/limit (1-based offset like read_file)
    let start = (offset.max(1)) - 1;
    if start >= lines.len() {
        return ToolResult {
            content: "offset beyond end of notebook".into(),
            is_error: false,
        };
    }
    let end = (start + limit).min(lines.len());

    let result: String = lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:4}\t{}", start + i + 1, line))
        .collect::<Vec<_>>()
        .join("\n");

    ToolResult {
        content: result,
        is_error: false,
    }
}

fn render_output(output: &Value, lines: &mut Vec<String>) {
    let output_type = output
        .get("output_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match output_type {
        "stream" => {
            let text = join_string_or_array(output.get("text"));
            if !text.is_empty() {
                lines.push("[output]".into());
                for line in text.lines() {
                    lines.push(line.to_string());
                }
            }
        }
        "execute_result" | "display_data" => {
            if let Some(data) = output.get("data") {
                // Prefer text/plain, note image presence
                if let Some(text) = data.get("text/plain") {
                    let t = join_string_or_array(Some(text));
                    if !t.is_empty() {
                        lines.push("[output]".into());
                        for line in t.lines() {
                            lines.push(line.to_string());
                        }
                    }
                }
                if data.get("image/png").is_some() || data.get("image/jpeg").is_some() {
                    lines.push("[image output]".into());
                }
                if let Some(html) = data.get("text/html") {
                    let h = join_string_or_array(Some(html));
                    if !h.is_empty() && data.get("text/plain").is_none() {
                        lines.push("[html output]".into());
                        for line in h.lines() {
                            lines.push(line.to_string());
                        }
                    }
                }
            }
        }
        "error" => {
            let ename = output
                .get("ename")
                .and_then(|v| v.as_str())
                .unwrap_or("Error");
            let evalue = output.get("evalue").and_then(|v| v.as_str()).unwrap_or("");
            lines.push(format!("[error: {ename}: {evalue}]"));
            if let Some(tb) = output.get("traceback").and_then(|v| v.as_array()) {
                for frame in tb {
                    if let Some(s) = frame.as_str() {
                        // Strip ANSI escape codes from traceback
                        let clean = strip_ansi(s);
                        for line in clean.lines() {
                            lines.push(line.to_string());
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Notebook source can be a string or an array of strings.
fn join_string_or_array(val: Option<&Value>) -> String {
    match val {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                // CSI sequence: ESC [ ... <letter>
                Some('[') => {
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                // OSC sequence: ESC ] ... (BEL | ST)
                Some(']') => {
                    chars.next();
                    while let Some(&next) = chars.peek() {
                        if next == '\x07' {
                            chars.next();
                            break;
                        }
                        if next == '\x1b' {
                            chars.next();
                            // Consume the backslash of ST (ESC \)
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        chars.next();
                    }
                }
                // Other escape sequences: skip until letter
                _ => {
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

pub struct NotebookEditTool {
    pub hashes: FileHashes,
}

impl Tool for NotebookEditTool {
    fn name(&self) -> &str {
        "notebook_edit"
    }

    fn description(&self) -> &str {
        "Edit a Jupyter notebook (.ipynb) cell. Supports replacing, inserting, and deleting cells. Identify cells by cell_id or cell_number (0-indexed)."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "notebook_path": {
                    "type": "string",
                    "description": "The absolute path to the Jupyter notebook file"
                },
                "cell_number": {
                    "type": "integer",
                    "description": "The 0-indexed cell number to edit. Used when cell_id is not provided."
                },
                "cell_id": {
                    "type": "string",
                    "description": "The ID of the cell to edit. Takes precedence over cell_number. When inserting, the new cell is placed after this cell (omit to insert at the beginning)."
                },
                "new_source": {
                    "type": "string",
                    "description": "The new source content for the cell. Required for replace and insert."
                },
                "cell_type": {
                    "type": "string",
                    "enum": ["code", "markdown"],
                    "description": "The cell type. Required for insert, defaults to current type for replace."
                },
                "edit_mode": {
                    "type": "string",
                    "enum": ["replace", "insert", "delete"],
                    "description": "The edit operation. Defaults to replace."
                }
            },
            "required": ["notebook_path"]
        })
    }

    fn needs_confirm(&self, args: &HashMap<String, Value>) -> Option<String> {
        Some(display_path(&str_arg(args, "notebook_path")))
    }

    fn execute<'a>(
        &'a self,
        args: HashMap<String, Value>,
        _ctx: &'a ToolContext<'a>,
    ) -> ToolFuture<'a> {
        Box::pin(async move { tokio::task::block_in_place(|| run_edit(&args, &self.hashes)) })
    }
}

fn run_edit(args: &HashMap<String, Value>, hashes: &FileHashes) -> ToolResult {
    let path = str_arg(args, "notebook_path");
    let edit_mode = {
        let m = str_arg(args, "edit_mode");
        if m.is_empty() {
            "replace".to_string()
        } else {
            m
        }
    };
    let new_source = str_arg(args, "new_source");
    let cell_id = str_arg(args, "cell_id");
    let cell_type = str_arg(args, "cell_type");
    let cell_number = args.get("cell_number").and_then(|v| v.as_i64());

    if path.is_empty() {
        return ToolResult {
            content: "notebook_path is required".into(),
            is_error: true,
        };
    }

    if !Path::new(&path).exists() {
        return ToolResult {
            content: format!("file not found: {}", display_path(&path)),
            is_error: true,
        };
    }

    // Validate edit_mode
    if !matches!(edit_mode.as_str(), "replace" | "insert" | "delete") {
        return ToolResult {
            content: format!(
                "invalid edit_mode: {edit_mode} (expected replace, insert, or delete)"
            ),
            is_error: true,
        };
    }

    // new_source required for replace and insert
    if edit_mode != "delete" && new_source.is_empty() {
        return ToolResult {
            content: format!("new_source is required for {edit_mode}"),
            is_error: true,
        };
    }

    // cell_type required for insert
    if edit_mode == "insert" && cell_type.is_empty() {
        return ToolResult {
            content: "cell_type is required when inserting a new cell".into(),
            is_error: true,
        };
    }

    let raw = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            return ToolResult {
                content: e.to_string(),
                is_error: true,
            }
        }
    };

    // Check staleness against stored hash
    if let Ok(map) = hashes.lock() {
        match map.get(&path) {
            None => {
                return ToolResult {
                    content: "You must use read_file before editing. Read the notebook first."
                        .into(),
                    is_error: true,
                };
            }
            Some(&stored_hash) => {
                let current_hash = hash_content(&raw);
                if stored_hash != current_hash {
                    return ToolResult {
                        content: "Notebook has been modified since last read. Use read_file to read the current contents before editing.".into(),
                        is_error: true,
                    };
                }
            }
        }
    }

    let mut nb: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            return ToolResult {
                content: format!("failed to parse notebook JSON: {e}"),
                is_error: true,
            }
        }
    };

    let cells = match nb.get_mut("cells").and_then(|c| c.as_array_mut()) {
        Some(c) => c,
        None => {
            return ToolResult {
                content: "notebook has no cells array".into(),
                is_error: true,
            }
        }
    };

    // Resolve target cell index
    let target_idx = resolve_cell_index(cells, &cell_id, cell_number);

    match edit_mode.as_str() {
        "replace" => {
            let idx = match target_idx {
                Some(i) => i,
                None => {
                    return ToolResult {
                        content: cell_not_found_msg(&cell_id, cell_number, cells.len()),
                        is_error: true,
                    }
                }
            };
            if idx >= cells.len() {
                return ToolResult {
                    content: format!(
                        "cell_number {idx} out of range (notebook has {} cells)",
                        cells.len()
                    ),
                    is_error: true,
                };
            }

            // Convert source to array of lines (notebook convention)
            let source_value = source_to_json(&new_source);
            cells[idx]["source"] = source_value;

            if !cell_type.is_empty() {
                cells[idx]["cell_type"] = Value::String(cell_type.clone());
                // If switching to markdown, remove outputs and execution_count
                if cell_type == "markdown" {
                    if let Some(o) = cells[idx].as_object_mut() {
                        o.remove("outputs");
                        o.remove("execution_count");
                    }
                }
                // If switching to code, ensure outputs/execution_count exist
                if cell_type == "code" {
                    let obj = cells[idx].as_object_mut().unwrap();
                    obj.entry("outputs").or_insert(Value::Array(vec![]));
                    obj.entry("execution_count").or_insert(Value::Null);
                }
            }

            // Clear outputs on replace (stale)
            if cells[idx].get("cell_type").and_then(|v| v.as_str()) == Some("code") {
                cells[idx]["outputs"] = Value::Array(vec![]);
                cells[idx]["execution_count"] = Value::Null;
            }

            write_notebook(&path, &nb, &format!("replaced cell {idx}"), hashes)
        }
        "insert" => {
            // Insert after target_idx, or at beginning if no target specified
            let insert_at = if cell_id.is_empty() && cell_number.is_none() {
                0
            } else {
                match target_idx {
                    Some(i) => {
                        if i >= cells.len() {
                            return ToolResult {
                                content: format!(
                                    "cell_number {i} out of range (notebook has {} cells)",
                                    cells.len()
                                ),
                                is_error: true,
                            };
                        }
                        i + 1
                    }
                    None => {
                        return ToolResult {
                            content: cell_not_found_msg(&cell_id, cell_number, cells.len()),
                            is_error: true,
                        }
                    }
                }
            };

            let new_cell = make_cell(&cell_type, &new_source);
            cells.insert(insert_at, new_cell);

            write_notebook(
                &path,
                &nb,
                &format!("inserted {cell_type} cell at position {insert_at}"),
                hashes,
            )
        }
        "delete" => {
            let idx = match target_idx {
                Some(i) => i,
                None => {
                    return ToolResult {
                        content: cell_not_found_msg(&cell_id, cell_number, cells.len()),
                        is_error: true,
                    }
                }
            };
            if idx >= cells.len() {
                return ToolResult {
                    content: format!(
                        "cell_number {idx} out of range (notebook has {} cells)",
                        cells.len()
                    ),
                    is_error: true,
                };
            }

            cells.remove(idx);

            write_notebook(&path, &nb, &format!("deleted cell {idx}"), hashes)
        }
        _ => unreachable!(),
    }
}

fn resolve_cell_index(cells: &[Value], cell_id: &str, cell_number: Option<i64>) -> Option<usize> {
    // cell_id takes precedence
    if !cell_id.is_empty() {
        return cells
            .iter()
            .position(|c| c.get("id").and_then(|v| v.as_str()) == Some(cell_id));
    }
    cell_number.and_then(|n| if n < 0 { None } else { Some(n as usize) })
}

fn cell_not_found_msg(cell_id: &str, cell_number: Option<i64>, total: usize) -> String {
    if !cell_id.is_empty() {
        format!("cell with id '{cell_id}' not found")
    } else if let Some(n) = cell_number {
        format!("cell_number {n} out of range (notebook has {total} cells)")
    } else {
        "either cell_id or cell_number must be provided".into()
    }
}

/// Convert a source string into the notebook JSON array-of-lines format.
fn source_to_json(source: &str) -> Value {
    let lines: Vec<&str> = source.split('\n').collect();
    let arr: Vec<Value> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            if i < lines.len() - 1 {
                Value::String(format!("{line}\n"))
            } else if line.is_empty() {
                // Last line empty means trailing newline was already captured
                Value::String(String::new())
            } else {
                Value::String((*line).to_string())
            }
        })
        .collect();
    Value::Array(arr)
}

fn make_cell(cell_type: &str, source: &str) -> Value {
    let id = generate_cell_id();
    let source_value = source_to_json(source);

    let mut cell = serde_json::json!({
        "cell_type": cell_type,
        "id": id,
        "metadata": {},
        "source": source_value
    });

    if cell_type == "code" {
        cell["execution_count"] = Value::Null;
        cell["outputs"] = Value::Array(vec![]);
    }

    cell
}

static NEXT_CELL_ID: AtomicU64 = AtomicU64::new(1);

fn generate_cell_id() -> String {
    let id = NEXT_CELL_ID.fetch_add(1, Ordering::Relaxed);
    format!("{:016x}", id)
}

fn write_notebook(path: &str, nb: &Value, action: &str, hashes: &FileHashes) -> ToolResult {
    // 1-space indent matches Jupyter/JupyterLab convention
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    if let Err(e) = nb.serialize(&mut ser) {
        return ToolResult {
            content: format!("failed to serialize notebook: {e}"),
            is_error: true,
        };
    }
    let mut json = match String::from_utf8(buf) {
        Ok(s) => s,
        Err(e) => {
            return ToolResult {
                content: format!("failed to serialize notebook: {e}"),
                is_error: true,
            }
        }
    };

    // Ensure trailing newline
    if !json.ends_with('\n') {
        json.push('\n');
    }

    match std::fs::write(path, &json) {
        Ok(_) => {
            if let Ok(mut map) = hashes.lock() {
                map.insert(path.to_string(), hash_content(&json));
            }
            ToolResult {
                content: format!("{action} in {}", display_path(path)),
                is_error: false,
            }
        }
        Err(e) => ToolResult {
            content: e.to_string(),
            is_error: true,
        },
    }
}
