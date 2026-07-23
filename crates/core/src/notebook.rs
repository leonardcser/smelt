//! Jupyter `.ipynb` helpers - pure JSON parse + editing, no I/O beyond
//! the `apply_edit` write path.

use serde_json::Value;

pub fn is_notebook_path(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".ipynb")
}

/// Unfamiliar cell types surface as `Other(_)` so callers don't lose information.
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

#[derive(Debug, Clone)]
pub struct Cell {
    pub kind: CellKind,
    pub id: Option<String>,
    pub source: String,
    pub execution_count: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct Notebook {
    pub format: Option<i64>,
    pub format_minor: Option<i64>,
    pub cells: Vec<Cell>,
}

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

/// `source` in `.ipynb` is a string or array of strings; return the concatenation.
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

    #[test]
    fn parse_handles_string_source() {
        let json = r#"{"cells":[{"cell_type":"code","source":"x = 1"}]}"#;
        let nb = parse(json).unwrap();
        assert_eq!(nb.cells[0].source, "x = 1");
    }

    #[test]
    fn parse_handles_missing_cells_array() {
        let nb = parse(r#"{"nbformat":4}"#).unwrap();
        assert!(nb.cells.is_empty());
        assert_eq!(nb.format, Some(4));
    }

    #[test]
    fn parse_handles_missing_nbformat() {
        let nb = parse(r#"{"cells":[]}"#).unwrap();
        assert_eq!(nb.format, None);
        assert_eq!(nb.format_minor, None);
    }

    #[test]
    fn parse_treats_unknown_cell_type_as_other_variant() {
        let json = r#"{"cells":[{"cell_type":"weird","source":""}]}"#;
        let nb = parse(json).unwrap();
        assert_eq!(nb.cells[0].kind, CellKind::Other("weird".into()));
    }

    #[test]
    fn parse_handles_cell_without_id() {
        let json = r#"{"cells":[{"cell_type":"code","source":""}]}"#;
        let nb = parse(json).unwrap();
        assert_eq!(nb.cells[0].id, None);
    }

    #[test]
    fn parse_cell_with_missing_cell_type_falls_back_to_other_unknown() {
        let json = r#"{"cells":[{"source":""}]}"#;
        let nb = parse(json).unwrap();
        assert_eq!(nb.cells[0].kind, CellKind::Other("unknown".into()));
    }

    #[test]
    fn cellkind_as_str_roundtrips_known_variants() {
        for s in ["code", "markdown", "raw"] {
            assert_eq!(CellKind::from_str(s).as_str(), s);
        }
    }

    #[test]
    fn cellkind_other_preserves_original_string() {
        assert_eq!(CellKind::Other("xyz".into()).as_str(), "xyz");
    }
}

// ── Notebook editing / rendering ─────────────────────────────────────────────

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::fs::{staleness_error, FileStateCache};
use crate::tools::str_arg;

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

/// Build preview data for an `edit_notebook` call. Returns `None` when the
/// notebook can't be read/parsed or the target cell is out of bounds.
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

/// Render notebook cells as line-numbered human-readable text (same format as `read_file` for `.ipynb`).
pub fn render_notebook_text(path: &str, offset: usize, limit: usize) -> Result<String, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    render_notebook_text_from_raw(&raw, offset, limit)
}

pub fn render_notebook_text_from_raw(
    raw: &str,
    offset: usize,
    limit: usize,
) -> Result<String, String> {
    let r = render_notebook_raw(raw, offset, limit);
    if r.is_error {
        Err(r.content)
    } else {
        Ok(r.content)
    }
}

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

#[cfg(test)]
pub(crate) fn read_notebook(path: &str, offset: usize, limit: usize) -> NbResult {
    let raw = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => return NbResult::err(e.to_string()),
    };

    render_notebook_raw(&raw, offset, limit)
}

