//! Walk every `register_fn` / `register_ui_fn` registration and emit:
//!   - LuaCATS stubs to `runtime/lua/smelt/_meta/<module>.lua`
//!     (consumed by lua-language-server for IDE completion)
//!   - Markdown reference pages to `docs/docs/reference/api/<module>.md`
//!     plus an `index.md` overview (rendered by the docs site)
//!   - A zensical nav block between `# >>> LUA API NAV` and
//!     `# <<< LUA API NAV` markers in `docs/zensical.toml`, so adding
//!     a new module surfaces in the docs site without manual edits.
//!
//! Outputs are derived from `LuaFnMeta` entries pushed by `register_fn`
//! at registration time, so doc strings live next to the registration
//! in Rust and never drift out of sync.
//!
//! Usage: `cargo xtask gen-lua-docs` from the repo root.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use std::collections::BTreeSet;

use smelt_core::lua::doc::{
    aliases_snapshot, classes_snapshot, modules_snapshot, snapshot, LuaFnMeta, Tier,
};
use smelt_core::lua::lua_type::{LuaAliasDecl, LuaClassDecl};
use tui::lua::LuaRuntime;

/// Set of LuaCATS type names declared via `#[derive(LuaOpts)]` /
/// `#[derive(LuaAlias)]`. Used by the markdown emitter to turn typed
/// references in function signatures into anchor links into `types.md`.
struct TypeIndex {
    known: BTreeSet<String>,
}

impl TypeIndex {
    fn new(classes: &[LuaClassDecl], aliases: &[LuaAliasDecl]) -> Self {
        let mut known = BTreeSet::new();
        for c in classes {
            known.insert(c.name.to_string());
        }
        for a in aliases {
            known.insert(a.name.to_string());
        }
        Self { known }
    }

    /// Find typed references in a signature and return them as
    /// markdown links (`[name](types.md#anchor)`), de-duplicated and
    /// in declaration order. Used to render a "Types: …" hint line
    /// under each function in the per-module markdown pages, since the
    /// signature itself sits in a code block where markdown links
    /// would not render.
    fn linkify_signature(&self, sig: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let bytes = sig.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let prev = if i == 0 { 0 } else { bytes[i - 1] };
            if prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'.' {
                i += 1;
                continue;
            }
            let mut hit: Option<&str> = None;
            for name in &self.known {
                let n = name.as_bytes();
                if i + n.len() > bytes.len() {
                    continue;
                }
                if &bytes[i..i + n.len()] != n {
                    continue;
                }
                let next = bytes.get(i + n.len()).copied().unwrap_or(0);
                if next.is_ascii_alphanumeric() || next == b'_' || next == b'.' {
                    continue;
                }
                if hit.map(|h| h.len() < name.len()).unwrap_or(true) {
                    hit = Some(name.as_str());
                }
            }
            if let Some(name) = hit {
                let anchor = name.to_ascii_lowercase().replace('.', "");
                let link = format!("[`{name}`](types.md#{anchor})");
                if !out.contains(&link) {
                    out.push(link);
                }
                i += name.len();
            } else {
                i += 1;
            }
        }
        out
    }

    fn anchor(&self, name: &str) -> String {
        let _ = self;
        name.to_ascii_lowercase().replace('.', "")
    }

    fn linkify_type(&self, ty: &str) -> String {
        // The Type column in the types.md table contains a single LuaCATS
        // type expression (e.g. `smelt.buf.VirtTextPos`, `integer`,
        // `smelt.buf.ExtmarkOpts?`). Strip the optional `?` suffix and
        // any trailing `[]` for matching.
        let stripped = ty.trim_end_matches('?').trim_end_matches("[]");
        if self.known.contains(stripped) {
            let anchor = self.anchor(stripped);
            format!("[{ty}](types.md#{anchor})")
        } else {
            format!("`{ty}`")
        }
    }
}

pub fn run() {
    if let Err(e) = run_inner() {
        eprintln!("gen-lua-docs: {e}");
        std::process::exit(1);
    }
}

