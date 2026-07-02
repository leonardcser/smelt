use super::{text_document, LspClient};
use globset::Glob;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, serde::Serialize)]
pub(super) struct SimpleLocation {
    pub(super) file_path: String,
    pub(super) line: u64,
    pub(super) column: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview: Option<String>,
}

impl SimpleLocation {
    pub(super) fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert(
            "file_path".into(),
            Value::String(display_path(&self.file_path)),
        );
        out.insert("line".into(), json!(self.line));
        out.insert("column".into(), json!(self.column));
        if let Some(preview) = &self.preview {
            out.insert("preview".into(), Value::String(preview.clone()));
        }
        Value::Object(out)
    }
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub(super) struct OutlinePosition {
    line: u64,
    column: u64,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub(super) struct OutlineRange {
    start: OutlinePosition,
    end: OutlinePosition,
}

impl OutlineRange {
    fn contains(&self, line: u64, column: u64) -> bool {
        (line > self.start.line || (line == self.start.line && column >= self.start.column))
            && (line < self.end.line || (line == self.end.line && column <= self.end.column))
    }

    fn depth(&self) -> u64 {
        self.end.line.saturating_sub(self.start.line)
    }
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub(super) struct NormalizedSymbol {
    pub(super) name: String,
    pub(super) kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) container_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) range: Option<OutlineRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) selection: Option<OutlineRange>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(super) children: Vec<NormalizedSymbol>,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub(super) struct CompactOutlineSymbol {
    name: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    column: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_column: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    children: Vec<CompactOutlineSymbol>,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub(super) struct WorkspaceSymbol {
    pub(super) name: String,
    pub(super) kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    container_name: Option<String>,
    pub(super) file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) column: Option<u64>,
    server: String,
    pub(super) rank: String,
    #[serde(skip_serializing)]
    pub(super) rank_score: u8,
}

pub(super) async fn request_document_symbols(
    client: &Arc<LspClient>,
    file_path: &str,
) -> Result<Value, String> {
    client
        .request(
            "textDocument/documentSymbol",
            json!({ "textDocument": text_document(file_path)? }),
        )
        .await
}

pub(super) async fn optional_lsp_request(
    client: &Arc<LspClient>,
    method: &str,
    params: Value,
) -> Value {
    match client.request(method, params).await {
        Ok(value) => value,
        Err(err) => json!({ "error": err }),
    }
}

pub(super) fn normalize_hover(value: Value) -> Value {
    if value.get("error").is_some() || value.is_null() {
        return value;
    }
    let contents = value.get("contents").unwrap_or(&value);
    hover_contents_text(contents).map_or(value, Value::String)
}

fn hover_contents_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => {
            let parts = items
                .iter()
                .filter_map(hover_contents_text)
                .filter(|text| !text.trim().is_empty())
                .collect::<Vec<_>>();
            (!parts.is_empty()).then(|| parts.join("\n\n"))
        }
        Value::Object(obj) => obj
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                obj.get("language")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
        _ => None,
    }
}

pub(super) async fn optional_lsp_locations(
    client: &Arc<LspClient>,
    method: &str,
    params: Value,
) -> Value {
    match client.request(method, params).await {
        Ok(value) => {
            let mut locations = normalize_locations(&value);
            add_location_previews(&mut locations);
            Value::Array(locations.into_iter().map(|loc| loc.to_json()).collect())
        }
        Err(err) => json!({ "error": err }),
    }
}

pub(super) fn normalize_document_symbols(value: &Value) -> Vec<NormalizedSymbol> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items.iter().filter_map(normalize_symbol).collect()
}

pub(super) fn count_symbols(symbols: &[NormalizedSymbol]) -> usize {
    symbols
        .iter()
        .map(|symbol| 1 + count_symbols(&symbol.children))
        .sum()
}

pub(super) fn count_compact_outline_symbols(symbols: &[CompactOutlineSymbol]) -> usize {
    symbols
        .iter()
        .map(|symbol| 1 + count_compact_outline_symbols(&symbol.children))
        .sum()
}