fn render_notebook_raw(raw: &str, offset: usize, limit: usize) -> NbResult {
    let nb: Value = match serde_json::from_str(raw) {
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

        let source = join_string_or_array(cell.get("source"));
        for line in source.lines() {
            lines.push(line.to_string());
        }
        if source.is_empty() {
            lines.push(String::new());
        }

        if cell_type == "code" {
            if let Some(outputs) = cell.get("outputs").and_then(|o| o.as_array()) {
                for output in outputs {
                    render_output(output, &mut lines);
                }
            }
        }

        lines.push(String::new());
    }

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
        "syntax_ext": data.syntax_ext(),
        "title": data.title(),
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

#[derive(Debug)]
pub struct NotebookEditOutcome {
    pub message: String,
    pub metadata: Value,
}

pub fn apply_edit(
    args: &HashMap<String, Value>,
    files: &FileStateCache,
) -> Result<NotebookEditOutcome, String> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let home = engine::paths::home_dir();
    apply_edit_with_roots(args, files, &cwd, &home)
}

pub fn apply_edit_with_roots(
    args: &HashMap<String, Value>,
    files: &FileStateCache,
    cwd: &Path,
    home: &Path,
) -> Result<NotebookEditOutcome, String> {
    let r = run_edit(args, files, cwd, home);
    if r.is_error {
        Err(r.content)
    } else {
        Ok(NotebookEditOutcome {
            message: r.content,
            metadata: r.metadata.unwrap_or(Value::Null),
        })
    }
}

fn run_edit(
    args: &HashMap<String, Value>,
    files: &FileStateCache,
    cwd: &Path,
    home: &Path,
) -> NbResult {
    let path = str_arg(args, "notebook_path");

    if path.is_empty() {
        return NbResult::err("notebook_path is required");
    }

    if !Path::new(&path).exists() {
        return NbResult::err(format!(
            "file not found: {}",
            crate::path_display::display_path_from(&path, cwd, home)
        ));
    }

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

    if !matches!(edit_mode.as_str(), "replace" | "insert" | "delete") {
        return NbResult::err(format!(
            "invalid edit_mode: {edit_mode} (expected replace, insert, or delete)"
        ));
    }

    if edit_mode != "delete" && new_source.is_empty() {
        return NbResult::err(format!("new_source is required for {edit_mode}"));
    }

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

            let source_value = source_to_json(&new_source);
            cells[idx]["source"] = source_value;

            if !cell_type.is_empty() {
                cells[idx]["cell_type"] = Value::String(cell_type.clone());
                if cell_type == "markdown" {
                    if let Some(o) = cells[idx].as_object_mut() {
                        o.remove("outputs");
                        o.remove("execution_count");
                    }
                }
                if cell_type == "code" {
                    let obj = cells[idx].as_object_mut().unwrap();
                    obj.entry("outputs").or_insert(Value::Array(vec![]));
                    obj.entry("execution_count").or_insert(Value::Null);
                }
            }

            // Clear stale outputs on replace.
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
                cwd,
                home,
                Some(render),
            )
        }
        "insert" => {
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
                cwd,
                home,
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
                cwd,
                home,
                Some(render),
            )
        }
        _ => unreachable!(),
    }
}