fn run_inner() -> std::io::Result<()> {
    // Register the full API surface (host-tier + UiHost-tier) without
    // loading bootstrap chunks or spinning up the full runtime state.
    // Registration populates the static doc registry as a side effect.
    #[allow(clippy::io_other_error)]
    LuaRuntime::register_for_docs()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    let metas = snapshot();
    if metas.is_empty() {
        eprintln!("no Lua functions registered — has any module been migrated to register_fn yet?");
        std::process::exit(1);
    }

    let mut by_mod: BTreeMap<&str, Vec<&LuaFnMeta>> = BTreeMap::new();
    for m in &metas {
        by_mod.entry(m.module).or_default().push(m);
    }
    for fns in by_mod.values_mut() {
        fns.sort_by_key(|m| m.name);
    }

    let root = repo_root()?;
    let stubs_dir = root.join("runtime/lua/smelt/_meta");
    let md_dir = root.join("docs/docs/reference/api");
    let zensical = root.join("docs/zensical.toml");
    std::fs::create_dir_all(&stubs_dir)?;
    std::fs::create_dir_all(&md_dir)?;

    let mut classes = classes_snapshot();
    classes.sort_by(|a, b| a.name.cmp(b.name));
    let mut aliases = aliases_snapshot();
    aliases.sort_by(|a, b| a.name.cmp(b.name));

    let type_index = TypeIndex::new(&classes, &aliases);
    let mod_docs: BTreeMap<&str, &str> = modules_snapshot()
        .into_iter()
        .map(|m| (m.module, m.doc))
        .collect();

    let mut expected_stems: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    expected_stems.insert("_types".into());
    expected_stems.insert("index".into());
    expected_stems.insert("types".into());

    for (module, fns) in &by_mod {
        if !module.starts_with("smelt.") && *module != "smelt" {
            continue; // skip internal helpers
        }
        if mod_docs.get(module).copied().unwrap_or("").is_empty() {
            eprintln!("warning: module `{module}` has functions but no module doc; consider adding record_module_doc");
        }
        let file_stem = module_file_stem(module);
        expected_stems.insert(file_stem.clone());
        let mod_doc = mod_docs.get(module).copied().unwrap_or("");
        std::fs::write(
            stubs_dir.join(format!("{file_stem}.lua")),
            render_stub(module, fns, mod_doc),
        )?;
        std::fs::write(
            md_dir.join(format!("{file_stem}.md")),
            render_markdown(module, fns, &type_index, mod_doc),
        )?;
    }

    clean_stale_files(&stubs_dir, "lua", &expected_stems)?;
    clean_stale_files(&md_dir, "md", &expected_stems)?;

    std::fs::write(
        stubs_dir.join("_types.lua"),
        render_types_stub(&classes, &aliases),
    )?;
    std::fs::write(
        md_dir.join("types.md"),
        render_types_markdown(&classes, &aliases, &type_index),
    )?;

    std::fs::write(
        md_dir.join("index.md"),
        render_index(&by_mod, classes.len(), aliases.len()),
    )?;
    let nav_status = sync_zensical_nav(&zensical, &by_mod)?;

    let total_fns: usize = by_mod.values().map(|v| v.len()).sum();
    println!(
        "wrote {} module(s) ({} function(s)), {} class(es), {} alias(es) to:",
        by_mod.len(),
        total_fns,
        classes.len(),
        aliases.len(),
    );
    println!("  {}", stubs_dir.display());
    println!("  {} (+ index.md, types.md)", md_dir.display());
    println!("zensical.toml nav: {nav_status}");
    Ok(())
}

fn module_file_stem(module: &str) -> String {
    if module == "smelt" {
        "index_smelt".into()
    } else {
        module.trim_start_matches("smelt.").replace('.', "_")
    }
}

fn local_name(module: &str) -> String {
    module
        .trim_start_matches("smelt.")
        .replace('.', "_")
        .to_string()
}

fn clean_sig(sig: &str) -> String {
    sig.replace("self: any, ", "").replace("self: any", "")
}

