//! `smelt.keymap` - register chord→callback and expose `keymap.help`.
//! Chords and modes are canonicalized at registration; unknown values raise immediately.

use crate::lua::{LuaHandle, LuaShared};
use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::lua_type::LuaCallback;
use smelt_core::lua::module::LuaMod;
use smelt_core::lua::reg::LuaReg;
use smelt_core::lua::RegisteredKeymap;
use std::sync::Arc;

fn default_leader() -> String {
    "\\".to_string()
}

fn current_leader(shared: &Arc<LuaShared>) -> String {
    shared
        .keymap_leader
        .lock()
        .map(|leader| leader.clone())
        .unwrap_or_else(|_| default_leader())
}

fn canonicalize_registered_chord(
    shared: &Arc<LuaShared>,
    api_name: &str,
    chord: &str,
) -> LuaResult<String> {
    let leader = current_leader(shared);
    crate::lua::canonicalize_chord_sequence_with_leader(chord, Some(&leader))
        .ok_or_else(|| LuaError::RuntimeError(format!("{api_name}: unknown chord `{chord}`")))
}

fn canonicalize_prefix_chord(
    shared: &Arc<LuaShared>,
    api_name: &str,
    chord: &str,
) -> LuaResult<String> {
    if chord.is_empty() {
        return Ok(String::new());
    }
    canonicalize_registered_chord(shared, api_name, chord)
}

fn current_query_mode() -> String {
    crate::lua::try_with_app(|app| {
        app.focused_vim_mode_label()
            .and_then(|mode| crate::lua::normalize_mode(&mode))
    })
    .flatten()
    .unwrap_or_else(|| "n".to_string())
}

fn mode_matches(binding_mode: &str, active_mode: &str) -> bool {
    binding_mode.is_empty() || binding_mode == active_mode
}

struct PrefixRow {
    mode: String,
    chord: String,
    suffix: String,
    description: Option<String>,
}