fn resolve_cell_index(cells: &[Value], cell_id: &str, cell_number: Option<i64>) -> Option<usize> {
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

/// Convert a source string to the notebook JSON array-of-lines format.
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

#[cfg(test)]
mod edit_tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    // ── Render data ─────────────────────────────────────────────────

    fn render_data(old: Option<&str>, new: Option<&str>) -> NotebookRenderData {
        NotebookRenderData {
            edit_mode: "replace".into(),
            path: "p.ipynb".into(),
            index: 0,
            old_type: old.map(String::from),
            new_type: new.map(String::from),
            cell_id: None,
            old_source: String::new(),
            new_source: String::new(),
        }
    }

    #[test]
    fn syntax_ext_returns_md_for_markdown_cell() {
        let d = render_data(None, Some("markdown"));
        assert_eq!(d.syntax_ext(), "md");
    }

    #[test]
    fn syntax_ext_returns_py_for_code_cell() {
        let d = render_data(None, Some("code"));
        assert_eq!(d.syntax_ext(), "py");
    }

    #[test]
    fn syntax_ext_uses_old_type_when_new_is_none() {
        let d = render_data(Some("markdown"), None);
        assert_eq!(d.syntax_ext(), "md");
    }

    #[test]
    fn syntax_ext_defaults_to_py_when_no_type() {
        let d = render_data(None, None);
        assert_eq!(d.syntax_ext(), "py");
    }

    #[test]
    fn title_uses_arrow_for_type_change() {
        let d = render_data(Some("markdown"), Some("code"));
        assert!(d.title().contains("markdown → code"));
    }

    #[test]
    fn title_uses_single_type_when_no_change() {
        let d = render_data(Some("code"), Some("code"));
        assert!(d.title().contains("[code]"));
        assert!(!d.title().contains("→"));
    }

    #[test]
    fn title_appends_cell_id_when_present() {
        let mut d = render_data(None, Some("code"));
        d.cell_id = Some("abc123".into());
        assert!(d.title().contains("id=abc123"));
    }

    #[test]
    fn title_falls_back_to_cell_label_when_no_types() {
        let d = render_data(None, None);
        assert!(d.title().contains("[cell]"));
    }

    // ── resolve_cell_index ─────────────────────────────────────────

    fn cells_with_ids(ids: &[&str]) -> Vec<Value> {
        ids.iter()
            .map(|id| json!({ "cell_type": "code", "id": id, "source": "" }))
            .collect()
    }

    #[test]
    fn resolve_by_id_returns_position() {
        let cells = cells_with_ids(&["a", "b", "c"]);
        assert_eq!(resolve_cell_index(&cells, "b", None), Some(1));
    }

    #[test]
    fn resolve_by_id_returns_none_for_missing() {
        let cells = cells_with_ids(&["a", "b"]);
        assert_eq!(resolve_cell_index(&cells, "z", None), None);
    }

    #[test]
    fn resolve_by_number_returns_usize() {
        let cells = cells_with_ids(&["a", "b", "c"]);
        assert_eq!(resolve_cell_index(&cells, "", Some(2)), Some(2));
    }

    #[test]
    fn resolve_negative_cell_number_returns_none() {
        let cells = cells_with_ids(&["a"]);
        assert_eq!(resolve_cell_index(&cells, "", Some(-1)), None);
    }

    #[test]
    fn resolve_id_takes_precedence_over_number() {
        let cells = cells_with_ids(&["a", "b", "c"]);
        // id "c" is at index 2; cell_number 0 should be ignored.
        assert_eq!(resolve_cell_index(&cells, "c", Some(0)), Some(2));
    }

    #[test]
    fn resolve_returns_none_when_neither_provided() {
        let cells = cells_with_ids(&["a"]);
        assert_eq!(resolve_cell_index(&cells, "", None), None);
    }

    // ── source_to_json ──────────────────────────────────────────────

    fn arr(v: &Value) -> Vec<&str> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect()
    }

    #[test]
    fn source_to_json_single_line_has_no_trailing_newline() {
        let v = source_to_json("x = 1");
        assert_eq!(arr(&v), vec!["x = 1"]);
    }

    #[test]
    fn source_to_json_multiline_appends_newline_to_each_but_last() {
        let v = source_to_json("a\nb\nc");
        assert_eq!(arr(&v), vec!["a\n", "b\n", "c"]);
    }

    #[test]
    fn source_to_json_trailing_newline_yields_empty_last_element() {
        let v = source_to_json("a\nb\n");
        assert_eq!(arr(&v), vec!["a\n", "b\n", ""]);
    }

    #[test]
    fn source_to_json_empty_string_yields_single_empty() {
        let v = source_to_json("");
        assert_eq!(arr(&v), vec![""]);
    }

    // ── strip_ansi ──────────────────────────────────────────────────

    #[test]
    fn strip_ansi_removes_csi_escape() {
        assert_eq!(strip_ansi("a\x1b[31mred\x1b[0mb"), "aredb");
    }

    #[test]
    fn strip_ansi_removes_osc_with_bell_terminator() {
        assert_eq!(strip_ansi("pre\x1b]0;title\x07post"), "prepost");
    }

    #[test]
    fn strip_ansi_removes_osc_with_st_terminator() {
        assert_eq!(strip_ansi("pre\x1b]0;title\x1b\\post"), "prepost");
    }

    #[test]
    fn strip_ansi_preserves_plain_text() {
        assert_eq!(strip_ansi("hello, world"), "hello, world");
    }

    #[test]
    fn strip_ansi_handles_bare_esc_without_bracket() {
        // Unknown escape - consume up to next alpha char.
        assert_eq!(strip_ansi("a\x1bMb"), "ab");
    }

    // ── render_output ───────────────────────────────────────────────

    fn collect_lines(output: &Value) -> Vec<String> {
        let mut lines = Vec::new();
        render_output(output, &mut lines);
        lines
    }

    #[test]
    fn render_output_stream_prefixes_output_marker() {
        let out = json!({"output_type": "stream", "text": "hello\nworld"});
        let lines = collect_lines(&out);
        assert_eq!(lines, vec!["[output]", "hello", "world"]);
    }

    #[test]
    fn render_output_stream_skips_when_empty() {
        let out = json!({"output_type": "stream", "text": ""});
        assert!(collect_lines(&out).is_empty());
    }

    #[test]
    fn render_output_execute_result_text_plain() {
        let out = json!({
            "output_type": "execute_result",
            "data": { "text/plain": "42" }
        });
        let lines = collect_lines(&out);
        assert_eq!(lines, vec!["[output]", "42"]);
    }

    #[test]
    fn render_output_image_png_marker() {
        let out = json!({
            "output_type": "display_data",
            "data": { "image/png": "<base64>" }
        });
        let lines = collect_lines(&out);
        assert!(lines.iter().any(|l| l == "[image output]"));
    }

    #[test]
    fn render_output_html_only_when_no_text_plain() {
        let with_text = json!({
            "output_type": "display_data",
            "data": { "text/plain": "p", "text/html": "<b>x</b>" }
        });
        assert!(!collect_lines(&with_text)
            .iter()
            .any(|l| l == "[html output]"));

        let html_only = json!({
            "output_type": "display_data",
            "data": { "text/html": "<b>x</b>" }
        });
        assert!(collect_lines(&html_only)
            .iter()
            .any(|l| l == "[html output]"));
    }

    #[test]
    fn render_output_error_includes_ename_and_evalue() {
        let out = json!({
            "output_type": "error",
            "ename": "ValueError",
            "evalue": "bad input"
        });
        let lines = collect_lines(&out);
        assert!(lines[0].contains("ValueError"));
        assert!(lines[0].contains("bad input"));
    }

    #[test]
    fn render_output_error_traceback_strips_ansi() {
        let out = json!({
            "output_type": "error",
            "ename": "E",
            "evalue": "",
            "traceback": ["\x1b[31mline1\x1b[0m\nline2"]
        });
        let lines = collect_lines(&out);
        assert!(lines.iter().any(|l| l == "line1"));
        assert!(lines.iter().any(|l| l == "line2"));
        assert!(!lines.iter().any(|l| l.contains('\x1b')));
    }

    // ── Disk-backed: read_notebook ──────────────────────────────────

    fn write_nb(dir: &std::path::Path, name: &str, json: Value) -> String {
        let path = dir.join(name);
        std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn sample_nb() -> Value {
        json!({
            "nbformat": 4,
            "nbformat_minor": 5,
            "cells": [
                {"cell_type": "markdown", "id": "intro", "source": "# Title"},
                {"cell_type": "code", "id": "c1", "source": "print('hi')", "outputs": []}
            ]
        })
    }

    #[test]
    fn read_notebook_renders_cell_markers() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", sample_nb());
        let r = read_notebook(&path, 1, 100);
        assert!(!r.is_error);
        assert!(r.content.contains("Cell 0 [markdown]"));
        assert!(r.content.contains("# Title"));
        assert!(r.content.contains("Cell 1 [code]"));
        assert!(r.content.contains("print('hi')"));
    }

    #[test]
    fn read_notebook_offset_limit_windows_lines() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", sample_nb());
        let full = read_notebook(&path, 1, 100).content;
        let total_lines = full.lines().count();
        assert!(total_lines >= 4);
        let middle = read_notebook(&path, 2, 1).content;
        assert_eq!(middle.lines().count(), 1);
    }

    #[test]
    fn read_notebook_error_on_missing_file() {
        let r = read_notebook("/nonexistent/path.ipynb", 1, 10);
        assert!(r.is_error);
    }

    #[test]
    fn read_notebook_error_on_invalid_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.ipynb");
        std::fs::write(&path, "{ not json").unwrap();
        let r = read_notebook(path.to_str().unwrap(), 1, 10);
        assert!(r.is_error);
        assert!(r.content.contains("failed to parse"));
    }

    #[test]
    fn read_notebook_ok_with_no_cells_array() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", json!({"nbformat": 4}));
        let r = read_notebook(&path, 1, 10);
        assert!(!r.is_error);
        assert!(r.content.contains("no cells"));
    }

    #[test]
    fn read_notebook_ok_with_empty_cells() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", json!({"nbformat": 4, "cells": []}));
        let r = read_notebook(&path, 1, 10);
        assert!(!r.is_error);
        assert!(r.content.contains("empty"));
    }

    #[test]
    fn read_notebook_offset_beyond_end_returns_marker_message() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", sample_nb());
        let r = read_notebook(&path, 9999, 10);
        assert!(!r.is_error);
        assert!(r.content.contains("offset beyond end"));
    }

    // ── preview_render_data ─────────────────────────────────────────

    fn args_for(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn preview_replace_returns_render_data_for_target_cell() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", sample_nb());
        let r = preview_render_data(&args_for(&[
            ("notebook_path", json!(path)),
            ("edit_mode", json!("replace")),
            ("cell_id", json!("c1")),
            ("new_source", json!("print('bye')")),
        ]))
        .unwrap();
        assert_eq!(r.edit_mode, "replace");
        assert_eq!(r.index, 1);
        assert_eq!(r.old_source, "print('hi')");
        assert_eq!(r.new_source, "print('bye')");
        assert_eq!(r.old_type.as_deref(), Some("code"));
    }

    #[test]
    fn preview_insert_with_no_target_inserts_at_position_zero() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", sample_nb());
        let r = preview_render_data(&args_for(&[
            ("notebook_path", json!(path)),
            ("edit_mode", json!("insert")),
            ("cell_type", json!("code")),
            ("new_source", json!("x=1")),
        ]))
        .unwrap();
        assert_eq!(r.index, 0);
        assert_eq!(r.edit_mode, "insert");
    }

    #[test]
    fn preview_insert_after_target_uses_target_plus_one() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", sample_nb());
        let r = preview_render_data(&args_for(&[
            ("notebook_path", json!(path)),
            ("edit_mode", json!("insert")),
            ("cell_id", json!("intro")),
            ("cell_type", json!("code")),
            ("new_source", json!("x=1")),
        ]))
        .unwrap();
        assert_eq!(r.index, 1);
    }

    #[test]
    fn preview_delete_returns_old_cell_data() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", sample_nb());
        let r = preview_render_data(&args_for(&[
            ("notebook_path", json!(path)),
            ("edit_mode", json!("delete")),
            ("cell_id", json!("c1")),
        ]))
        .unwrap();
        assert_eq!(r.edit_mode, "delete");
        assert_eq!(r.old_source, "print('hi')");
        assert_eq!(r.new_source, "");
    }

    #[test]
    fn preview_missing_file_returns_none() {
        let r = preview_render_data(&args_for(&[
            ("notebook_path", json!("/nonexistent.ipynb")),
            ("edit_mode", json!("replace")),
            ("cell_id", json!("x")),
            ("new_source", json!("y")),
        ]));
        assert!(r.is_none());
    }

    // ── apply_edit ──────────────────────────────────────────────────

    fn prime_cache(files: &FileStateCache, path: &str) {
        let content = std::fs::read_to_string(path).unwrap();
        files.record_read(path, content, (1, 1000));
    }

    fn parse_file(path: &str) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn apply_replace_updates_source_in_target_cell() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", sample_nb());
        let files = FileStateCache::new();
        prime_cache(&files, &path);

        apply_edit(
            &args_for(&[
                ("notebook_path", json!(path)),
                ("edit_mode", json!("replace")),
                ("cell_id", json!("c1")),
                ("new_source", json!("print('bye')")),
            ]),
            &files,
        )
        .unwrap();

        let nb = parse_file(&path);
        let cells = nb.get("cells").unwrap().as_array().unwrap();
        let source = join_string_or_array(cells[1].get("source"));
        assert_eq!(source, "print('bye')");
    }

    #[test]
    fn apply_succeeds_when_caller_holds_flock() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", sample_nb());
        let files = FileStateCache::new();
        prime_cache(&files, &path);
        let _lock = crate::fs::try_flock(&path).unwrap();

        apply_edit(
            &args_for(&[
                ("notebook_path", json!(path)),
                ("edit_mode", json!("replace")),
                ("cell_id", json!("c1")),
                ("new_source", json!("print('bye')")),
            ]),
            &files,
        )
        .unwrap();

        let nb = parse_file(&path);
        let cells = nb.get("cells").unwrap().as_array().unwrap();
        assert_eq!(join_string_or_array(cells[1].get("source")), "print('bye')");
    }

    #[test]
    fn apply_insert_adds_new_cell() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", sample_nb());
        let files = FileStateCache::new();
        prime_cache(&files, &path);

        apply_edit(
            &args_for(&[
                ("notebook_path", json!(path)),
                ("edit_mode", json!("insert")),
                ("cell_id", json!("intro")),
                ("cell_type", json!("code")),
                ("new_source", json!("z=2")),
            ]),
            &files,
        )
        .unwrap();

        let nb = parse_file(&path);
        let cells = nb.get("cells").unwrap().as_array().unwrap();
        assert_eq!(cells.len(), 3);
        let inserted = &cells[1];
        assert_eq!(inserted.get("cell_type").unwrap(), "code");
    }

    #[test]
    fn apply_delete_removes_cell() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", sample_nb());
        let files = FileStateCache::new();
        prime_cache(&files, &path);

        apply_edit(
            &args_for(&[
                ("notebook_path", json!(path)),
                ("edit_mode", json!("delete")),
                ("cell_id", json!("c1")),
            ]),
            &files,
        )
        .unwrap();

        let nb = parse_file(&path);
        let cells = nb.get("cells").unwrap().as_array().unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].get("id").unwrap(), "intro");
    }

    #[test]
    fn apply_errors_when_path_empty() {
        let files = FileStateCache::new();
        let err = apply_edit(
            &args_for(&[
                ("edit_mode", json!("replace")),
                ("cell_id", json!("x")),
                ("new_source", json!("y")),
            ]),
            &files,
        )
        .unwrap_err();
        assert!(err.contains("notebook_path"));
    }

    #[test]
    fn apply_errors_on_invalid_edit_mode() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", sample_nb());
        let files = FileStateCache::new();
        prime_cache(&files, &path);

        let err = apply_edit(
            &args_for(&[
                ("notebook_path", json!(path)),
                ("edit_mode", json!("twist")),
                ("new_source", json!("y")),
            ]),
            &files,
        )
        .unwrap_err();
        assert!(err.contains("invalid edit_mode"));
    }

    #[test]
    fn apply_errors_when_new_source_missing_for_replace() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", sample_nb());
        let files = FileStateCache::new();
        prime_cache(&files, &path);

        let err = apply_edit(
            &args_for(&[
                ("notebook_path", json!(path)),
                ("edit_mode", json!("replace")),
                ("cell_id", json!("c1")),
            ]),
            &files,
        )
        .unwrap_err();
        assert!(err.contains("new_source"));
    }

    #[test]
    fn apply_errors_when_cell_type_missing_for_insert() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", sample_nb());
        let files = FileStateCache::new();
        prime_cache(&files, &path);

        let err = apply_edit(
            &args_for(&[
                ("notebook_path", json!(path)),
                ("edit_mode", json!("insert")),
                ("new_source", json!("y")),
            ]),
            &files,
        )
        .unwrap_err();
        assert!(err.contains("cell_type"));
    }

    #[test]
    fn apply_replace_clears_outputs_on_code_cell() {
        let dir = tempdir().unwrap();
        let nb_with_outputs = json!({
            "cells": [{
                "cell_type": "code",
                "id": "c1",
                "source": "print(1)",
                "outputs": [{"output_type": "stream", "text": "1"}],
                "execution_count": 5
            }]
        });
        let path = write_nb(dir.path(), "a.ipynb", nb_with_outputs);
        let files = FileStateCache::new();
        prime_cache(&files, &path);

        apply_edit(
            &args_for(&[
                ("notebook_path", json!(path)),
                ("edit_mode", json!("replace")),
                ("cell_id", json!("c1")),
                ("new_source", json!("print(2)")),
            ]),
            &files,
        )
        .unwrap();

        let nb = parse_file(&path);
        let cell = &nb.get("cells").unwrap().as_array().unwrap()[0];
        assert_eq!(cell.get("outputs").unwrap().as_array().unwrap().len(), 0);
        assert!(cell.get("execution_count").unwrap().is_null());
    }

    #[test]
    fn apply_replace_to_markdown_drops_code_fields() {
        let dir = tempdir().unwrap();
        let nb_code = json!({
            "cells": [{
                "cell_type": "code",
                "id": "c1",
                "source": "x=1",
                "outputs": [],
                "execution_count": 1
            }]
        });
        let path = write_nb(dir.path(), "a.ipynb", nb_code);
        let files = FileStateCache::new();
        prime_cache(&files, &path);

        apply_edit(
            &args_for(&[
                ("notebook_path", json!(path)),
                ("edit_mode", json!("replace")),
                ("cell_id", json!("c1")),
                ("cell_type", json!("markdown")),
                ("new_source", json!("# heading")),
            ]),
            &files,
        )
        .unwrap();

        let nb = parse_file(&path);
        let cell = &nb.get("cells").unwrap().as_array().unwrap()[0];
        assert_eq!(cell.get("cell_type").unwrap(), "markdown");
        assert!(cell.get("outputs").is_none());
        assert!(cell.get("execution_count").is_none());
    }

    #[test]
    fn apply_errors_without_prior_read() {
        let dir = tempdir().unwrap();
        let path = write_nb(dir.path(), "a.ipynb", sample_nb());
        let files = FileStateCache::new();
        // No prime_cache - staleness check should reject.
        let err = apply_edit(
            &args_for(&[
                ("notebook_path", json!(path)),
                ("edit_mode", json!("replace")),
                ("cell_id", json!("c1")),
                ("new_source", json!("y")),
            ]),
            &files,
        )
        .unwrap_err();
        assert!(err.contains("read_file"));
    }
}

fn write_notebook(
    path: &str,
    nb: &Value,
    action: &str,
    files: &FileStateCache,
    cwd: &Path,
    home: &Path,
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
            let display = crate::path_display::display_path_from(path, cwd, home);
            if let Some(render) = render {
                NbResult::ok(format!("{action} in {display}"))
                    .with_metadata(render_data_metadata(&render))
            } else {
                NbResult::ok(format!("{action} in {display}"))
            }
        }
        Err(e) => NbResult::err(e.to_string()),
    }
}