fn strip_markdown_links(text: &str) -> String {
    // Turn `[foo](bar)` into `foo` for LuaCATS comments where links
    // are not clickable.
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        if let Some(close_bracket) = rest.find("](") {
            let link_text = &rest[1..close_bracket];
            if let Some(close_paren) = rest.find(')') {
                out.push_str(link_text);
                rest = &rest[close_paren + 1..];
                continue;
            }
        }
        // Not a well-formed link; emit the '[' and continue.
        out.push('[');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

fn render_stub(module: &str, fns: &[&LuaFnMeta], mod_doc: &str) -> String {
    let mut s = String::new();
    s.push_str("---@meta\n\n");
    s.push_str("-- Auto-generated by `cargo xtask gen-lua-docs`.\n");
    s.push_str("-- Do not edit by hand; update the `register_fn` call in Rust instead.\n\n");
    if !mod_doc.is_empty() {
        for line in mod_doc.lines() {
            s.push_str(&format!("--- {line}\n"));
        }
    }
    s.push_str(&format!("---@class {module}\n"));
    let local = if module == "smelt" {
        "smelt".into()
    } else {
        local_name(module)
    };
    s.push_str(&format!("local {local} = {{}}\n\n"));
    for f in fns {
        if f.name.starts_with("__") {
            continue; // skip metatable hooks
        }
        let plain_doc = strip_markdown_links(f.doc);
        for line in plain_doc.lines() {
            s.push_str(&format!("--- {line}\n"));
        }
        // Emit ---@see for cross-references that look like fully-qualified names.
        for line in plain_doc.lines() {
            if line.contains("smelt.") {
                for word in line.split_whitespace() {
                    let w = word.trim_matches(|c: char| c.is_ascii_punctuation());
                    if w.starts_with("smelt.")
                        && w.matches('.').count() >= 2
                        && w.chars()
                            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
                    {
                        s.push_str(&format!("---@see {w}\n"));
                    }
                }
            }
        }
        s.push_str(&format!("---@type {}\n", clean_sig(&f.sig)));
        s.push_str(&format!("{local}.{} = nil\n\n", f.name));
    }
    s.push_str(&format!("return {local}\n"));
    s
}

fn render_markdown(module: &str, fns: &[&LuaFnMeta], types: &TypeIndex, mod_doc: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!("# `{module}`\n\n"));
    s.push_str(
        "<!-- Auto-generated by `cargo xtask gen-lua-docs`. \
Do not edit by hand. -->\n\n",
    );
    let tier = module_tier(fns);
    s.push_str(&format!(
        "**Tier:** `{}` — {}\n\n",
        tier.label(),
        tier.description()
    ));
    if !mod_doc.is_empty() {
        s.push_str(mod_doc);
        s.push_str("\n\n");
    }
    for f in fns {
        if f.name.starts_with("__") {
            continue; // skip metatable hooks
        }
        s.push_str(&format!("## `{module}.{}`\n\n", f.name));
        s.push_str("```lua\n");
        s.push_str(&clean_sig(&f.sig));
        s.push('\n');
        s.push_str("```\n\n");
        let referenced = types.linkify_signature(&clean_sig(&f.sig));
        if !referenced.is_empty() {
            s.push_str("Types: ");
            s.push_str(&referenced.join(", "));
            s.push_str("\n\n");
        }
        s.push_str(f.doc);
        s.push_str("\n\n");
    }
    s
}

/// Pick the dominant tier for a module — every binding within a
/// namespace lives in the same crate, so `host` and `ui_host` never
/// mix in practice. We assert that with `find` and fall back to the
/// first registration if somehow they did.
fn module_tier(fns: &[&LuaFnMeta]) -> Tier {
    fns.first().map(|f| f.tier).unwrap_or(Tier::Host)
}