fn keymap_prefix_rows(shared: &Arc<LuaShared>, pending: &str, mode: &str) -> Vec<PrefixRow> {
    let mut rows: Vec<PrefixRow> = shared
        .keymaps
        .lock()
        .map(|map| {
            map.iter()
                .filter_map(|((row_mode, chord), entry)| {
                    if !mode_matches(row_mode, mode)
                        || chord.len() <= pending.len()
                        || !chord.starts_with(pending)
                    {
                        return None;
                    }
                    Some(PrefixRow {
                        mode: row_mode.clone(),
                        chord: chord.clone(),
                        suffix: chord[pending.len()..].to_string(),
                        description: entry.description.clone(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    rows.sort_by(|a, b| {
        a.chord
            .cmp(&b.chord)
            .then_with(|| a.mode.is_empty().cmp(&b.mode.is_empty()))
            .then_with(|| a.description.cmp(&b.description))
    });
    rows.dedup_by(|a, b| a.chord == b.chord);
    rows.sort_by(|a, b| {
        a.suffix
            .cmp(&b.suffix)
            .then_with(|| a.description.cmp(&b.description))
            .then_with(|| a.mode.cmp(&b.mode))
    });
    rows
}

pub(super) fn register(
    lua: &Lua,
    smelt_keymap: &mlua::Table,
    shared: &Arc<LuaShared>,
) -> LuaResult<()> {
    let m = LuaMod::own(
        lua,
        smelt_keymap.clone(),
        "smelt.keymap",
        "Register chord→callback bindings and inspect the layered help index. Chords and modes are canonicalized at registration; unknown values raise immediately. UiHost-only.",
        Tier::UiHost,
    );
    m.fn_(
        "help_sections",
        "Return layered keybinding help as `{ title, entries = { { label, detail } } }` rows. Filters vim-only chords when vim mode is disabled.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let vim_enabled =
                crate::lua::try_with_app(|app| app.input.vim_enabled(app.prompt_win()))
                    .unwrap_or(false);
            let sections = crate::keymap::hints::help_sections(vim_enabled);
            let out = lua.create_table()?;
            for (i, (title, entries)) in sections.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("title", title)?;
                let entries_tbl = lua.create_table()?;
                for (j, (label, detail)) in entries.into_iter().enumerate() {
                    let entry = lua.create_table()?;
                    entry.set("label", label)?;
                    entry.set("detail", detail)?;
                    entries_tbl.set(j + 1, entry)?;
                }
                row.set("entries", entries_tbl)?;
                out.set(i + 1, row)?;
            }
            Ok(out)
        },
    )?;
    smelt_keymap.set("help", smelt_keymap.get::<mlua::Function>("help_sections")?)?;

    {
        let s = shared.clone();
        m.fn_(
            "leader",
            "Return the current `<leader>` expansion used when registering keymaps. Defaults to a single backslash (`\\`).",
            &[],
            move |_, ()| -> LuaResult<String> {
                Ok(crate::lua::display_chord_sequence(&current_leader(&s)))
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "set_leader",
            "Set the `<leader>` expansion for subsequent keymap registrations. `leader` must be one canonicalizable key token, e.g. `<space>` or a single backslash (`\\`). Existing keymaps keep the expansion they were registered with.",
            &["leader"],
            move |_, leader: String| -> LuaResult<()> {
                let canonical = crate::lua::canonicalize_leader(&leader).ok_or_else(|| {
                    LuaError::RuntimeError(format!(
                        "keymap.set_leader: unknown leader `{leader}`"
                    ))
                })?;
                if let Ok(mut current) = s.keymap_leader.lock() {
                    *current = canonical;
                }
                Ok(())
            },
        )?;
    }

    {
        let s = shared.clone();
        m.fn_(
            "set",
            "Bind `chord` in `mode` to a Lua callback. `mode` is `\"n\"|\"i\"|\"v\"|\"\"` (or the long form `normal`/`insert`/`visual`); the chord is canonicalized at registration and unknown values raise immediately. `opts.desc` is an optional one-line description used by keybinding help UIs. Re-binding the same `(mode, chord)` overwrites the prior handler. Returns a `Reg` whose `:remove()` drops the binding.",
            &["mode", "chord", "handler", "opts"],
            move |lua,
                  (mode, chord, handler, opts): (
                String,
                String,
                LuaCallback<(), ()>,
                Option<mlua::Table>,
            )|
                  -> LuaResult<LuaReg> {
                let canonical_mode = crate::lua::normalize_mode(&mode).ok_or_else(
                    || {
                        LuaError::RuntimeError(format!(
                            "keymap.set: unknown mode `{mode}` (expected \"n\"|\"i\"|\"v\"|\"\" or \"normal\"|\"insert\"|\"visual\")"
                        ))
                    },
                )?;
                let canonical_chord = canonicalize_registered_chord(&s, "keymap.set", &chord)?;
                let description = match opts {
                    Some(t) => t.get::<Option<String>>("desc")?,
                    None => None,
                };
                let handle = LuaHandle::from_func(lua, handler.into_inner())?;
                if let Ok(mut map) = s.keymaps.lock() {
                    map.insert(
                        (canonical_mode.clone(), canonical_chord.clone()),
                        RegisteredKeymap {
                            handle,
                            description,
                        },
                    );
                }
                let s_for_reg = s.clone();
                Ok(LuaReg::new(move || {
                    s_for_reg
                        .keymaps
                        .lock()
                        .map(|mut m| m.remove(&(canonical_mode.clone(), canonical_chord.clone())).is_some())
                        .unwrap_or(false)
                }))
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "unset",
            "Drop the binding for `chord` in `mode`. `mode` accepts the same forms as `set`. Returns `true` if a binding was removed.",
            &["mode", "chord"],
            move |_, (mode, chord): (String, String)| -> LuaResult<bool> {
                let canonical_mode = crate::lua::normalize_mode(&mode).ok_or_else(|| {
                    LuaError::RuntimeError(format!(
                        "keymap.unset: unknown mode `{mode}` (expected \"n\"|\"i\"|\"v\"|\"\" or \"normal\"|\"insert\"|\"visual\")"
                    ))
                })?;
                let canonical_chord = canonicalize_registered_chord(&s, "keymap.unset", &chord)?;
                Ok(s.keymaps
                    .lock()
                    .map(|mut m| m.remove(&(canonical_mode, canonical_chord)).is_some())
                    .unwrap_or(false))
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "list",
            "Return the set of currently-bound `{ mode, chord, desc? }` rows. `mode` is the canonical short form (`\"n\"`/`\"i\"`/`\"v\"`/`\"\"`). `chord` is the display form after canonicalization and `<leader>` expansion.",
            &[],
            move |lua, ()| -> LuaResult<mlua::Table> {
                let mut rows: Vec<(String, String, Option<String>)> = s
                    .keymaps
                    .lock()
                    .map(|m| {
                        m.iter()
                            .map(|((mode, chord), entry)| {
                                (mode.clone(), chord.clone(), entry.description.clone())
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
                let out = lua.create_table()?;
                for (i, (mode, chord, desc)) in rows.into_iter().enumerate() {
                    let row = lua.create_table()?;
                    row.set("mode", mode)?;
                    row.set("chord", crate::lua::display_chord_sequence(&chord))?;
                    if let Some(desc) = desc {
                        row.set("desc", desc)?;
                    }
                    out.set(i + 1, row)?;
                }
                Ok(out)
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "prefixes",
            "Return effective keymaps that extend `pending` in `mode` as `{ mode, chord, suffix, desc? }` rows. `pending` may be empty to list top-level mappings, otherwise it is canonicalized with the current leader. `mode` accepts the same forms as `set`; when omitted, the focused Vim mode is used (or Normal outside an app). Mode-specific bindings shadow global bindings for the same chord.",
            &["pending", "mode"],
            move |lua, (pending, mode): (String, Option<String>)| -> LuaResult<mlua::Table> {
                let canonical_pending = canonicalize_prefix_chord(&s, "keymap.prefixes", &pending)?;
                let canonical_mode = match mode {
                    Some(mode) => crate::lua::normalize_mode(&mode).ok_or_else(|| {
                        LuaError::RuntimeError(format!(
                            "keymap.prefixes: unknown mode `{mode}` (expected \"n\"|\"i\"|\"v\"|\"\" or \"normal\"|\"insert\"|\"visual\")"
                        ))
                    })?,
                    None => current_query_mode(),
                };
                let rows = keymap_prefix_rows(&s, &canonical_pending, &canonical_mode);
                let out = lua.create_table()?;
                for (i, row) in rows.into_iter().enumerate() {
                    let t = lua.create_table()?;
                    t.set("mode", row.mode)?;
                    t.set("chord", crate::lua::display_chord_sequence(&row.chord))?;
                    t.set("suffix", crate::lua::display_chord_sequence(&row.suffix))?;
                    if let Some(desc) = row.description {
                        t.set("desc", desc)?;
                    }
                    out.set(i + 1, t)?;
                }
                Ok(out)
            },
        )?;
    }
    Ok(())
}