pub(super) struct OutlineFilter<'a> {
    pub(super) symbol: Option<&'a str>,
    pub(super) kind: Option<&'a str>,
    pub(super) name_contains: Option<&'a str>,
    pub(super) max_depth: Option<usize>,
}

pub(super) fn compact_outline_symbols_filtered(
    symbols: &[NormalizedSymbol],
    remaining: &mut usize,
    filter: OutlineFilter<'_>,
) -> Vec<CompactOutlineSymbol> {
    if !filter.has_symbol_filters() {
        return compact_outline_symbols_unfiltered(symbols, remaining, filter.max_depth);
    }
    compact_outline_symbols_filtered_at(symbols, remaining, filter)
}

impl OutlineFilter<'_> {
    fn has_symbol_filters(&self) -> bool {
        self.symbol.is_some() || self.kind.is_some() || self.name_contains.is_some()
    }
}

fn compact_outline_symbols_unfiltered(
    symbols: &[NormalizedSymbol],
    remaining: &mut usize,
    max_depth: Option<usize>,
) -> Vec<CompactOutlineSymbol> {
    let mut out = Vec::new();
    for symbol in symbols {
        if *remaining == 0 {
            break;
        }
        *remaining -= 1;
        out.push(compact_outline_symbol(symbol, Vec::new()));
    }

    if max_depth == Some(0) {
        return out;
    }

    for (item, symbol) in out.iter_mut().zip(symbols) {
        fill_unfiltered_outline_children(item, symbol, remaining, max_depth, 1);
        if *remaining == 0 {
            break;
        }
    }
    out
}

fn fill_unfiltered_outline_children(
    item: &mut CompactOutlineSymbol,
    symbol: &NormalizedSymbol,
    remaining: &mut usize,
    max_depth: Option<usize>,
    depth: usize,
) {
    if *remaining == 0 || max_depth.is_some_and(|max| depth > max) {
        return;
    }
    for child in &symbol.children {
        if *remaining == 0 {
            break;
        }
        *remaining -= 1;
        let mut child_item = compact_outline_symbol(child, Vec::new());
        fill_unfiltered_outline_children(&mut child_item, child, remaining, max_depth, depth + 1);
        item.children.push(child_item);
    }
}

fn compact_outline_symbols_filtered_at(
    symbols: &[NormalizedSymbol],
    remaining: &mut usize,
    filter: OutlineFilter<'_>,
) -> Vec<CompactOutlineSymbol> {
    let mut out = Vec::new();
    let filter = PreparedOutlineFilter::from(filter);
    for item in symbols {
        collect_compact_outline_symbol(item, remaining, &filter, &mut out);
        if *remaining == 0 {
            break;
        }
    }
    out
}

struct PreparedOutlineFilter {
    symbol: Option<String>,
    kind: Option<String>,
    name_contains: Option<String>,
}

impl From<OutlineFilter<'_>> for PreparedOutlineFilter {
    fn from(filter: OutlineFilter<'_>) -> Self {
        Self {
            symbol: filter.symbol.map(str::to_ascii_lowercase),
            kind: filter.kind.map(normalize_kind_filter),
            name_contains: filter.name_contains.map(str::to_ascii_lowercase),
        }
    }
}

impl PreparedOutlineFilter {
    fn matches(&self, symbol: &NormalizedSymbol) -> bool {
        let name_lc = symbol.name.to_ascii_lowercase();
        self.symbol
            .as_deref()
            .is_none_or(|filter| name_lc == filter)
            && self
                .kind
                .as_deref()
                .is_none_or(|filter| symbol.kind == filter)
            && self
                .name_contains
                .as_deref()
                .is_none_or(|filter| name_lc.contains(filter))
    }
}

fn collect_compact_outline_symbol(
    symbol: &NormalizedSymbol,
    remaining: &mut usize,
    filter: &PreparedOutlineFilter,
    out: &mut Vec<CompactOutlineSymbol>,
) {
    if *remaining == 0 {
        return;
    }

    if filter.matches(symbol) {
        *remaining -= 1;
        let mut item = compact_outline_symbol(symbol, Vec::new());
        for child in &symbol.children {
            collect_compact_outline_symbol(child, remaining, filter, &mut item.children);
            if *remaining == 0 {
                break;
            }
        }
        out.push(item);
        return;
    }

    let mut children = Vec::new();
    for child in &symbol.children {
        collect_compact_outline_symbol(child, remaining, filter, &mut children);
        if *remaining == 0 {
            break;
        }
    }
    if !children.is_empty() {
        out.push(compact_outline_symbol(symbol, children));
    }
}