fn render_index(
    by_mod: &BTreeMap<&str, Vec<&LuaFnMeta>>,
    n_classes: usize,
    n_aliases: usize,
) -> String {
    let mut s = String::new();
    s.push_str("# Lua API\n\n");
    s.push_str(
        "<!-- Auto-generated by `cargo xtask gen-lua-docs`. \
Do not edit by hand. -->\n\n",
    );
    s.push_str(
        "Reference for every namespace exposed under the global `smelt` table. \
Signatures are derived from the Rust closure's argument tuple and return type, \
so the LuaCATS-style annotation always matches what mlua actually marshals.\n\n",
    );
    s.push_str(&format!(
        "**Coverage:** {} namespace(s), {} function(s), {} class(es), {} alias(es).\n\n",
        by_mod.len(),
        by_mod.values().map(|v| v.len()).sum::<usize>(),
        n_classes,
        n_aliases,
    ));

    s.push_str("## IDE completion\n\n");
    s.push_str(
        "Stubs land in `runtime/lua/smelt/_meta/` (one file per namespace, plus \
`_types.lua` for shared records and aliases). Point lua-language-server at that \
directory — see [`runtime/.luarc.json`](https://github.com/leonardcser/smelt/blob/main/runtime/.luarc.json) \
for a working config.\n\n",
    );

    let mut by_tier: BTreeMap<&'static str, Vec<(&&str, &Vec<&LuaFnMeta>)>> = BTreeMap::new();
    for (module, fns) in by_mod {
        let label = module_tier(fns).label();
        by_tier.entry(label).or_default().push((module, fns));
    }
    for tier in [Tier::Host, Tier::UiHost] {
        let Some(rows) = by_tier.get(tier.label()) else {
            continue;
        };
        s.push_str(&format!("## {} namespaces\n\n", tier.label()));
        s.push_str(tier.description());
        s.push_str("\n\n");
        for (module, fns) in rows {
            let stem = module_file_stem(module);
            s.push_str(&format!(
                "- [`{module}`]({stem}.md) — {} function(s)\n",
                fns.len()
            ));
        }
        s.push('\n');
    }
    if n_classes + n_aliases > 0 {
        s.push_str("## Types\n\n");
        s.push_str("- [Classes & aliases](types.md) — typed opts records and string-literal unions referenced from the namespace pages.\n");
    }
    s
}

fn render_types_stub(classes: &[LuaClassDecl], aliases: &[LuaAliasDecl]) -> String {
    let mut s = String::new();
    s.push_str("---@meta\n\n");
    s.push_str("-- Auto-generated by `cargo xtask gen-lua-docs`.\n");
    s.push_str("-- Do not edit by hand; update the `#[derive(LuaOpts)]`/`#[derive(LuaAlias)]` site in Rust instead.\n\n");
    for c in classes {
        if !c.doc.is_empty() {
            for line in c.doc.lines() {
                s.push_str(&format!("--- {line}\n"));
            }
        }
        s.push_str(&format!("---@class {}\n", c.name));
        for f in &c.fields {
            // `[string]` marks an index signature (rest-key capture);
            // LuaCATS spells it `---@field [string] V` (no `?` suffix on
            // the name — the index is always optional by definition).
            if f.name == "[string]" {
                if f.doc.is_empty() {
                    s.push_str(&format!("---@field [string] {}\n", f.ty));
                } else {
                    s.push_str(&format!("---@field [string] {} {}\n", f.ty, f.doc));
                }
                continue;
            }
            let opt = if f.optional { "?" } else { "" };
            if f.doc.is_empty() {
                s.push_str(&format!("---@field {}{opt} {}\n", f.name, f.ty));
            } else {
                s.push_str(&format!("---@field {}{opt} {} {}\n", f.name, f.ty, f.doc));
            }
        }
        s.push('\n');
    }
    for a in aliases {
        if !a.doc.is_empty() {
            for line in a.doc.lines() {
                s.push_str(&format!("--- {line}\n"));
            }
        }
        let mut parts: Vec<String> = a.variants.iter().map(|v| format!("\"{v}\"")).collect();
        if a.open {
            // Open aliases admit any string at runtime; the literal
            // list is just an autocomplete hint. LuaLS treats `string`
            // as a superset and still suggests the literals.
            parts.insert(0, "string".into());
        }
        s.push_str(&format!("---@alias {} {}\n\n", a.name, parts.join("|")));
    }
    s
}

