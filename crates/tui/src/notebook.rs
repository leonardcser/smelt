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

/// One notebook cell, normalised into a plain `source` string. The
/// raw JSON value stays available for tools that need fields beyond
/// `kind / id / source / execution_count`.
#[derive(Debug, Clone)]
pub struct Cell {
    pub kind: CellKind,
    pub id: Option<String>,
    pub source: String,
    pub execution_count: Option<i64>,
    pub raw: Value,
}

/// Parsed notebook view. `metadata` and `format_*` mirror nbformat's
/// top-level keys; `cells` walks `cells[]` in order.
#[derive(Debug, Clone)]
pub struct Notebook {
    pub format: Option<i64>,
    pub format_minor: Option<i64>,
    pub metadata: Value,
    pub cells: Vec<Cell>,
    pub raw: Value,
}

/// Parse `.ipynb` JSON. Errors surface as `serde_json::Error`.
pub fn parse(json: &str) -> Result<Notebook, serde_json::Error> {
    let raw: Value = serde_json::from_str(json)?;
    let format = raw.get("nbformat").and_then(|v| v.as_i64());
    let format_minor = raw.get("nbformat_minor").and_then(|v| v.as_i64());
    let metadata = raw.get("metadata").cloned().unwrap_or(Value::Null);

    let mut cells = Vec::new();
    if let Some(arr) = raw.get("cells").and_then(|v| v.as_array()) {
        for cell in arr {
            cells.push(parse_cell(cell));
        }
    }

    Ok(Notebook {
        format,
        format_minor,
        metadata,
        cells,
        raw,
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
        raw: cell.clone(),
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
