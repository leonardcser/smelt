//! Notebook capability — Jupyter `.ipynb` JSON helpers. Pure parse +
//! introspection surface, no I/O. Exposed to Lua via
//! `crates/tui/src/lua/api/notebook.rs::{parse, ...}` and composed by
//! tools that need to understand notebook structure.
//!
//! Apply-edit / atomic write semantics (the `edit_notebook` tool's
//! 600 LOC of cell mutation, source-string concatenation, and id
//! generation) migrate here when `notebook_edit` moves to Lua in
//! P5.b. Today this module ships the read shapes a plugin needs:
//! parse JSON → typed `Notebook { cells, metadata }`, pull a cell's
//! plain-text source, locate a cell by id.

use serde_json::Value;

/// File-extension probe (case-insensitive).
pub fn is_notebook_path(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".ipynb")
}

/// Cell types Jupyter recognises. Anything unfamiliar surfaces as
/// `Other(_)` so callers don't lose information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellKind {
    Code,
    Markdown,
    Raw,
    Other(String),
}

impl CellKind {
    pub fn as_str(&self) -> &str {
        match self {
            CellKind::Code => "code",
            CellKind::Markdown => "markdown",
            CellKind::Raw => "raw",
            CellKind::Other(s) => s.as_str(),
        }
    }

    fn from_str(s: &str) -> CellKind {
        match s {
            "code" => CellKind::Code,
            "markdown" => CellKind::Markdown,
            "raw" => CellKind::Raw,
            other => CellKind::Other(other.to_string()),
        }
    }
}

/// One notebook cell, normalised into a plain `source` string.
#[derive(Debug, Clone)]
pub struct Cell {
    pub kind: CellKind,
    pub id: Option<String>,
    pub source: String,
    pub execution_count: Option<i64>,
}

/// Parsed notebook view. `format_*` mirror nbformat's top-level keys;
/// `cells` walks `cells[]` in order.
#[derive(Debug, Clone)]
pub struct Notebook {
    pub format: Option<i64>,
    pub format_minor: Option<i64>,
    pub cells: Vec<Cell>,
}

/// Parse `.ipynb` JSON. Errors surface as `serde_json::Error`.
pub fn parse(json: &str) -> Result<Notebook, serde_json::Error> {
    let raw: Value = serde_json::from_str(json)?;
    let format = raw.get("nbformat").and_then(|v| v.as_i64());
    let format_minor = raw.get("nbformat_minor").and_then(|v| v.as_i64());

    let mut cells = Vec::new();
    if let Some(arr) = raw.get("cells").and_then(|v| v.as_array()) {
        for cell in arr {
            cells.push(parse_cell(cell));
        }
    }

    Ok(Notebook {
        format,
        format_minor,
        cells,
    })
}

fn parse_cell(cell: &Value) -> Cell {
    let kind = cell
        .get("cell_type")
        .and_then(|v| v.as_str())
        .map(CellKind::from_str)
        .unwrap_or(CellKind::Other("unknown".into()));
    let id = cell.get("id").and_then(|v| v.as_str()).map(String::from);
    let execution_count = cell.get("execution_count").and_then(|v| v.as_i64());
    let source = source_to_string(cell.get("source"));

    Cell {
        kind,
        id,
        source,
        execution_count,
    }
}

