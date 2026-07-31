//! Walk every `LuaMod::fn_` registration and emit:
//!   - LuaCATS stubs to `runtime/lua/smelt/_meta/<module>.lua`
//!     (consumed by lua-language-server for IDE completion)
//!   - Markdown reference pages to `docs/docs/reference/api/<module>.md`
//!     plus an `index.md` overview (rendered by the docs site)
//!   - A zensical nav block between `# >>> LUA API NAV` and
//!     `# <<< LUA API NAV` markers in `docs/zensical.toml`, so adding
//!     a new module surfaces in the docs site without manual edits.
//!
//! Outputs combine `LuaFnMeta` entries pushed by `LuaMod::fn_` with LuaCATS
//! annotations in bundled Lua modules. Documentation stays next to each Rust
//! registration or Lua implementation and never drifts out of sync.
//!
//! Usage: `cargo xtask gen-lua-docs` from the repo root.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use smelt_core::lua::doc::{
    aliases_snapshot, classes_snapshot, modules_snapshot, snapshot, LuaFnMeta, Tier, Visibility,
};
use smelt_core::lua::lua_type::{LuaAliasDecl, LuaClassDecl, LuaClassField};
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
            format!("`{}`", ty.replace('|', "\\|"))
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
    // Register the full API surface and bundled bootstrap chunks without
    // spinning up the full runtime state. Registration populates the static
    // doc registry as a side effect.
    #[allow(clippy::io_other_error)]
    LuaRuntime::register_for_docs()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    let mut metas = snapshot();
    if metas.is_empty() {
        eprintln!("no Lua functions registered - has any module been migrated to LuaMod::fn_ yet?");
        std::process::exit(1);
    }

    // Bundled Lua chunks (every entry in `BOOTSTRAP_FILES`) are part
    // of the public API surface. Each file can contribute three doc
    // artifacts in one pass:
    //   * functions (smelt.sleep, smelt.dialog.open, smelt.cmd.picker, …)
    //   * `---@class smelt.X.Y` opts records
    //   * `---@alias smelt.X.Y …` string-literal unions
    // Rust-registered items always take precedence on collision so a
    // Lua-side reassignment (e.g. the `smelt.tools.register` wrap)
    // never clobbers the canonical doc.
    let runtime_root = repo_root()?.join("runtime/lua/smelt");
    let mut registered_fns: std::collections::HashSet<(&str, &str)> =
        metas.iter().map(|m| (m.module, m.name)).collect();
    let mut bundled_classes: Vec<LuaClassDecl> = Vec::new();
    let mut bundled_aliases: Vec<LuaAliasDecl> = Vec::new();
    for rel in smelt_core::lua::runtime::BOOTSTRAP_FILES {
        let path = runtime_root.join(rel);
        if !path.is_file() {
            continue;
        }
        let content = std::fs::read_to_string(&path)?;
        let tier = if smelt_core::lua::runtime::UI_BOOTSTRAP_FILES.contains(rel) {
            Tier::UiHost
        } else {
            Tier::Host
        };
        let items = parse_bundled_lua(&content, tier);
        for w in items.warnings {
            eprintln!("warning: {rel}:{w}");
        }
        for mut parsed in items.fns {
            parsed.tier = bundled_lua_fn_tier(tier, parsed.module, parsed.name);
            if registered_fns.contains(&(parsed.module, parsed.name)) {
                continue;
            }
            registered_fns.insert((parsed.module, parsed.name));
            metas.push(parsed);
        }
        bundled_classes.extend(items.classes);
        bundled_aliases.extend(items.aliases);
    }

    let mut by_mod: BTreeMap<&str, Vec<&LuaFnMeta>> = BTreeMap::new();
    for m in &metas {
        // Private API: names starting with `__` are not surfaced in
        // generated stubs / reference docs. They stay callable from
        // bundled lua (which can reference them by literal name).
        if m.name.starts_with("__") {
            continue;
        }
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

    // Merge bundled-Lua class/alias declarations with the Rust-registered
    // ones; Rust wins on collision (canonical type stays put).
    let mut classes = classes_snapshot();
    let registered_class_names: std::collections::HashSet<&str> =
        classes.iter().map(|c| c.name).collect();
    for c in bundled_classes {
        if !registered_class_names.contains(c.name) {
            classes.push(c);
        }
    }
    classes.sort_by(|a, b| a.name.cmp(b.name));

    let mut aliases = aliases_snapshot();
    let registered_alias_names: std::collections::HashSet<&str> =
        aliases.iter().map(|a| a.name).collect();
    for a in bundled_aliases {
        if !registered_alias_names.contains(a.name) {
            aliases.push(a);
        }
    }
    aliases.sort_by(|a, b| a.name.cmp(b.name));

    let type_index = TypeIndex::new(&classes, &aliases);
    let mod_docs: BTreeMap<&str, (&str, Option<Tier>, Visibility)> = modules_snapshot()
        .into_iter()
        .map(|m| (m.module, (m.doc, m.tier, m.visibility)))
        .collect();

    let mut expected_stems: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    expected_stems.insert("_types".into());
    expected_stems.insert("index".into());
    expected_stems.insert("types".into());

    // Doc-only modules (every fn is `__`-prefixed and filtered out)
    // still appear in every generated surface so callers can discover the
    // namespace and read its module doc.
    let mut all_modules: BTreeSet<&str> = by_mod.keys().copied().collect();
    all_modules.extend(mod_docs.keys().copied());
    all_modules.retain(|module| {
        (module.starts_with("smelt.") || *module == "smelt")
            && (by_mod.get(module).is_some_and(|fns| !fns.is_empty())
                || mod_docs
                    .get(module)
                    .is_some_and(|(doc, _, _)| !doc.is_empty()))
    });
    for module in &all_modules {
        let fns = by_mod.get(module).map(Vec::as_slice).unwrap_or_default();
        let (mod_doc, declared_tier, module_visibility) = mod_docs
            .get(module)
            .copied()
            .unwrap_or(("", None, Visibility::Public));
        if mod_doc.is_empty() {
            eprintln!("warning: module `{module}` has functions but no module doc; consider adding record_module_doc");
        }
        let tier = declared_tier
            .or_else(|| fns.first().map(|f| f.tier))
            .unwrap_or(Tier::Host);
        let file_stem = module_file_stem(module);
        expected_stems.insert(file_stem.clone());
        std::fs::write(
            stubs_dir.join(format!("{file_stem}.lua")),
            render_stub(module, fns, mod_doc, tier, module_visibility),
        )?;
        std::fs::write(
            md_dir.join(format!("{file_stem}.md")),
            render_markdown(module, fns, &type_index, mod_doc, tier, module_visibility),
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
        render_index(
            &all_modules,
            &by_mod,
            &mod_docs,
            classes.len(),
            aliases.len(),
        ),
    )?;
    let nav_status = sync_zensical_nav(&zensical, &all_modules)?;
    let skill_status = sync_customize_skill(&root, &all_modules, &by_mod, &mod_docs)?;
    let public_status = sync_public_docs(&root)?;

    let total_fns: usize = by_mod.values().map(|v| v.len()).sum();
    println!(
        "wrote {} module(s) ({} function(s)), {} class(es), {} alias(es) to:",
        all_modules.len(),
        total_fns,
        classes.len(),
        aliases.len(),
    );
    println!("  {}", stubs_dir.display());
    println!("  {} (+ index.md, types.md)", md_dir.display());
    println!("zensical.toml nav: {nav_status}");
    println!("runtime/skills/customize/SKILL.md: {skill_status}");
    println!("public settings reference: {}", public_status.0);
    println!("public plugin inventory: {}", public_status.1);
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

fn bundled_lua_fn_tier(default_tier: Tier, module: &str, name: &str) -> Tier {
    // `_bootstrap.lua` is Host-loaded because it also installs task/fs/state
    // helpers, but a few functions are guarded by UiHost-only namespaces.
    // Document them at the tier where they can actually be called.
    const UI_GUARDED_BOOTSTRAP_FNS: &[(&str, &str)] = &[
        ("smelt.model", "preferred"),
        ("smelt.notebook", "apply_edit_async"),
        ("smelt.notebook", "read_async"),
        ("smelt.notify", "scoped"),
        ("smelt.picker", "fuzzy"),
        ("smelt.theme", "use"),
    ];
    if UI_GUARDED_BOOTSTRAP_FNS.contains(&(module, name)) {
        Tier::UiHost
    } else {
        default_tier
    }
}

fn has_mixed_tiers(fns: &[&LuaFnMeta]) -> bool {
    fns.first()
        .is_some_and(|first| fns.iter().any(|f| f.tier != first.tier))
}

fn render_stub(
    module: &str,
    fns: &[&LuaFnMeta],
    mod_doc: &str,
    tier: Tier,
    module_visibility: Visibility,
) -> String {
    let mut s = String::new();
    s.push_str("---@meta\n\n");
    s.push_str("-- Auto-generated by `cargo xtask gen-lua-docs`.\n");
    s.push_str("-- Do not edit by hand; update the `LuaMod::fn_` call in Rust instead.\n\n");
    if !mod_doc.is_empty() {
        for line in mod_doc.lines() {
            s.push_str(&format!("--- {line}\n"));
        }
    }
    if module_visibility == Visibility::Internal {
        s.push_str(&format!(
            "--- Visibility: {} - {}\n",
            module_visibility.label(),
            module_visibility.description()
        ));
    }
    s.push_str(&format!("---@class {module}\n"));
    let local = if module == "smelt" {
        "smelt".into()
    } else {
        local_name(module)
    };
    s.push_str(&format!("local {local} = {{}}\n\n"));
    let mixed_tiers = has_mixed_tiers(fns);
    for f in fns {
        if f.name.starts_with("__") {
            continue; // skip metatable hooks
        }
        if mixed_tiers && f.tier != tier {
            s.push_str(&format!(
                "--- Tier: {} - {}\n",
                f.tier.label(),
                f.tier.description()
            ));
        }
        if f.visibility == Visibility::Internal && module_visibility != Visibility::Internal {
            s.push_str(&format!(
                "--- Visibility: {} - {}\n",
                f.visibility.label(),
                f.visibility.description()
            ));
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

fn render_markdown(
    module: &str,
    fns: &[&LuaFnMeta],
    types: &TypeIndex,
    mod_doc: &str,
    tier: Tier,
    module_visibility: Visibility,
) -> String {
    let mut s = String::new();
    s.push_str(&format!("# `{module}`\n\n"));
    s.push_str(
        "<!-- Auto-generated by `cargo xtask gen-lua-docs`. \
Do not edit by hand. -->\n\n",
    );
    let mixed_tiers = has_mixed_tiers(fns);
    if mixed_tiers {
        s.push_str("**Tier:** `Mixed` - Contains both Host and UiHost functions; each function below lists its exact tier.\n\n");
    } else {
        s.push_str(&format!(
            "**Tier:** `{}` - {}\n\n",
            tier.label(),
            tier.description()
        ));
    }
    s.push_str(&format!(
        "**Visibility:** `{}` - {}\n\n",
        module_visibility.label(),
        module_visibility.description()
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
        if mixed_tiers {
            s.push_str(&format!(
                "**Tier:** `{}` - {}\n\n",
                f.tier.label(),
                f.tier.description()
            ));
        }
        if f.visibility != module_visibility {
            s.push_str(&format!(
                "**Visibility:** `{}` - {}\n\n",
                f.visibility.label(),
                f.visibility.description()
            ));
        }
        s.push_str(f.doc);
        s.push_str("\n\n");
    }
    s
}

fn render_index(
    all_modules: &BTreeSet<&str>,
    by_mod: &BTreeMap<&str, Vec<&LuaFnMeta>>,
    mod_docs: &BTreeMap<&str, (&str, Option<Tier>, Visibility)>,
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
Rust-registered signatures come from the closure's argument and return types; \
bundled Lua signatures come from LuaCATS annotations beside the implementation.\n\n",
    );
    s.push_str(&format!(
        "**Coverage:** {} namespace(s), {} function(s), {} class(es), {} alias(es).\n\n",
        all_modules.len(),
        by_mod.values().map(|v| v.len()).sum::<usize>(),
        n_classes,
        n_aliases,
    ));

    s.push_str("Functions and namespaces marked `Internal` are implementation details for bundled Lua. They are documented for transparency, but user config and plugins should prefer public APIs.\n\n");

    s.push_str("## IDE completion\n\n");
    s.push_str(
        "Stubs land in `runtime/lua/smelt/_meta/` (one file per namespace, plus \
`_types.lua` for shared records and aliases). Point lua-language-server at that \
directory - see [`runtime/.luarc.json`](https://github.com/leonardcser/smelt/blob/main/runtime/.luarc.json) \
for a working config.\n\n",
    );

    let mut by_tier: BTreeMap<&'static str, Vec<&str>> = BTreeMap::new();
    for &module in all_modules {
        let fns = by_mod.get(module).map(Vec::as_slice).unwrap_or_default();
        let label = if has_mixed_tiers(fns) {
            "Mixed"
        } else {
            mod_docs
                .get(module)
                .and_then(|(_, t, _)| *t)
                .or_else(|| fns.first().map(|f| f.tier))
                .unwrap_or(Tier::Host)
                .label()
        };
        by_tier.entry(label).or_default().push(module);
    }
    for label in ["Host", "UiHost", "Mixed"] {
        let Some(rows) = by_tier.get(label) else {
            continue;
        };
        s.push_str(&format!("## {label} namespaces\n\n"));
        let description = match label {
            "Host" => Tier::Host.description(),
            "UiHost" => Tier::UiHost.description(),
            _ => "Contains both Host and UiHost functions; per-function pages list the exact tier.",
        };
        s.push_str(description);
        s.push_str("\n\n");
        for &module in rows {
            let fns = by_mod.get(module).map(Vec::as_slice).unwrap_or_default();
            let stem = module_file_stem(module);
            let module_visibility = mod_docs
                .get(module)
                .map(|(_, _, visibility)| *visibility)
                .unwrap_or(Visibility::Public);
            let internal_count = fns
                .iter()
                .filter(|f| f.visibility == Visibility::Internal)
                .count();
            let visibility_suffix = if module_visibility == Visibility::Internal {
                " - Internal namespace".to_string()
            } else if internal_count > 0 {
                format!(" - {internal_count} internal function(s)")
            } else {
                String::new()
            };
            s.push_str(&format!(
                "- [`{module}`]({stem}.md) - {} function(s){visibility_suffix}\n",
                fns.len()
            ));
        }
        s.push('\n');
    }
    if n_classes + n_aliases > 0 {
        s.push_str("## Types\n\n");
        s.push_str("- [Classes & aliases](types.md) - typed opts records and string-literal unions referenced from the namespace pages.\n");
    }
    s
}

fn render_types_stub(classes: &[LuaClassDecl], aliases: &[LuaAliasDecl]) -> String {
    let mut s = String::new();
    s.push_str("---@meta\n\n");
    s.push_str("-- Auto-generated by `cargo xtask gen-lua-docs`.\n");
    s.push_str("-- Do not edit by hand; update the `#[derive(LuaOpts)]` / `#[derive(LuaAlias)]` site in Rust, or the `---@class` / `---@alias` block in the matching bundled Lua module.\n\n");
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
            // the name - the index is always optional by definition).
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
Generated from `#[derive(LuaOpts)]` / `#[derive(LuaAlias)]` sites in Rust and \
from `---@class` / `---@alias` blocks in the bundled Lua modules.\n\n",
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
                    "Open alias - accepts any `string`. Well-known names: {union}.\n\n"
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

const SKILL_API_BEGIN: &str = "<!-- API_INDEX_BEGIN -->";
const SKILL_API_END: &str = "<!-- API_INDEX_END -->";
const SKILL_SETTINGS_BEGIN: &str = "<!-- SETTINGS_BEGIN -->";
const SKILL_SETTINGS_END: &str = "<!-- SETTINGS_END -->";
const SKILL_PLUGINS_BEGIN: &str = "<!-- PLUGINS_BEGIN -->";
const SKILL_PLUGINS_END: &str = "<!-- PLUGINS_END -->";
const PUBLIC_SETTINGS_BEGIN: &str = "<!-- SETTINGS_REFERENCE_BEGIN -->";
const PUBLIC_SETTINGS_END: &str = "<!-- SETTINGS_REFERENCE_END -->";
const PUBLIC_PLUGINS_BEGIN: &str = "<!-- BUNDLED_PLUGINS_BEGIN -->";
const PUBLIC_PLUGINS_END: &str = "<!-- BUNDLED_PLUGINS_END -->";

fn render_zensical_nav(all_modules: &BTreeSet<&str>) -> String {
    let mut block = String::new();
    block.push_str(NAV_BEGIN);
    block.push('\n');
    for module in all_modules {
        let stem = module_file_stem(module);
        block.push_str(&format!(
            "    {{ \"`{module}`\" = \"reference/api/{stem}.md\" }},\n"
        ));
    }
    block.push_str("    { \"Types\" = \"reference/api/types.md\" },\n");
    block.push_str("    ");
    block.push_str(NAV_END);
    block
}

fn sync_zensical_nav(
    zensical: &Path,
    all_modules: &BTreeSet<&str>,
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

    let block = render_zensical_nav(all_modules);

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

/// Rewrite the three auto-generated regions of the built-in `customize`
/// skill: API index, settings table, and bundled-plugin inventory. The
/// hand-authored prose around them stays untouched. Idempotent.
fn sync_customize_skill(
    root: &Path,
    all_modules: &BTreeSet<&str>,
    by_mod: &BTreeMap<&str, Vec<&LuaFnMeta>>,
    mod_docs: &BTreeMap<&str, (&str, Option<Tier>, Visibility)>,
) -> std::io::Result<&'static str> {
    let path = root.join("runtime/skills/customize/SKILL.md");
    let Ok(existing) = std::fs::read_to_string(&path) else {
        return Ok("skipped (SKILL.md not found)");
    };

    let mut out = existing.clone();

    let api_body = render_skill_api_index(all_modules, by_mod, mod_docs);
    out = match replace_region(&out, SKILL_API_BEGIN, SKILL_API_END, &api_body) {
        Some(s) => s,
        None => return Ok("skipped (no `<!-- API_INDEX_BEGIN -->` marker)"),
    };

    let settings_body = render_skill_settings();
    out = match replace_region(
        &out,
        SKILL_SETTINGS_BEGIN,
        SKILL_SETTINGS_END,
        &settings_body,
    ) {
        Some(s) => s,
        None => return Ok("skipped (no `<!-- SETTINGS_BEGIN -->` marker)"),
    };

    let plugins_body = render_skill_plugins(root);
    out = match replace_region(&out, SKILL_PLUGINS_BEGIN, SKILL_PLUGINS_END, &plugins_body) {
        Some(s) => s,
        None => return Ok("skipped (no `<!-- PLUGINS_BEGIN -->` marker)"),
    };

    if out == existing {
        Ok("up-to-date")
    } else {
        std::fs::write(&path, out)?;
        Ok("updated")
    }
}

/// Keep the public scalar-settings table and bundled-plugin inventory derived
/// from the same canonical sources as the built-in `customize` skill.
fn sync_public_docs(root: &Path) -> std::io::Result<(&'static str, &'static str)> {
    let settings = sync_generated_region(
        &root.join("docs/docs/reference/configuration.md"),
        PUBLIC_SETTINGS_BEGIN,
        PUBLIC_SETTINGS_END,
        &render_skill_settings(),
    )?;
    let plugins = sync_generated_region(
        &root.join("docs/docs/guide/plugins.md"),
        PUBLIC_PLUGINS_BEGIN,
        PUBLIC_PLUGINS_END,
        &render_skill_plugins(root),
    )?;
    Ok((settings, plugins))
}

fn sync_generated_region(
    path: &Path,
    begin: &str,
    end: &str,
    body: &str,
) -> std::io::Result<&'static str> {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Ok("skipped (file not found)");
    };
    let Some(out) = replace_region(&existing, begin, end, body) else {
        return Ok("skipped (markers not found)");
    };
    if out == existing {
        Ok("up-to-date")
    } else {
        std::fs::write(path, out)?;
        Ok("updated")
    }
}

/// Replace the content between `begin` and `end` markers (both kept) with
/// `body`, prefixed with the "auto-generated, do not edit" banner. Returns
/// `None` if either marker is missing.
fn replace_region(src: &str, begin: &str, end: &str, body: &str) -> Option<String> {
    let begin_pos = src.find(begin)?;
    let end_rel = src[begin_pos..].find(end)?;
    let end_pos = begin_pos + end_rel + end.len();

    let mut block = String::new();
    block.push_str(begin);
    block.push_str("\n<!-- This region is auto-generated by `cargo xtask gen-lua-docs`. Do not edit by hand. -->\n\n");
    block.push_str(body);
    block.push_str(end);

    let mut out = String::with_capacity(src.len() + block.len());
    out.push_str(&src[..begin_pos]);
    out.push_str(&block);
    out.push_str(&src[end_pos..]);
    Some(out)
}

/// Render the API index region: every module grouped by tier, each
/// function as a single bullet with `signature` + first-sentence doc.
/// Kept compact so the skill stays usable as an LLM context payload.
fn render_skill_api_index(
    all_modules: &BTreeSet<&str>,
    by_mod: &BTreeMap<&str, Vec<&LuaFnMeta>>,
    mod_docs: &BTreeMap<&str, (&str, Option<Tier>, Visibility)>,
) -> String {
    let mut by_tier: BTreeMap<&'static str, Vec<&str>> = BTreeMap::new();
    for &module in all_modules {
        let fns = by_mod.get(module).map(Vec::as_slice).unwrap_or_default();
        let label = if has_mixed_tiers(fns) {
            "Mixed"
        } else {
            mod_docs
                .get(module)
                .and_then(|(_, t, _)| *t)
                .or_else(|| fns.first().map(|f| f.tier))
                .unwrap_or(Tier::Host)
                .label()
        };
        by_tier.entry(label).or_default().push(module);
    }

    let mut s = String::new();
    for label in ["Host", "UiHost", "Mixed"] {
        let Some(rows) = by_tier.get(label) else {
            continue;
        };
        s.push_str(&format!("### {label} tier\n\n"));
        let description = match label {
            "Host" => Tier::Host.description(),
            "UiHost" => Tier::UiHost.description(),
            _ => {
                "Contains both Host and UiHost functions; each function below lists its exact tier."
            }
        };
        s.push_str(description);
        s.push_str("\n\n");
        for &module in rows {
            let fns = by_mod.get(module).map(Vec::as_slice).unwrap_or_default();
            s.push_str(&format!("#### `{module}`\n\n"));
            if let Some((doc, _, _)) = mod_docs.get(module) {
                if !doc.is_empty() {
                    s.push_str(&first_sentence(doc));
                    s.push_str("\n\n");
                }
            }
            for f in fns {
                if f.name.starts_with("__") || f.visibility == Visibility::Internal {
                    continue;
                }
                let sig = clean_sig(&f.sig);
                let summary = first_sentence(f.doc);
                let tier_suffix = if label == "Mixed" {
                    format!(" ({})", f.tier.label())
                } else {
                    String::new()
                };
                s.push_str(&format!(
                    "- `{module}.{name}`{tier_suffix} :: `{sig}`\n",
                    name = f.name,
                ));
                if !summary.is_empty() {
                    s.push_str(&format!("  {summary}\n"));
                }
            }
            s.push('\n');
        }
    }
    s
}

/// Render the Settings region: every `smelt.settings.<key>` slot with
/// type, default value, and a one-line description, all derived from the
/// canonical [`smelt_core::config::SETTINGS`] schema. The table is the
/// single source of truth shared with the Lua `__pairs` iterator, public
/// configuration reference, and `customize` skill.
fn render_skill_settings() -> String {
    use smelt_core::config::{ResolvedSettings, SettingKind, SettingValue, SETTINGS};

    let defaults = ResolvedSettings::default();

    let mut s = String::new();
    s.push_str("Read or write via `smelt.settings.<key>` from `init.lua`. ");
    s.push_str("Saved Lua config reloads automatically by default; run `/reload` to apply changes manually. ");
    s.push_str("Override from the CLI with `--set KEY=VALUE`. ");
    s.push_str("Assigning an unknown key or wrong type raises at the call site.\n\n");

    s.push_str("| Key | Type | Default | Description |\n");
    s.push_str("| --- | --- | --- | --- |\n");
    for decl in SETTINGS {
        let ty = match decl.kind {
            SettingKind::Bool => "boolean".to_string(),
            SettingKind::Number => "number".to_string(),
            SettingKind::String => match decl.choices {
                Some(cs) => cs
                    .iter()
                    .map(|c| format!("`\"{c}\"`"))
                    .collect::<Vec<_>>()
                    .join(" \\| "),
                None => "string".to_string(),
            },
        };
        let default = match (decl.read)(&defaults) {
            SettingValue::Bool(b) => format!("`{b}`"),
            SettingValue::Number(n) => format!("`{n}`"),
            SettingValue::String(s) => format!("`\"{s}\"`"),
        };
        let doc_md = decl.doc.split_whitespace().collect::<Vec<_>>().join(" ");
        let ty_cell = if matches!(decl.kind, SettingKind::String) {
            ty // already markdown (backticked choices joined with `\|`)
        } else {
            format!("`{ty}`")
        };
        s.push_str(&format!(
            "| `{key}` | {ty_cell} | {default} | {doc_md} |\n",
            key = decl.key
        ));
    }
    s.push('\n');
    s
}

/// Render the Bundled plugins region: two tables (autoloaded vs opt-in)
/// derived from a filesystem scan of `runtime/lua/smelt/plugins/` plus
/// the [`smelt_core::lua::OPTIONAL_PLUGINS`] const that gates which
/// modules are auto-required at startup. Each row's summary is the first
/// contiguous comment block at the top of the plugin file.
fn render_skill_plugins(root: &Path) -> String {
    use smelt_core::lua::OPTIONAL_PLUGINS;

    let plugins_dir = root.join("runtime/lua/smelt/plugins");
    let opt_in: std::collections::HashSet<&str> = OPTIONAL_PLUGINS
        .iter()
        .map(|n| n.trim_start_matches("smelt.plugins."))
        .collect();

    let mut entries: Vec<(String, String)> = Vec::new();
    if let Ok(read) = std::fs::read_dir(&plugins_dir) {
        for ent in read.flatten() {
            let path = ent.path();
            if path.extension().and_then(|s| s.to_str()) != Some("lua") {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let summary = leading_comment_summary(&text);
            entries.push((stem, summary));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let (auto, optional): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .partition(|(stem, _)| !opt_in.contains(stem.as_str()));

    let mut s = String::new();
    s.push_str(
        "Bundled with smelt. Drop a file under `~/.config/smelt/plugins/` to add your own.\n\n",
    );

    s.push_str("### Autoloaded\n\n");
    s.push_str("Loaded on every launch unless opted out via `smelt.builtins.disable({ plugins = { \"<name>\" } })` in `early.lua`.\n\n");
    if auto.is_empty() {
        s.push_str("_(none)_\n\n");
    } else {
        s.push_str("| Plugin | Summary |\n| --- | --- |\n");
        for (stem, summary) in &auto {
            s.push_str(&format!("| `smelt.plugins.{stem}` | {summary} |\n"));
        }
        s.push('\n');
    }

    s.push_str("### Opt-in\n\n");
    s.push_str("Shipped but not autoloaded. Add `require(\"smelt.plugins.<name>\")` to `~/.config/smelt/init.lua` to enable.\n\n");
    if optional.is_empty() {
        s.push_str("_(none)_\n\n");
    } else {
        s.push_str("| Plugin | Summary |\n| --- | --- |\n");
        for (stem, summary) in &optional {
            s.push_str(&format!("| `smelt.plugins.{stem}` | {summary} |\n"));
        }
        s.push('\n');
    }
    s
}

/// Return the first contiguous `--`-prefixed comment block at the top of
/// a Lua file, collapsed to a single line. Stops at the first blank or
/// non-comment line.
fn leading_comment_summary(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if let Some(rest) = trimmed.strip_prefix("--") {
            let body = rest.strip_prefix(' ').unwrap_or(rest);
            if body.trim().is_empty() {
                break;
            }
            lines.push(body.trim().to_string());
        } else {
            break;
        }
    }
    let joined = lines.join(" ");
    first_sentence(&joined)
}

/// Compress a multi-line doc string down to a single short summary
/// suitable for a bullet point. Takes the first sentence (terminator
/// followed by space + uppercase, to avoid cutting mid-`e.g.`) of the
/// first non-empty paragraph and collapses internal whitespace.
fn first_sentence(doc: &str) -> String {
    let para = doc
        .split("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty())
        .unwrap_or("");
    let flat: String = para.split_whitespace().collect::<Vec<_>>().join(" ");
    let bytes = flat.as_bytes();
    let mut end = flat.len();
    let mut i = 0;
    while i + 2 < bytes.len() {
        let c = bytes[i];
        let next = bytes[i + 1];
        let after = bytes[i + 2];
        if (c == b'.' || c == b'!' || c == b'?')
            && next == b' '
            && (after.is_ascii_uppercase() || after == b'`')
        {
            end = i + 1;
            break;
        }
        i += 1;
    }
    flat[..end].to_string()
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

/// Artifacts harvested from one bundled Lua chunk. Every doc surface
/// the renderer knows about - functions, opts classes, string-literal
/// aliases - is parsed in a single pass so adding a new bundled module
/// is "drop the file into a bootstrap chunk" with no Rust ceremony.
/// `warnings` carries human-readable diagnostics for malformed LuaCATS
/// blocks (bad fields, dangling `@field` outside a class, …) so the
/// caller can surface them at gen time instead of silently dropping
/// the bad annotation.
#[derive(Default)]
struct BundledItems {
    fns: Vec<LuaFnMeta>,
    classes: Vec<LuaClassDecl>,
    aliases: Vec<LuaAliasDecl>,
    warnings: Vec<String>,
}

/// Walk a bundled Lua file once and emit every doc-bearing surface in
/// it. Recognises three styles of annotation:
///
///   * `function smelt.X.Y(args)` / `smelt.X.Y = function(args)` →
///     [`LuaFnMeta`]. Doc lines are the contiguous `--` or `---` block
///     immediately above. A trailing `---@type fun(...): T` directive
///     overrides the inferred signature; otherwise we fall back to
///     `fun(<args>: any...): any`.
///   * `---@class smelt.X.Y` followed by zero or more `---@field …`
///     lines → [`LuaClassDecl`]. The preceding `---` doc lines become
///     the class doc.
///   * `---@alias smelt.X.Y T|"a"|"b"` → [`LuaAliasDecl`]. Bare
///     `string` as the first member marks an open alias; the remaining
///     `"literal"` members become the well-known names.
///
/// Names starting with `_` are skipped (internal helpers like
/// `__smelt_state__` or `__smelt_raw_*`).
fn parse_bundled_lua(content: &str, tier: Tier) -> BundledItems {
    let mut out = BundledItems::default();
    let mut doc_buf: Vec<String> = Vec::new();
    let mut sig_override: Option<String> = None;
    let mut visibility = Visibility::Public;
    let mut active_class: Option<(&'static str, &'static str, Vec<LuaClassField>)> = None;

    let flush_doc = |doc_buf: &[String]| -> String {
        doc_buf
            .iter()
            .map(|s| s.trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    };

    for (lineno0, raw) in content.lines().enumerate() {
        let lineno = lineno0 + 1;
        let trimmed = raw.trim_start();

        // LuaCATS directives (`---@…`). Must be checked before the
        // generic `---` / `--` doc-line rules so `---@class` doesn't
        // get accumulated as a literal doc line.
        if let Some(rest) = trimmed.strip_prefix("---@") {
            match parse_cats_directive(rest) {
                CatsDirective::Class { name } => {
                    if !name.starts_with("smelt.") {
                        out.warnings.push(format!(
                            "line {lineno}: `---@class {name}` ignored (must start with `smelt.`)"
                        ));
                    } else {
                        flush_active_class(&mut active_class, &mut out.classes);
                        let doc = leak(flush_doc(&doc_buf));
                        active_class = Some((leak(name), doc, Vec::new()));
                        doc_buf.clear();
                        sig_override = None;
                    }
                }
                CatsDirective::Field(Some(field)) => {
                    if let Some((_, _, ref mut fields)) = active_class {
                        fields.push(field);
                    } else {
                        out.warnings.push(format!(
                            "line {lineno}: `---@field` outside any `---@class` block - dropped"
                        ));
                    }
                    // Stay in class-accumulation mode; don't clobber doc_buf.
                }
                CatsDirective::Field(None) => {
                    out.warnings.push(format!(
                        "line {lineno}: malformed `---@field` (expected `name[?] type [description]`)"
                    ));
                }
                CatsDirective::Alias(Some(decl)) => {
                    flush_active_class(&mut active_class, &mut out.classes);
                    out.aliases.push(LuaAliasDecl {
                        doc: leak(flush_doc(&doc_buf)),
                        ..decl
                    });
                    doc_buf.clear();
                    sig_override = None;
                }
                CatsDirective::Alias(None) => {
                    out.warnings.push(format!(
                        "line {lineno}: malformed `---@alias` (expected `smelt.X.Y T1|T2|\"lit\"|…`)"
                    ));
                }
                CatsDirective::Type { sig } => {
                    sig_override = Some(sig);
                }
                CatsDirective::Internal => {
                    visibility = Visibility::Internal;
                }
                CatsDirective::Ignored => {
                    // `---@meta`, `---@see`, etc. - pass through silently.
                }
            }
            continue;
        }
        // Leaving any `---@field` streak closes the active class.
        if active_class.is_some() && !trimmed.is_empty() && !trimmed.starts_with("---@field") {
            flush_active_class(&mut active_class, &mut out.classes);
        }

        // Both `---` (LuaCATS) and `--` (plain Lua) feed the same doc
        // accumulator. `---` wins by being checked first via the
        // directive arm above, so any non-directive `---` falls through.
        if let Some(rest) = trimmed.strip_prefix("---") {
            let text = rest.strip_prefix(' ').unwrap_or(rest);
            doc_buf.push(text.to_string());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("--") {
            let text = rest.strip_prefix(' ').unwrap_or(rest);
            doc_buf.push(text.to_string());
            continue;
        }
        if let Some((full_name, args)) = parse_lua_fn_decl(raw) {
            let Some((module, name)) = split_module_and_name(&full_name) else {
                doc_buf.clear();
                sig_override = None;
                visibility = Visibility::Public;
                continue;
            };
            if name.starts_with('_') {
                doc_buf.clear();
                sig_override = None;
                visibility = Visibility::Public;
                continue;
            }
            let doc = flush_doc(&doc_buf);
            let sig = sig_override.clone().unwrap_or_else(|| default_sig(&args));
            out.fns.push(LuaFnMeta {
                module: leak(module),
                name: leak(name),
                doc: leak(doc),
                sig,
                tier,
                visibility,
            });
            doc_buf.clear();
            sig_override = None;
            visibility = Visibility::Public;
            continue;
        }
        if !trimmed.is_empty() {
            doc_buf.clear();
            sig_override = None;
            visibility = Visibility::Public;
        }
    }

    // Flush a trailing class block (file ended with `---@field` lines).
    flush_active_class(&mut active_class, &mut out.classes);

    out
}

/// Move a built-up class block (name + accumulated fields + leading
/// doc) into the bundled-classes vec. Used at every `---@class` /
/// `---@alias` boundary and once at end-of-file so the renderer always
/// sees fully-formed classes.
fn flush_active_class(
    active: &mut Option<(&'static str, &'static str, Vec<LuaClassField>)>,
    out: &mut Vec<LuaClassDecl>,
) {
    if let Some((name, doc, fields)) = active.take() {
        out.push(LuaClassDecl { name, doc, fields });
    }
}

/// Promote an owned `String` to `&'static str` for the doc registry's
/// borrow contract. The whole gen-lua-docs run is short-lived (≪1s),
/// so leaking the handful of bundled-Lua-derived names + docs is
/// cheaper than threading lifetimes through every render call.
fn leak(s: impl Into<String>) -> &'static str {
    Box::leak(s.into().into_boxed_str())
}

/// LuaCATS directive recognised by [`parse_bundled_lua`]. `Field` and
/// `Alias` wrap an `Option` so the caller can warn on malformed bodies
/// without dropping back into the "is this even a directive?" check -
/// every recognised keyword produces exactly one variant, and the
/// fallback `Ignored` covers `---@meta`, `---@see`, `---@return`, …
/// which we deliberately don't act on (LuaCATS keeps their meaning
/// for the LSP without our help).
enum CatsDirective {
    Class { name: String },
    Field(Option<LuaClassField>),
    Alias(Option<LuaAliasDecl>),
    Type { sig: String },
    Internal,
    Ignored,
}

fn parse_cats_directive(rest: &str) -> CatsDirective {
    let rest = rest.trim_start();
    if let Some(tail) = strip_keyword(rest, "class") {
        let name = tail
            .split([':', ' ', '\t'])
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        return CatsDirective::Class { name };
    }
    if let Some(tail) = strip_keyword(rest, "field") {
        return CatsDirective::Field(parse_cats_field(tail));
    }
    if let Some(tail) = strip_keyword(rest, "alias") {
        return CatsDirective::Alias(parse_cats_alias(tail));
    }
    if let Some(tail) = strip_keyword(rest, "type") {
        return CatsDirective::Type {
            sig: tail.trim().to_string(),
        };
    }
    if rest == "internal" {
        return CatsDirective::Internal;
    }
    CatsDirective::Ignored
}

/// Strip a `keyword` followed by ASCII whitespace from the front of
/// `rest`. Matches the `---@<kw> body` shape without conflating
/// `---@class` with `---@classified` (which is silly but cheap to
/// guard against).
fn strip_keyword<'a>(rest: &'a str, keyword: &str) -> Option<&'a str> {
    let tail = rest.strip_prefix(keyword)?;
    let next = tail.chars().next()?;
    if next.is_ascii_whitespace() {
        Some(&tail[next.len_utf8()..])
    } else {
        None
    }
}

/// Parse the body of `---@field NAME[?] TYPE [doc...]`. The type can
/// contain spaces inside balanced delimiters (`fun(x: any): any`,
/// `table<string, integer>`, `{ a: integer, b: string }`); the parser
/// tracks `()`/`[]`/`<>`/`{}` depth so `description text` after the
/// type is split off cleanly.
fn parse_cats_field(body: &str) -> Option<LuaClassField> {
    let body = body.trim_start();
    if body.is_empty() {
        return None;
    }
    let (raw_name, after_name) = split_token(body);
    if raw_name.is_empty() {
        return None;
    }
    let (name, optional) = if let Some(stripped) = raw_name.strip_suffix('?') {
        (stripped.to_string(), true)
    } else {
        (raw_name.to_string(), false)
    };
    let after_name = after_name.trim_start();
    if after_name.is_empty() {
        return None;
    }
    let (ty, doc) = split_type_and_doc(after_name);
    Some(LuaClassField {
        name: leak(name),
        ty,
        optional,
        doc: leak(doc),
    })
}

/// Greedy whitespace-aware tokenizer: returns `(token, rest)` where
/// `token` is everything up to the first ASCII whitespace and `rest`
/// is the remainder (leading whitespace stripped). Used to peel the
/// field name off a `---@field` body.
fn split_token(s: &str) -> (&str, &str) {
    let end = s.find(|c: char| c.is_ascii_whitespace()).unwrap_or(s.len());
    (&s[..end], s[end..].trim_start())
}

/// Split `<type> <description>` while respecting the LuaCATS depth
/// rules. Spaces inside `()`/`[]`/`<>`/`{}`, after a function return
/// colon, or around a union operator belong to the type.
fn split_type_and_doc(s: &str) -> (String, String) {
    let bytes = s.as_bytes();
    let mut depth: i32 = 0;
    let mut end = bytes.len();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'<' | b'{' => depth += 1,
            b')' | b']' | b'>' | b'}' => depth = depth.saturating_sub(1),
            b' ' | b'\t' if depth == 0 => {
                let mut next = i;
                while next < bytes.len() && matches!(bytes[next], b' ' | b'\t') {
                    next += 1;
                }
                let previous = s[..i].trim_end().as_bytes().last().copied();
                let following = bytes.get(next).copied();
                if matches!(previous, Some(b':' | b'|')) || following == Some(b'|') {
                    i = next;
                    continue;
                }
                end = i;
                break;
            }
            _ => {}
        }
        i += 1;
    }
    let ty = s[..end].trim().to_string();
    let doc = s[end..].trim().to_string();
    (ty, doc)
}

/// Parse `---@alias NAME T1|T2|"literal"|…`. A leading `string|` marks
/// an open alias; the remaining `"…"` members are stored as the
/// well-known literal set.
fn parse_cats_alias(body: &str) -> Option<LuaAliasDecl> {
    let body = body.trim_start();
    let (raw_name, rest) = split_token(body);
    if !raw_name.starts_with("smelt.") {
        return None;
    }
    let mut variants: Vec<&'static str> = Vec::new();
    let mut open = false;
    for part in rest.split('|') {
        let p = part.trim();
        if p == "string" {
            open = true;
            continue;
        }
        if let Some(lit) = p.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            variants.push(leak(lit));
        }
    }
    Some(LuaAliasDecl {
        name: leak(raw_name),
        doc: "",
        variants,
        open,
    })
}

fn parse_lua_fn_decl(line: &str) -> Option<(String, String)> {
    let line = line.trim_start();
    // Form 1: `function smelt.X.Y(args)`
    if let Some(rest) = line.strip_prefix("function ") {
        let paren = rest.find('(')?;
        let name = rest[..paren].trim().to_string();
        if !name.starts_with("smelt.") {
            return None;
        }
        let close = rest.rfind(')')?;
        if close <= paren {
            return None;
        }
        let args = rest[paren + 1..close].trim().to_string();
        return Some((name, args));
    }
    // Form 2: `smelt.X.Y = function(args)`
    if !line.starts_with("smelt.") {
        return None;
    }
    let eq = line.find('=')?;
    let lhs = line[..eq].trim();
    let rhs = line[eq + 1..].trim_start();
    let after_fn = rhs.strip_prefix("function")?;
    if !after_fn.starts_with('(') {
        return None;
    }
    let close = after_fn.rfind(')')?;
    let args = after_fn[1..close].trim().to_string();
    Some((lhs.to_string(), args))
}

fn split_module_and_name(full: &str) -> Option<(String, String)> {
    let idx = full.rfind('.')?;
    let module = full[..idx].to_string();
    let name = full[idx + 1..].to_string();
    if name.is_empty() {
        return None;
    }
    Some((module, name))
}

fn default_sig(args: &str) -> String {
    let typed = args
        .split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| format!("{p}: any"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("fun({typed}): any")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_type_and_doc_respects_balanced_delimiters() {
        assert_eq!(
            split_type_and_doc("table<string, string> Optional map."),
            (
                "table<string, string>".to_string(),
                "Optional map.".to_string()
            )
        );
        assert_eq!(
            split_type_and_doc("fun(value: string, opts?: { strict: boolean }): nil Callback."),
            (
                "fun(value: string, opts?: { strict: boolean }): nil".to_string(),
                "Callback.".to_string()
            )
        );
    }

    #[test]
    fn split_type_and_doc_keeps_required_callback_return_type() {
        assert_eq!(
            split_type_and_doc(
                "fun(tool: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node Complete replacement renderer."
            ),
            (
                "fun(tool: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node".to_string(),
                "Complete replacement renderer.".to_string()
            )
        );
    }

    #[test]
    fn split_type_and_doc_keeps_optional_callback_return_type() {
        assert_eq!(
            split_type_and_doc(
                "fun(tool: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node | nil Optional renderer."
            ),
            (
                "fun(tool: smelt.transcript.Block, ctx: smelt.transcript.Context): smelt.layout.Node | nil".to_string(),
                "Optional renderer.".to_string()
            )
        );
    }

    #[test]
    fn linkify_type_escapes_markdown_table_pipes() {
        let index = TypeIndex::new(&[], &[]);
        assert_eq!(
            index.linkify_type("string|smelt.layout.Node|nil"),
            "`string\\|smelt.layout.Node\\|nil`"
        );
    }

    #[test]
    fn generated_region_replacement_preserves_surrounding_prose() {
        let source = "before\n<!-- START -->\nstale\n<!-- END -->\nafter\n";
        let replaced = replace_region(source, "<!-- START -->", "<!-- END -->", "fresh\n")
            .expect("markers should be replaced");

        assert!(replaced.starts_with("before\n<!-- START -->"));
        assert!(replaced.contains("fresh\n<!-- END -->"));
        assert!(replaced.ends_with("after\n"));
        assert!(!replaced.contains("stale"));
    }

    #[test]
    fn doc_only_module_appears_in_generated_indexes_and_navigation() {
        let all_modules = BTreeSet::from(["smelt.build"]);
        let by_mod: BTreeMap<&str, Vec<&LuaFnMeta>> = BTreeMap::new();
        let mod_docs = BTreeMap::from([(
            "smelt.build",
            (
                "Build metadata exposed to plugins.",
                Some(Tier::Host),
                Visibility::Public,
            ),
        )]);

        let index = render_index(&all_modules, &by_mod, &mod_docs, 0, 0);
        assert!(index.contains("**Coverage:** 1 namespace(s), 0 function(s)"));
        assert!(index.contains("[`smelt.build`](build.md) - 0 function(s)"));

        let skill_index = render_skill_api_index(&all_modules, &by_mod, &mod_docs);
        assert!(skill_index.contains("#### `smelt.build`"));
        assert!(skill_index.contains("Build metadata exposed to plugins."));

        let nav = render_zensical_nav(&all_modules);
        assert!(nav.contains("{ \"`smelt.build`\" = \"reference/api/build.md\" }"));
    }

    #[test]
    fn generated_settings_include_every_scalar_schema_key() {
        let rendered = render_skill_settings();

        for declaration in smelt_core::config::SETTINGS {
            assert!(
                rendered.contains(&format!("| `{}` |", declaration.key)),
                "missing setting {}",
                declaration.key
            );
        }
    }
}