fn compact_outline_symbol(
    symbol: &NormalizedSymbol,
    children: Vec<CompactOutlineSymbol>,
) -> CompactOutlineSymbol {
    let position = symbol
        .selection
        .as_ref()
        .or(symbol.range.as_ref())
        .map(|range| (range.start.line, range.start.column));
    let end_position = symbol
        .range
        .as_ref()
        .map(|range| (range.end.line, range.end.column));
    CompactOutlineSymbol {
        name: symbol.name.clone(),
        kind: symbol.kind.clone(),
        line: position.map(|(line, _)| line),
        column: position.map(|(_, column)| column),
        end_line: end_position.map(|(line, _)| line),
        end_column: end_position.map(|(_, column)| column),
        children,
    }
}

fn normalize_symbol(value: &Value) -> Option<NormalizedSymbol> {
    let name = value.get("name")?.as_str()?.to_string();
    let kind = value
        .get("kind")
        .and_then(Value::as_u64)
        .map(symbol_kind)
        .unwrap_or("unknown")
        .to_string();
    let detail = value
        .get("detail")
        .and_then(Value::as_str)
        .filter(|detail| !detail.is_empty())
        .map(str::to_string);
    let container_name = value
        .get("containerName")
        .and_then(Value::as_str)
        .map(str::to_string);
    let (file_path, range) = if let Some(location) = value.get("location") {
        (
            location
                .get("uri")
                .and_then(Value::as_str)
                .map(uri_path_string),
            location.get("range").and_then(normalize_range),
        )
    } else {
        (None, value.get("range").and_then(normalize_range))
    };
    let selection = value.get("selectionRange").and_then(normalize_range);
    let children = value
        .get("children")
        .and_then(Value::as_array)
        .map(|children| children.iter().filter_map(normalize_symbol).collect())
        .unwrap_or_default();
    Some(NormalizedSymbol {
        name,
        kind,
        detail,
        container_name,
        file_path,
        range,
        selection,
        children,
    })
}

pub(super) fn normalize_locations(value: &Value) -> Vec<SimpleLocation> {
    match value {
        Value::Array(items) => items.iter().filter_map(simple_location).collect(),
        Value::Null => Vec::new(),
        other => simple_location(other).into_iter().collect(),
    }
}

pub(super) fn add_location_previews(locations: &mut [SimpleLocation]) {
    let mut previews = SourcePreviewCache::default();
    for loc in locations {
        loc.preview = previews.preview(&loc.file_path, loc.line);
    }
}

#[derive(Default)]
struct SourcePreviewCache {
    by_file: HashMap<String, Option<Vec<String>>>,
}

impl SourcePreviewCache {
    fn preview(&mut self, file_path: &str, line: u64) -> Option<String> {
        let lines = self
            .by_file
            .entry(file_path.to_string())
            .or_insert_with(|| {
                std::fs::read_to_string(file_path)
                    .ok()
                    .map(|text| text.lines().map(str::to_string).collect())
            })
            .as_ref()?;
        trim_preview(lines.get(line.saturating_sub(1) as usize)?.trim())
    }
}

fn simple_location(value: &Value) -> Option<SimpleLocation> {
    let uri = value
        .get("uri")
        .or_else(|| value.get("targetUri"))
        .and_then(Value::as_str)?;
    let range = value
        .get("range")
        .or_else(|| value.get("targetSelectionRange"))
        .or_else(|| value.get("targetRange"))?;
    let start = range.get("start")?;
    let line = start.get("line")?.as_u64()? + 1;
    let column = start.get("character")?.as_u64()? + 1;
    let file_path = uri_path_string(uri);
    Some(SimpleLocation {
        file_path,
        line,
        column,
        preview: None,
    })
}