/// Jupyter stores `source` as either a single string or an array of
/// strings. Concatenate and return the canonical text.
fn source_to_string(source: Option<&Value>) -> String {
    match source {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Find the index of a cell by id. `None` when no match.
#[cfg(test)]
fn cell_index_by_id(nb: &Notebook, id: &str) -> Option<usize> {
    nb.cells.iter().position(|c| c.id.as_deref() == Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"{
        "nbformat": 4,
        "nbformat_minor": 5,
        "metadata": { "kernelspec": { "name": "python3" } },
        "cells": [
            {
                "cell_type": "markdown",
                "id": "intro",
                "source": ["# title\n", "hello"]
            },
            {
                "cell_type": "code",
                "id": "c1",
                "execution_count": 2,
                "source": "print('hi')",
                "outputs": []
            }
        ]
    }"##;

    #[test]
    fn parse_extracts_format_and_cells() {
        let nb = parse(SAMPLE).unwrap();
        assert_eq!(nb.format, Some(4));
        assert_eq!(nb.format_minor, Some(5));
        assert_eq!(nb.cells.len(), 2);
    }

    #[test]
    fn cells_normalize_source_array_to_string() {
        let nb = parse(SAMPLE).unwrap();
        assert_eq!(nb.cells[0].kind, CellKind::Markdown);
        assert_eq!(nb.cells[0].source, "# title\nhello");
        assert_eq!(nb.cells[1].kind, CellKind::Code);
        assert_eq!(nb.cells[1].source, "print('hi')");
        assert_eq!(nb.cells[1].execution_count, Some(2));
    }

    #[test]
    fn cell_index_by_id_finds_cells() {
        let nb = parse(SAMPLE).unwrap();
        assert_eq!(cell_index_by_id(&nb, "intro"), Some(0));
        assert_eq!(cell_index_by_id(&nb, "c1"), Some(1));
        assert_eq!(cell_index_by_id(&nb, "missing"), None);
    }

    #[test]
    fn is_notebook_path_matches_extension() {
        assert!(is_notebook_path("foo.ipynb"));
        assert!(is_notebook_path("FOO.IPYNB"));
        assert!(!is_notebook_path("foo.py"));
        assert!(!is_notebook_path("foo"));
    }

    #[test]
    fn parse_errors_on_bad_json() {
        assert!(parse("{ not json").is_err());
    }
}

// ── Notebook editing / rendering (migrated from engine/tools/notebook.rs) ───

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::fs::{staleness_error, FileStateCache};
use crate::tools::{display_path, str_arg};

#[derive(Debug, Clone)]
struct NotebookCellSnapshot {
    index: usize,
    cell_type: String,
    cell_id: Option<String>,
    source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotebookRenderData {
    pub edit_mode: String,
    pub path: String,
    pub index: usize,
    pub old_type: Option<String>,
    pub new_type: Option<String>,
    pub cell_id: Option<String>,
    pub old_source: String,
    pub new_source: String,
}

impl NotebookRenderData {
    /// Return the file extension for syntax highlighting based on the cell type.
    pub fn syntax_ext(&self) -> &str {
        let cell_type = self.new_type.as_deref().or(self.old_type.as_deref());
        match cell_type {
            Some("markdown") => "md",
            _ => "py",
        }
    }

    pub fn title(&self) -> String {
        let kind = match (self.old_type.as_deref(), self.new_type.as_deref()) {
            (Some(old), Some(new)) if old != new => format!("{old} → {new}"),
            (_, Some(new)) => new.to_string(),
            (Some(old), None) => old.to_string(),
            _ => "cell".into(),
        };
        let mut title = format!("{} cell {} [{}]", self.edit_mode, self.index, kind);
        if let Some(id) = self.cell_id.as_deref() {
            title.push_str(&format!(" id={id}"));
        }
        title
    }
}

/// Build preview data for an `edit_notebook` call. Returns `None`
/// when the notebook can't be read, parsed, or the targeted cell is
/// out of bounds; callers can then leave the preview pane blank.
pub fn preview_render_data(args: &HashMap<String, Value>) -> Option<NotebookRenderData> {
    let path = args
        .get("notebook_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let raw = std::fs::read_to_string(path).ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    let cells = parsed.get("cells").and_then(|c| c.as_array())?;

    let edit_mode = args
        .get("edit_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("replace");
    let cell_id = args.get("cell_id").and_then(|v| v.as_str()).unwrap_or("");
    let cell_number = args.get("cell_number").and_then(|v| v.as_i64());
    let new_source = args
        .get("new_source")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let requested_type = args
        .get("cell_type")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let target_idx = resolve_cell_index(cells, cell_id, cell_number);

    match edit_mode {
        "insert" => {
            let insert_at = if cell_id.is_empty() && cell_number.is_none() {
                0
            } else {
                match target_idx {
                    Some(i) if i < cells.len() => i + 1,
                    _ => return None,
                }
            };
            Some(NotebookRenderData {
                edit_mode: "insert".into(),
                path: path.into(),
                index: insert_at,
                old_type: None,
                new_type: requested_type,
                cell_id: None,
                old_source: String::new(),
                new_source,
            })
        }
        "delete" => {
            let idx = match target_idx {
                Some(i) if i < cells.len() => i,
                _ => return None,
            };
            let cell = &cells[idx];
            Some(NotebookRenderData {
                edit_mode: "delete".into(),
                path: path.into(),
                index: idx,
                old_type: cell
                    .get("cell_type")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                new_type: None,
                cell_id: cell.get("id").and_then(|v| v.as_str()).map(str::to_string),
                old_source: join_string_or_array(cell.get("source")),
                new_source: String::new(),
            })
        }
        _ => {
            let idx = match target_idx {
                Some(i) if i < cells.len() => i,
                _ => return None,
            };
            let cell = &cells[idx];
            Some(NotebookRenderData {
                edit_mode: "replace".into(),
                path: path.into(),
                index: idx,
                old_type: cell
                    .get("cell_type")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                new_type: requested_type.or_else(|| {
                    cell.get("cell_type")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                }),
                cell_id: cell.get("id").and_then(|v| v.as_str()).map(str::to_string),
                old_source: join_string_or_array(cell.get("source")),
                new_source,
            })
        }
    }
}

/// Render a notebook's cells as human-readable text with line numbers.
/// Public wrapper for Lua tools that need the same line-numbered
/// formatted-cell output the engine `read_file` tool produces for
/// `.ipynb` paths.
pub fn render_notebook_text(path: &str, offset: usize, limit: usize) -> Result<String, String> {
    let r = read_notebook(path, offset, limit);
    if r.is_error {
        Err(r.content)
    } else {
        Ok(r.content)
    }
}

/// Local result type used by internal notebook helpers.
pub(crate) struct NbResult {
    content: String,
    is_error: bool,
    metadata: Option<Value>,
}

impl NbResult {
    fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            metadata: None,
        }
    }
    fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            metadata: None,
        }
    }
    fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// Render a notebook's cells as human-readable text with line numbers.
pub(crate) fn read_notebook(path: &str, offset: usize, limit: usize) -> NbResult {
    let raw = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => return NbResult::err(e.to_string()),
    };

    let nb: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => return NbResult::err(format!("failed to parse notebook JSON: {e}")),
    };

    let cells = match nb.get("cells").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => return NbResult::ok("notebook has no cells array"),
    };

    if cells.is_empty() {
        return NbResult::ok("notebook is empty (0 cells)");
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
        return NbResult::ok("offset beyond end of notebook");
    }
    let end = (start + limit).min(lines.len());

    let result: String = lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:4}\t{}", start + i + 1, line))
        .collect::<Vec<_>>()
        .join("\n");

    NbResult::ok(result)
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
                Some('[') => {
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(&next) = chars.peek() {
                        if next == '\x07' {
                            chars.next();
                            break;
                        }
                        if next == '\x1b' {
                            chars.next();
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        chars.next();
                    }
                }
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

fn render_data_metadata(data: &NotebookRenderData) -> serde_json::Value {
    serde_json::json!({
        "kind": "notebook_cell_edit",
        "edit_mode": data.edit_mode,
        "path": data.path,
        "index": data.index,
        "old_type": data.old_type,
        "new_type": data.new_type,
        "cell_id": data.cell_id,
        "old_source": data.old_source,
        "new_source": data.new_source,
    })
}

fn cell_snapshot(cell: &Value, index: usize) -> NotebookCellSnapshot {
    NotebookCellSnapshot {
        index,
        cell_type: cell
            .get("cell_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        cell_id: cell.get("id").and_then(|v| v.as_str()).map(str::to_string),
        source: join_string_or_array(cell.get("source")),
    }
}

fn render_data_from_snapshots(
    edit_mode: &str,
    path: &str,
    old: Option<&NotebookCellSnapshot>,
    new: Option<&NotebookCellSnapshot>,
) -> NotebookRenderData {
    let index = new
        .map(|c| c.index)
        .or_else(|| old.map(|c| c.index))
        .unwrap_or(0);
    NotebookRenderData {
        edit_mode: edit_mode.to_string(),
        path: path.to_string(),
        index,
        old_type: old.map(|c| c.cell_type.clone()),
        new_type: new.map(|c| c.cell_type.clone()),
        cell_id: new
            .and_then(|c| c.cell_id.clone())
            .or_else(|| old.and_then(|c| c.cell_id.clone())),
        old_source: old.map(|c| c.source.clone()).unwrap_or_default(),
        new_source: new.map(|c| c.source.clone()).unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

/// Result of a successful `apply_edit`. Carries the human-readable
/// confirmation message plus the dialog metadata payload.
pub struct NotebookEditOutcome {
    pub message: String,
    pub metadata: Value,
}

/// Public entry-point for the Lua `edit_notebook` tool. Performs the
/// JSON cell munging, writes the notebook with one-space indent
/// (matching Jupyter convention), and records the new content in the
/// shared file-state cache. The caller holds the per-path advisory
/// lock for the duration of the call.
pub fn apply_edit(
    args: &HashMap<String, Value>,
    files: &FileStateCache,
) -> Result<NotebookEditOutcome, String> {
    let r = run_edit(args, files);
    if r.is_error {
        Err(r.content)
    } else {
        Ok(NotebookEditOutcome {
            message: r.content,
            metadata: r.metadata.unwrap_or(Value::Null),
        })
    }
}

fn run_edit(args: &HashMap<String, Value>, files: &FileStateCache) -> NbResult {
    let path = str_arg(args, "notebook_path");

    if path.is_empty() {
        return NbResult::err("notebook_path is required");
    }

    if !Path::new(&path).exists() {
        return NbResult::err(format!("file not found: {}", display_path(&path)));
    }

    // Acquire cross-process advisory lock (non-blocking).
    let _flock = match crate::fs::try_flock(&path) {
        Ok(guard) => Some(guard),
        Err(e) => return NbResult::err(e),
    };

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

    // Validate edit_mode
    if !matches!(edit_mode.as_str(), "replace" | "insert" | "delete") {
        return NbResult::err(format!(
            "invalid edit_mode: {edit_mode} (expected replace, insert, or delete)"
        ));
    }

    // new_source required for replace and insert
    if edit_mode != "delete" && new_source.is_empty() {
        return NbResult::err(format!("new_source is required for {edit_mode}"));
    }

    // cell_type required for insert
    if edit_mode == "insert" && cell_type.is_empty() {
        return NbResult::err("cell_type is required when inserting a new cell");
    }

    if let Some(err) = staleness_error(files, &path, "notebook") {
        return NbResult::err(err);
    }

    let raw = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return NbResult::err(e.to_string()),
    };

    let mut nb: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => return NbResult::err(format!("failed to parse notebook JSON: {e}")),
    };

    let cells = match nb.get_mut("cells").and_then(|c| c.as_array_mut()) {
        Some(c) => c,
        None => return NbResult::err("notebook has no cells array"),
    };

    // Resolve target cell index
    let target_idx = resolve_cell_index(cells, &cell_id, cell_number);

    match edit_mode.as_str() {
        "replace" => {
            let idx = match target_idx {
                Some(i) => i,
                None => {
                    return NbResult::err(cell_not_found_msg(&cell_id, cell_number, cells.len()))
                }
            };
            if idx >= cells.len() {
                return NbResult::err(format!(
                    "cell_number {idx} out of range (notebook has {} cells)",
                    cells.len()
                ));
            }

            let old_cell = cell_snapshot(&cells[idx], idx);

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

            let new_cell = cell_snapshot(&cells[idx], idx);
            let render =
                render_data_from_snapshots("replace", &path, Some(&old_cell), Some(&new_cell));
            write_notebook(
                &path,
                &nb,
                &format!("replaced cell {idx}"),
                files,
                Some(render),
            )
        }
        "insert" => {
            // Insert after target_idx, or at beginning if no target specified
            let insert_at = if cell_id.is_empty() && cell_number.is_none() {
                0
            } else {
                match target_idx {
                    Some(i) => {
                        if i >= cells.len() {
                            return NbResult::err(format!(
                                "cell_number {i} out of range (notebook has {} cells)",
                                cells.len()
                            ));
                        }
                        i + 1
                    }
                    None => {
                        return NbResult::err(cell_not_found_msg(
                            &cell_id,
                            cell_number,
                            cells.len(),
                        ))
                    }
                }
            };

            let new_cell = make_cell(&cell_type, &new_source);
            cells.insert(insert_at, new_cell);
            let inserted = cell_snapshot(&cells[insert_at], insert_at);
            let render = render_data_from_snapshots("insert", &path, None, Some(&inserted));

            write_notebook(
                &path,
                &nb,
                &format!("inserted {cell_type} cell at position {insert_at}"),
                files,
                Some(render),
            )
        }
        "delete" => {
            let idx = match target_idx {
                Some(i) => i,
                None => {
                    return NbResult::err(cell_not_found_msg(&cell_id, cell_number, cells.len()))
                }
            };
            if idx >= cells.len() {
                return NbResult::err(format!(
                    "cell_number {idx} out of range (notebook has {} cells)",
                    cells.len()
                ));
            }

            let deleted = cell_snapshot(&cells[idx], idx);
            cells.remove(idx);
            let render = render_data_from_snapshots("delete", &path, Some(&deleted), None);

            write_notebook(
                &path,
                &nb,
                &format!("deleted cell {idx}"),
                files,
                Some(render),
            )
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

fn write_notebook(
    path: &str,
    nb: &Value,
    action: &str,
    files: &FileStateCache,
    render: Option<NotebookRenderData>,
) -> NbResult {
    // 1-space indent matches Jupyter/JupyterLab convention
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    if let Err(e) = nb.serialize(&mut ser) {
        return NbResult::err(format!("failed to serialize notebook: {e}"));
    }
    let mut json = match String::from_utf8(buf) {
        Ok(s) => s,
        Err(e) => return NbResult::err(format!("failed to serialize notebook: {e}")),
    };

    if !json.ends_with('\n') {
        json.push('\n');
    }

    match std::fs::write(path, &json) {
        Ok(_) => {
            files.record_write(path, json);
            if let Some(render) = render {
                NbResult::ok(format!("{action} in {}", display_path(path)))
                    .with_metadata(render_data_metadata(&render))
            } else {
                NbResult::ok(format!("{action} in {}", display_path(path)))
            }
        }
        Err(e) => NbResult::err(e.to_string()),
    }
}