fn render_types_markdown(
    classes: &[LuaClassDecl],
    aliases: &[LuaAliasDecl],
    types: &TypeIndex,
) -> String {
    let mut s = String::new();
    s.push_str("# Types\n\n");
    s.push_str(
        "<!-- Auto-generated by `cargo xtask gen-lua-docs`. \
Do not edit by hand. -->\n\n",
    );
    s.push_str(
        "Records and string-literal unions referenced from the namespace pages. \
Generated from `#[derive(LuaOpts)]` and `#[derive(LuaAlias)]` sites in Rust.\n\n",
    );

    if !classes.is_empty() {
        s.push_str("## Classes\n\n");
        for c in classes {
            s.push_str(&format!("### `{}`\n\n", c.name));
            if !c.doc.is_empty() {
                s.push_str(c.doc);
                s.push_str("\n\n");
            }
            s.push_str("| Field | Type | Required | Description |\n");
            s.push_str("| --- | --- | --- | --- |\n");
            for f in &c.fields {
                let req = if f.optional { "" } else { "yes" };
                let doc = if f.doc.is_empty() { "" } else { f.doc };
                // Index signatures render with an explicit `[string]:`
                // prefix so the dynamic-key shape is obvious in the docs
                // table.
                let name_cell = if f.name == "[string]" {
                    "`[string]`".to_string()
                } else {
                    format!("`{}`", f.name)
                };
                s.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    name_cell,
                    types.linkify_type(&f.ty),
                    req,
                    doc
                ));
            }
            s.push('\n');
        }
    }

    if !aliases.is_empty() {
        s.push_str("## Aliases\n\n");
        for a in aliases {
            s.push_str(&format!("### `{}`\n\n", a.name));
            if !a.doc.is_empty() {
                s.push_str(a.doc);
                s.push_str("\n\n");
            }
            let union = a
                .variants
                .iter()
                .map(|v| format!("`\"{v}\"`"))
                .collect::<Vec<_>>()
                .join(" \\| ");
            if a.open {
                s.push_str(&format!(
                    "Open alias — accepts any `string`. Well-known names: {union}.\n\n"
                ));
            } else {
                s.push_str(&format!("Variants: {union}\n\n"));
            }
        }
    }

    s
}

const NAV_BEGIN: &str = "# >>> LUA API NAV (auto-generated by gen-lua-docs)";
const NAV_END: &str = "# <<< LUA API NAV";

fn sync_zensical_nav(
    zensical: &Path,
    by_mod: &BTreeMap<&str, Vec<&LuaFnMeta>>,
) -> std::io::Result<&'static str> {
    let Ok(existing) = std::fs::read_to_string(zensical) else {
        return Ok("skipped (zensical.toml not found)");
    };
    let Some(begin) = existing.find(NAV_BEGIN) else {
        return Ok("skipped (no `# >>> LUA API NAV` marker)");
    };
    let Some(end) = existing[begin..].find(NAV_END) else {
        return Ok("skipped (no `# <<< LUA API NAV` closing marker)");
    };
    let end_abs = begin + end + NAV_END.len();

    let mut block = String::new();
    block.push_str(NAV_BEGIN);
    block.push('\n');
    for module in by_mod.keys() {
        let stem = module_file_stem(module);
        block.push_str(&format!(
            "    {{ \"`{module}`\" = \"reference/api/{stem}.md\" }},\n"
        ));
    }
    block.push_str("    { \"Types\" = \"reference/api/types.md\" },\n");
    block.push_str("    ");
    block.push_str(NAV_END);

    let mut out = String::with_capacity(existing.len());
    out.push_str(&existing[..begin]);
    out.push_str(&block);
    out.push_str(&existing[end_abs..]);

    if out == existing {
        Ok("up-to-date")
    } else {
        std::fs::write(zensical, out)?;
        Ok("updated")
    }
}

fn clean_stale_files(
    dir: &Path,
    ext: &str,
    expected: &std::collections::BTreeSet<String>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some(ext) {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if !expected.contains(&stem) {
            eprintln!("removing stale file: {}", path.display());
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

fn repo_root() -> std::io::Result<PathBuf> {
    // Walk up from CWD looking for a `Cargo.toml` that declares a
    // workspace; that's the repo root. Cargo runs `cargo run -p tui
    // --bin gen-lua-docs` from any subdir, so don't assume CWD.
    let mut p = std::env::current_dir()?;
    loop {
        let candidate = p.join("Cargo.toml");
        if candidate.exists() {
            let txt = std::fs::read_to_string(&candidate).unwrap_or_default();
            if txt.contains("[workspace]") {
                return Ok(p);
            }
        }
        if !p.pop() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not locate workspace root from CWD",
            ));
        }
    }
}