pub(super) fn collect_workspace_symbols(
    value: &Value,
    server: &str,
    kind_filter: Option<&str>,
    path_glob: Option<&str>,
    out: &mut Vec<WorkspaceSymbol>,
) {
    let Some(items) = value.as_array() else {
        return;
    };
    for item in items {
        let Some(symbol_model) = normalize_symbol(item) else {
            continue;
        };
        let kind = symbol_model.kind.as_str();
        if let Some(filter) = kind_filter {
            if kind != normalize_kind_filter(filter) {
                continue;
            }
        }
        let Some(file_path) = symbol_model.file_path.as_deref() else {
            continue;
        };
        if let Some(pattern) = path_glob {
            if !path_matches_glob(file_path, pattern) {
                continue;
            }
        }
        let position = symbol_model
            .range
            .as_ref()
            .map(|range| (range.start.line, range.start.column));
        let rank_score = symbol_rank_score(&symbol_model.name, "");
        out.push(WorkspaceSymbol {
            name: symbol_model.name,
            kind: symbol_model.kind,
            detail: symbol_model.detail,
            container_name: symbol_model.container_name,
            file_path: display_path(file_path),
            line: position.map(|(line, _)| line),
            column: position.map(|(_, column)| column),
            server: server.to_string(),
            rank: symbol_rank_label(rank_score).to_string(),
            rank_score,
        });
    }
}

pub(super) fn rank_workspace_symbols(symbols: &mut Vec<WorkspaceSymbol>, query: &str, exact: bool) {
    let mut seen = HashSet::new();
    symbols.retain_mut(|symbol| {
        symbol.rank_score = symbol_rank_score(&symbol.name, query);
        symbol.rank = symbol_rank_label(symbol.rank_score).to_string();
        if exact && symbol.rank_score != 0 {
            return false;
        }
        seen.insert((
            symbol.name.clone(),
            symbol.kind.clone(),
            symbol.file_path.clone(),
            symbol.line,
            symbol.column,
        ))
    });
    symbols.sort_by_key(symbol_sort_key);
}

fn symbol_rank_score(name: &str, query: &str) -> u8 {
    if query.is_empty() {
        return 3;
    }
    let name_lc = name.to_ascii_lowercase();
    let query_lc = query.to_ascii_lowercase();
    if name == query {
        0
    } else if name_lc == query_lc {
        1
    } else if name_lc.contains(&query_lc) || symbol_query_tokens_match(&name_lc, &query_lc) {
        2
    } else {
        3
    }
}

fn symbol_query_tokens_match(name_lc: &str, query_lc: &str) -> bool {
    let mut tokens = query_lc
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty());
    let Some(first) = tokens.next() else {
        return false;
    };
    name_lc.contains(first) && tokens.all(|token| name_lc.contains(token))
}

fn symbol_rank_label(rank: u8) -> &'static str {
    match rank {
        0 => "exact",
        1 => "case_insensitive",
        2 => "contains",
        _ => "other",
    }
}

pub(super) fn symbol_sort_key(value: &WorkspaceSymbol) -> (u8, String, u64, u64, String) {
    (
        value.rank_score,
        value.file_path.clone(),
        value.line.unwrap_or_default(),
        value.column.unwrap_or_default(),
        value.name.clone(),
    )
}

fn normalize_range(range: &Value) -> Option<OutlineRange> {
    Some(OutlineRange {
        start: normalize_position(range.get("start"))?,
        end: normalize_position(range.get("end"))?,
    })
}

fn normalize_position(position: Option<&Value>) -> Option<OutlinePosition> {
    Some(OutlinePosition {
        line: position?.get("line")?.as_u64()? + 1,
        column: position?.get("character")?.as_u64()? + 1,
    })
}

#[cfg(test)]
pub(super) fn enclosing_symbol_name(
    symbols: &[NormalizedSymbol],
    line: u64,
    column: u64,
) -> Option<String> {
    let mut best = None;
    for item in symbols {
        enclosing_symbol_in(item, line, column, &mut best);
    }
    best.map(|(_, name)| name)
}

