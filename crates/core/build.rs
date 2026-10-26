//! Precompiles the embedded `runtime/lua/smelt/` tree to Lua bytecode so
//! startup loads chunks instead of lexing and parsing them. The bytecode is
//! produced by the same vendored Lua the runtime links, with debug info kept
//! so tracebacks still carry file names and line numbers.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let root = manifest_dir.join("../../runtime/lua/smelt");
    println!("cargo:rerun-if-changed={}", root.display());
    println!("cargo:rerun-if-changed=build.rs");

    let mut files = Vec::new();
    collect_lua_files(&root, &root, &mut files);
    files.sort();

    let lua = mlua::Lua::new();
    let mut index = String::from("pub(crate) static EMBEDDED_BYTECODE: &[(&str, &[u8])] = &[\n");
    for rel in &files {
        let source = std::fs::read(root.join(rel)).unwrap();
        let bytecode = lua
            .load(&source)
            .set_name(format!("smelt/{rel}"))
            .into_function()
            .unwrap_or_else(|error| panic!("compile runtime/lua/smelt/{rel}: {error}"))
            .dump(false);
        let target = out_dir.join("bytecode").join(format!("{rel}c"));
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, bytecode).unwrap();
        writeln!(
            index,
            "    ({rel:?}, include_bytes!({:?})),",
            target.display().to_string()
        )
        .unwrap();
    }
    index.push_str("];\n");
    std::fs::write(out_dir.join("embedded_bytecode.rs"), index).unwrap();
}

/// Collects `.lua` files under `dir` as `/`-separated paths relative to `root`.
fn collect_lua_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_lua_files(root, &path, out);
        } else if path.extension().is_some_and(|ext| ext == "lua") {
            let rel = path.strip_prefix(root).unwrap();
            let parts: Vec<_> = rel.iter().map(|part| part.to_str().unwrap()).collect();
            out.push(parts.join("/"));
        }
    }
}
