use super::*;

impl LspManager {
    pub async fn dispatch_local(&self, operation: &str, args: Value) -> Result<Value, String> {
        match operation {
            "status" => {
                let file_path = optional_string(&args, "file_path");
                Ok(Value::String(self.status(file_path.as_deref()).await))
            }
            "outline" => {
                let file_path = required_string(&args, "file_path")?;
                let symbol = optional_string(&args, "symbol");
                let kind = optional_string(&args, "kind");
                let name_contains = optional_string(&args, "name_contains");
                self.outline(OutlineOptions {
                    file_path: &file_path,
                    max_symbols: optional_usize(&args, "max_symbols").unwrap_or(200),
                    symbol: symbol.as_deref(),
                    kind: kind.as_deref(),
                    name_contains: name_contains.as_deref(),
                    max_depth: optional_usize(&args, "max_depth"),
                })
                .await
            }
            "workspace_symbols" => {
                let query = required_string(&args, "query")?;
                let kind = optional_string(&args, "kind");
                let path_glob = optional_string(&args, "path_glob");
                self.workspace_symbols(
                    &query,
                    kind.as_deref(),
                    path_glob.as_deref(),
                    optional_usize(&args, "limit").unwrap_or(20),
                    args.get("exact").and_then(Value::as_bool).unwrap_or(false),
                )
                .await
            }
            "inspect_symbol_at" => {
                let file_path = required_string(&args, "file_path")?;
                self.inspect_symbol(
                    &file_path,
                    int_arg(&args, "line")?,
                    int_arg(&args, "column")?,
                    optional_u64(&args, "depth").unwrap_or(1),
                )
                .await
            }
            "inspect_symbol" => {
                let (file_path, line, column) = self.resolve_symbol_query(&args).await?;
                self.inspect_symbol(
                    &file_path,
                    line,
                    column,
                    optional_u64(&args, "depth").unwrap_or(1),
                )
                .await
            }
            "definition" => {
                let file_path = required_string(&args, "file_path")?;
                self.definition(
                    &file_path,
                    int_arg(&args, "line")?,
                    int_arg(&args, "column")?,
                )
                .await
            }
            "references" => {
                let file_path = required_string(&args, "file_path")?;
                self.references(
                    &file_path,
                    int_arg(&args, "line")?,
                    int_arg(&args, "column")?,
                    ReferenceOptions {
                        include_declaration: args
                            .get("include_declaration")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        limit: optional_usize(&args, "limit").unwrap_or(50),
                        raw: args.get("raw").and_then(Value::as_bool).unwrap_or(false),
                    },
                )
                .await
            }
            "diagnostics" => {
                let file_path = args.get("file_path").and_then(Value::as_str);
                self.diagnostics(file_path).await
            }
            "rename_preview" | "rename" => {
                let file_path = required_string(&args, "file_path")?;
                let new_name = required_string(&args, "new_name")?;
                self.rename(
                    &file_path,
                    int_arg(&args, "line")?,
                    int_arg(&args, "column")?,
                    &new_name,
                    operation == "rename",
                )
                .await
            }
            _ => Err(format!("unknown LSP operation: {operation}")),
        }
    }

    async fn resolve_symbol_query(&self, args: &Value) -> Result<(String, u64, u64), String> {
        let query = required_string(args, "query")?;
        let kind = optional_string(args, "kind");
        let path_glob = optional_string(args, "path_glob");
        let result = self
            .workspace_symbols(
                &query,
                kind.as_deref(),
                path_glob.as_deref(),
                5,
                args.get("exact").and_then(Value::as_bool).unwrap_or(true),
            )
            .await?;
        let symbols = result
            .get("symbols")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("no symbol found for query: {query}"))?;
        if symbols.is_empty() {
            return Err(format!("no symbol found for query: {query}"));
        }
        let exact_count = symbols
            .iter()
            .filter(|symbol| symbol.get("rank").and_then(Value::as_str) == Some("exact"))
            .count();
        if exact_count > 1 {
            return Err(json!({
                "error": "ambiguous symbol query",
                "query": query,
                "candidates": symbols,
            })
            .to_string());
        }
        let symbol = symbols.first().unwrap();
        Ok((
            absolute_path_string(
                symbol
                    .get("file_path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("symbol has no file_path: {query}"))?,
            ),
            symbol
                .get("line")
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("symbol has no line: {query}"))?,
            symbol
                .get("column")
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("symbol has no column: {query}"))?,
        ))
    }
}

fn required_string(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing required argument: {key}"))
}

fn int_arg(args: &Value, key: &str) -> Result<u64, String> {
    args.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("missing required argument: {key}"))
}

fn optional_string(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn optional_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn optional_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}