#[cfg(test)]
fn enclosing_symbol_in(
    symbol: &NormalizedSymbol,
    line: u64,
    column: u64,
    best: &mut Option<(u64, String)>,
) {
    let Some(range) = &symbol.range else {
        return;
    };
    if !range.contains(line, column) {
        return;
    }
    let depth = range.depth();
    let label = format!("{} {}", symbol.kind, symbol.name);
    if best
        .as_ref()
        .is_none_or(|(best_depth, _)| depth <= *best_depth)
    {
        *best = Some((depth, label));
    }
    for child in &symbol.children {
        enclosing_symbol_in(child, line, column, best);
    }
}

pub(super) fn enclosing_symbol_at(symbols: &[NormalizedSymbol], line: u64, column: u64) -> Value {
    let mut best = None;
    for item in symbols {
        enclosing_symbol_json_in(item, line, column, &mut best);
    }
    best.map(|(_, value)| value).unwrap_or(Value::Null)
}

fn enclosing_symbol_json_in(
    symbol: &NormalizedSymbol,
    line: u64,
    column: u64,
    best: &mut Option<(u64, Value)>,
) {
    let Some(range) = &symbol.range else {
        return;
    };
    if !range.contains(line, column) {
        return;
    }
    let depth = range.depth();
    if best
        .as_ref()
        .is_none_or(|(best_depth, _)| depth <= *best_depth)
    {
        *best = Some((
            depth,
            json!({
                "name": symbol.name,
                "kind": symbol.kind,
                "range": range,
            }),
        ));
    }
    for child in &symbol.children {
        enclosing_symbol_json_in(child, line, column, best);
    }
}

pub(super) fn outline_path_at_position(
    symbols: &[NormalizedSymbol],
    line: u64,
    column: u64,
) -> Value {
    for item in symbols {
        if let Some(path) = outline_path_in(item, line, column) {
            return Value::Array(path);
        }
    }
    Value::Array(Vec::new())
}

fn outline_path_in(symbol: &NormalizedSymbol, line: u64, column: u64) -> Option<Vec<Value>> {
    let range = symbol.range.as_ref()?;
    if !range.contains(line, column) {
        return None;
    }
    let mut path = vec![json!({
        "name": symbol.name,
        "kind": symbol.kind,
        "range": range,
    })];
    for child in &symbol.children {
        if let Some(mut child_path) = outline_path_in(child, line, column) {
            path.append(&mut child_path);
            break;
        }
    }
    Some(path)
}

fn symbol_kind(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum_member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type_parameter",
        _ => "unknown",
    }
}

pub(super) fn normalize_kind_filter(kind: &str) -> String {
    let normalized = kind.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    match normalized.as_str() {
        "trait" => "interface".into(),
        "fn" => "function".into(),
        "const" => "constant".into(),
        other => other.into(),
    }
}

pub(super) fn bounded_limit(limit: usize, default: usize, max: usize) -> usize {
    if limit == 0 {
        default
    } else {
        limit.min(max)
    }
}

fn path_matches_glob(file_path: &str, pattern: &str) -> bool {
    let Ok(glob) = Glob::new(pattern) else {
        return false;
    };
    let matcher = glob.compile_matcher();
    let path = Path::new(file_path);
    if matcher.is_match(path) {
        return true;
    }
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(relative) = path.strip_prefix(cwd) {
            return matcher.is_match(relative);
        }
    }
    false
}

pub(super) fn display_path(file_path: &str) -> String {
    crate::path_display::display_path(file_path)
}

fn trim_preview(preview: &str) -> Option<String> {
    if preview.chars().count() > 200 {
        Some(format!(
            "{}…",
            preview.chars().take(200).collect::<String>()
        ))
    } else {
        Some(preview.to_string())
    }
}

pub(super) fn absolute_path_string(file_path: &str) -> String {
    let path = PathBuf::from(file_path);
    if path.is_absolute() {
        return path.display().to_string();
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(path)
        .display()
        .to_string()
}

fn uri_path_string(uri: &str) -> String {
    super::uri_to_path(uri)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| uri.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_and_filters_workspace_symbols() {
        let mut symbols = Vec::new();
        collect_workspace_symbols(
            &json!([
                {"name":"handle_request","kind":12,"location":{"uri":"file:///tmp/a.rs","range":{"start":{"line":9,"character":2},"end":{"line":9,"character":8}}}},
                {"name":"handle_request","kind":12,"location":{"uri":"file:///tmp/a.rs","range":{"start":{"line":9,"character":2},"end":{"line":9,"character":8}}}},
                {"name":"HandleRequest","kind":23,"location":{"uri":"file:///tmp/b.rs","range":{"start":{"line":1,"character":0},"end":{"line":1,"character":13}}}},
                {"name":"request_handler","kind":12,"location":{"uri":"file:///tmp/c.rs","range":{"start":{"line":2,"character":0},"end":{"line":2,"character":15}}}}
            ]),
            "rust-analyzer",
            None,
            None,
            &mut symbols,
        );
        rank_workspace_symbols(&mut symbols, "handle_request", false);
        assert_eq!(symbols.len(), 3);
        assert_eq!(symbols[0].name, "handle_request");
        assert_eq!(symbols[0].rank, "exact");

        rank_workspace_symbols(&mut symbols, "handle_request", true);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "handle_request");
    }

    #[test]
    fn filters_outline_by_kind_name_and_depth() {
        let symbols = normalize_document_symbols(&json!([
            {"name":"outer","kind":12,"range":{"start":{"line":0,"character":0},"end":{"line":9,"character":0}},"selectionRange":{"start":{"line":0,"character":3},"end":{"line":0,"character":8}},"children":[
                {"name":"target_child","kind":6,"range":{"start":{"line":2,"character":2},"end":{"line":4,"character":2}},"selectionRange":{"start":{"line":2,"character":5},"end":{"line":2,"character":17}}}
            ]},
            {"name":"TargetType","kind":23,"range":{"start":{"line":11,"character":0},"end":{"line":12,"character":0}},"selectionRange":{"start":{"line":11,"character":0},"end":{"line":11,"character":10}}}
        ]));
        let mut remaining = 10;
        let outline = compact_outline_symbols_filtered(
            &symbols,
            &mut remaining,
            OutlineFilter {
                symbol: None,
                kind: Some("method"),
                name_contains: Some("target"),
                max_depth: Some(1),
            },
        );
        assert_eq!(outline.len(), 1);
        assert_eq!(outline[0].name, "outer");
        assert_eq!(outline[0].children.len(), 1);
        assert_eq!(outline[0].children[0].name, "target_child");
        assert_eq!(outline[0].children[0].line, Some(3));
        assert_eq!(outline[0].children[0].end_line, Some(5));
    }

    #[test]
    fn filtered_outline_searches_nested_symbols_past_output_depth() {
        let symbols = normalize_document_symbols(&json!([
            {"name":"module","kind":2,"range":{"start":{"line":0,"character":0},"end":{"line":30,"character":0}},"selectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":6}},"children":[
                {"name":"Container","kind":5,"range":{"start":{"line":4,"character":0},"end":{"line":24,"character":0}},"selectionRange":{"start":{"line":4,"character":0},"end":{"line":4,"character":9}},"children":[
                    {"name":"target_method","kind":6,"range":{"start":{"line":10,"character":4},"end":{"line":20,"character":4}},"selectionRange":{"start":{"line":10,"character":7},"end":{"line":10,"character":20}}}
                ]}
            ]}
        ]));
        let mut remaining = 10;
        let outline = compact_outline_symbols_filtered(
            &symbols,
            &mut remaining,
            OutlineFilter {
                symbol: None,
                kind: Some("method"),
                name_contains: Some("target"),
                max_depth: Some(1),
            },
        );

        assert_eq!(outline.len(), 1);
        assert_eq!(outline[0].name, "module");
        assert_eq!(outline[0].children.len(), 1);
        assert_eq!(outline[0].children[0].name, "Container");
        assert_eq!(outline[0].children[0].children.len(), 1);
        assert_eq!(outline[0].children[0].children[0].name, "target_method");
    }

    #[test]
    fn unfiltered_outline_prefers_top_level_symbols_before_children() {
        let symbols = normalize_document_symbols(&json!([
            {"name":"module_a","kind":2,"range":{"start":{"line":0,"character":0},"end":{"line":10,"character":0}},"selectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":8}},"children":[
                {"name":"child_a","kind":12,"range":{"start":{"line":1,"character":0},"end":{"line":2,"character":0}},"selectionRange":{"start":{"line":1,"character":0},"end":{"line":1,"character":7}}}
            ]},
            {"name":"module_b","kind":2,"range":{"start":{"line":12,"character":0},"end":{"line":20,"character":0}},"selectionRange":{"start":{"line":12,"character":0},"end":{"line":12,"character":8}}}
        ]));
        let mut remaining = 2;
        let outline = compact_outline_symbols_filtered(
            &symbols,
            &mut remaining,
            OutlineFilter {
                symbol: None,
                kind: None,
                name_contains: None,
                max_depth: None,
            },
        );

        assert_eq!(count_compact_outline_symbols(&outline), 2);
        assert_eq!(outline.len(), 2);
        assert_eq!(outline[0].name, "module_a");
        assert!(outline[0].children.is_empty());
        assert_eq!(outline[1].name, "module_b");
    }

    #[test]
    fn unfiltered_outline_returns_visible_symbols_for_nested_files() {
        let symbols = normalize_document_symbols(&json!([
            {"name":"root","kind":2,"range":{"start":{"line":0,"character":0},"end":{"line":100,"character":0}},"selectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":4}},"children":[
                {"name":"child_1","kind":12,"range":{"start":{"line":1,"character":0},"end":{"line":2,"character":0}},"selectionRange":{"start":{"line":1,"character":0},"end":{"line":1,"character":7}}},
                {"name":"child_2","kind":12,"range":{"start":{"line":3,"character":0},"end":{"line":4,"character":0}},"selectionRange":{"start":{"line":3,"character":0},"end":{"line":3,"character":7}}}
            ]}
        ]));
        let mut remaining = 2;
        let outline = compact_outline_symbols_filtered(
            &symbols,
            &mut remaining,
            OutlineFilter {
                symbol: None,
                kind: None,
                name_contains: None,
                max_depth: None,
            },
        );

        assert_eq!(count_symbols(&symbols), 3);
        assert_eq!(count_compact_outline_symbols(&outline), 2);
        assert_eq!(outline[0].name, "root");
        assert_eq!(outline[0].children.len(), 1);
        assert_eq!(outline[0].children[0].name, "child_1");
    }

    #[test]
    fn filtered_outline_counts_visible_context_symbols() {
        let symbols = normalize_document_symbols(&json!([
            {"name":"outer","kind":12,"range":{"start":{"line":0,"character":0},"end":{"line":9,"character":0}},"selectionRange":{"start":{"line":0,"character":3},"end":{"line":0,"character":8}},"children":[
                {"name":"target_child","kind":6,"range":{"start":{"line":2,"character":2},"end":{"line":4,"character":2}},"selectionRange":{"start":{"line":2,"character":5},"end":{"line":2,"character":17}}}
            ]}
        ]));
        let mut remaining = 10;
        let outline = compact_outline_symbols_filtered(
            &symbols,
            &mut remaining,
            OutlineFilter {
                symbol: None,
                kind: Some("method"),
                name_contains: Some("target"),
                max_depth: Some(1),
            },
        );

        assert_eq!(count_compact_outline_symbols(&outline), 2);
    }

    #[test]
    fn display_paths_are_relative_inside_current_directory() {
        let path = std::env::current_dir()
            .unwrap()
            .join("crates/core/src/lsp/mod.rs");
        assert_eq!(
            display_path(&path.display().to_string()),
            "crates/core/src/lsp/mod.rs"
        );

        let loc = SimpleLocation {
            file_path: path.display().to_string(),
            line: 7,
            column: 3,
            preview: None,
        }
        .to_json();
        assert_eq!(loc["file_path"], "crates/core/src/lsp/mod.rs");
    }

    #[test]
    fn groups_by_enclosing_caller_symbol() {
        let symbols = normalize_document_symbols(&json!([
            {"name":"caller","kind":12,"range":{"start":{"line":4,"character":0},"end":{"line":8,"character":0}},"selectionRange":{"start":{"line":4,"character":3},"end":{"line":4,"character":9}}}
        ]));
        assert_eq!(
            enclosing_symbol_name(&symbols, 6, 4),
            Some("function caller".to_string())
        );
    }
}
