//! `smelt.keymap` - global and scoped key bindings, keymap scopes and their
//! actions, and the layered help index. Chords and modes are canonicalized at
//! registration; unknown values raise immediately.

use crate::app::lua_bridge::ScopeAction;
use crate::lua::LuaHandle;
use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::keymaps::{ActionSpec, Binding, DefaultKey, Effective, ScopeSpec, Target};
use smelt_core::lua::module::LuaMod;
use smelt_core::lua::reg::LuaReg;
use smelt_core::lua::LuaShared;
use std::collections::HashMap;
use std::sync::Arc;

const MODES: &str = "\"n\"|\"i\"|\"v\"|\"\" or \"normal\"|\"insert\"|\"visual\"";

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

fn canonical_chord(shared: &Arc<LuaShared>, api_name: &str, chord: &str) -> LuaResult<String> {
    let leader = current_leader(shared);
    crate::lua::canonicalize_chord_sequence_with_leader(chord, Some(&leader))
        .ok_or_else(|| LuaError::RuntimeError(format!("{api_name}: unknown chord `{chord}`")))
}

fn canonical_mode(api_name: &str, mode: &str) -> LuaResult<String> {
    crate::lua::normalize_mode(mode).ok_or_else(|| {
        LuaError::RuntimeError(format!(
            "{api_name}: unknown mode `{mode}` (expected {MODES})"
        ))
    })
}

fn valid_scope(api_name: &str, scope: &str) -> LuaResult<()> {
    if scope.is_empty() {
        return Err(LuaError::RuntimeError(format!(
            "{api_name}: scope name must not be empty"
        )));
    }
    Ok(())
}

/// The scope named by `opts.scope`, or `""` for the global scope.
fn opts_scope(api_name: &str, opts: Option<&mlua::Table>) -> LuaResult<String> {
    let scope = match opts {
        Some(opts) => opts.get::<Option<String>>("scope")?,
        None => None,
    };
    match scope {
        Some(scope) => {
            valid_scope(api_name, &scope)?;
            Ok(scope)
        }
        None => Ok(String::new()),
    }
}

fn keymaps_mut<R>(
    shared: &Arc<LuaShared>,
    f: impl FnOnce(&mut smelt_core::lua::keymaps::Keymaps) -> R,
) -> Option<R> {
    shared.keymaps.lock().ok().map(|mut map| f(&mut map))
}

/// The chain of scopes around focus, or only the global scope outside an app.
fn focus_chain() -> Option<crate::app::lua_bridge::KeymapChain> {
    crate::lua::try_with_ui_host(|host| host.keymap_chain())
}

fn row_table(lua: &Lua, scope: &str, row: &Effective<'_>) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("scope", scope)?;
    t.set("mode", row.mode)?;
    t.set("chord", crate::lua::display_chord_sequence(row.chord))?;
    if let Some(action) = row.action {
        t.set("action", action)?;
    }
    if let Some(desc) = row.description {
        t.set("desc", desc)?;
    }
    t.set("source", row.source.name())?;
    Ok(t)
}

/// Parse a `smelt.keymap.scope` spec into actions and default keys.
fn parse_scope_spec(
    shared: &Arc<LuaShared>,
    name: &str,
    spec: &mlua::Table,
) -> LuaResult<ScopeSpec> {
    let api = "keymap.scope";
    let mut actions = Vec::new();
    if let Some(table) = spec.get::<Option<mlua::Table>>("actions")? {
        for pair in table.pairs::<String, String>() {
            let (action, description) = pair.map_err(|error| {
                LuaError::RuntimeError(format!(
                    "{api}: `actions` maps action names to descriptions: {error}"
                ))
            })?;
            actions.push(ActionSpec {
                name: action,
                description,
            });
        }
    }
    let extends: Option<String> = spec.get("extends")?;
    let inherited: Vec<String> = extends
        .as_deref()
        .and_then(|parent| keymaps_mut(shared, |map| map.actions(parent)))
        .unwrap_or_default();
    let mut keys = Vec::new();
    if let Some(table) = spec.get::<Option<mlua::Table>>("keys")? {
        for entry in table.sequence_values::<mlua::Table>() {
            let entry = entry?;
            let (mode, chord, action): (String, String, String) =
                (entry.get(1)?, entry.get(2)?, entry.get(3)?);
            if !actions.iter().any(|spec| spec.name == action) && !inherited.contains(&action) {
                return Err(LuaError::RuntimeError(format!(
                    "{api}: key `{chord}` of scope `{name}` runs undeclared action `{action}`"
                )));
            }
            keys.push(DefaultKey {
                mode: canonical_mode(api, &mode)?,
                chord: canonical_chord(shared, api, &chord)?,
                action,
            });
        }
    }
    // Actions come in the order of their first default key, the order the
    // owner lists its keys in; actions without one follow by name.
    let rank = |action: &ActionSpec| {
        keys.iter()
            .position(|key| key.action == action.name)
            .unwrap_or(usize::MAX)
    };
    actions.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name)));
    Ok(ScopeSpec {
        description: spec.get("desc")?,
        extends,
        actions,
        keys,
    })
}

/// Put a window, modal or overlay in keymap scope `name` with `actions`, a
/// table of action names to functions. Replaces its previous scope.
pub(crate) fn install_scope(
    lua: &Lua,
    owner: crate::smelt_edit::ScopeOwner,
    name: String,
    actions: Option<mlua::Table>,
) -> LuaResult<()> {
    valid_scope("scope", &name)?;
    let shared = lua
        .named_registry_value::<mlua::AnyUserData>("__smelt_shared")?
        .borrow::<super::win::SharedHandle>()?
        .0
        .clone();
    let mut ids = HashMap::new();
    if let Some(actions) = actions {
        for pair in actions.pairs::<String, mlua::Function>() {
            let (action, func) = pair.map_err(|error| {
                LuaError::RuntimeError(format!(
                    "scope `{name}`: actions map names to functions: {error}"
                ))
            })?;
            ids.insert(
                action,
                crate::lua::register_callback_handle(&shared, lua, func)?,
            );
        }
    }
    crate::lua::with_ui_host(|host| host.set_scope(owner, name, ids));
    Ok(())
}

pub(super) fn register(
    lua: &Lua,
    smelt_keymap: &mlua::Table,
    shared: &Arc<LuaShared>,
) -> LuaResult<()> {
    let m = LuaMod::own_supported(
        lua,
        smelt_keymap.clone(),
        "smelt.keymap",
        "Key bindings, global or per keymap scope. A scope names a part of the UI, such as `\"diff\"` or `\"resume\"`: windows, dialogs and overlays declare the scope they belong to and implement its actions, and `keymap.scope` declares each scope's actions and default keys. Keys look up the scopes around focus innermost first, then global bindings. User bindings override a scope's defaults whatever the load order. Chords and modes are canonicalized at registration; unknown values raise immediately.",
        Tier::Host,
    );
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
            "scope",
            "Declare keymap scope `name`: `spec.desc` describes it, `spec.actions` maps its action names to one-line descriptions, and `spec.keys` lists its default keys as `{ mode, chord, action }` rows. `spec.extends` names a declared scope to inherit actions and bindings from; the scope's own defaults and user bindings override inherited ones, and containers in the scope implement the inherited actions too. Containers in the scope implement the actions (`win:scope`, `opts.scope` of overlays and dialogs). Declaring a scope again replaces its actions and defaults; user bindings stay.",
            &["name", "spec"],
            move |_, (name, spec): (String, mlua::Table)| -> LuaResult<()> {
                valid_scope("keymap.scope", &name)?;
                let spec = parse_scope_spec(&s, &name, &spec)?;
                keymaps_mut(&s, |map| map.declare(&name, spec))
                    .unwrap_or(Ok(()))
                    .map_err(|error| LuaError::RuntimeError(format!("keymap.scope: {error}")))
            },
        )?;
    }

    {
        let s = shared.clone();
        m.fn_(
            "set",
            "Bind `chord` in `mode` to `target`: a function, or with `opts.scope` the name of one of the scope's actions. `mode` is `\"n\"|\"i\"|\"v\"|\"\"` (or the long form `normal`/`insert`/`visual`). Bindings follow the mode of the focused surface: its Vim mode, or without Vim, Insert in a text input and Normal anywhere else. A function receives a context table with the binding's `scope`; returning `false` passes the key on. `opts.desc` describes the binding for help UIs. Binding the same `(scope, mode, chord)` again replaces it, and a scoped binding overrides that scope's default. Returns a `Reg` whose `:remove()` drops the binding.",
            &["mode", "chord", "target", "opts"],
            move |lua,
                  (mode, chord, target, opts): (
                String,
                String,
                mlua::Value,
                Option<mlua::Table>,
            )|
                  -> LuaResult<LuaReg> {
                let api = "keymap.set";
                let mode = canonical_mode(api, &mode)?;
                let chord = canonical_chord(&s, api, &chord)?;
                let scope = opts_scope(api, opts.as_ref())?;
                let description = match &opts {
                    Some(opts) => opts.get::<Option<String>>("desc")?,
                    None => None,
                };
                let target = match target {
                    mlua::Value::Function(func) => Target::Handler(LuaHandle::from_func(lua, func)?),
                    mlua::Value::String(action) if !scope.is_empty() => {
                        Target::Action(action.to_str()?.to_string())
                    }
                    mlua::Value::String(_) => {
                        return Err(LuaError::RuntimeError(format!(
                            "{api}: an action target needs `opts.scope`"
                        )))
                    }
                    other => {
                        return Err(LuaError::RuntimeError(format!(
                            "{api}: target must be a function or an action name, got {}",
                            other.type_name()
                        )))
                    }
                };
                let serial = keymaps_mut(&s, |map| {
                    map.set(&scope, &mode, &chord, Binding { target, description })
                });
                let s_for_reg = s.clone();
                Ok(LuaReg::new(move || {
                    serial.is_some_and(|serial| {
                        keymaps_mut(&s_for_reg, |map| map.remove_set(&scope, &mode, &chord, serial))
                            .unwrap_or(false)
                    })
                }))
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "unset",
            "Drop the binding for `chord` in `mode`. With `opts.scope`, also hide that scope's default for the chord until a later `set`. `mode` accepts the same forms as `set`. Returns `true` if a binding was removed or hidden.",
            &["mode", "chord", "opts"],
            move |_, (mode, chord, opts): (String, String, Option<mlua::Table>)| -> LuaResult<bool> {
                let api = "keymap.unset";
                let mode = canonical_mode(api, &mode)?;
                let chord = canonical_chord(&s, api, &chord)?;
                let scope = opts_scope(api, opts.as_ref())?;
                Ok(keymaps_mut(&s, |map| {
                    if scope.is_empty() {
                        return map.remove(&scope, &mode, &chord);
                    }
                    let bound = map
                        .effective(&scope)
                        .iter()
                        .any(|row| row.chord == chord && (row.mode == mode || row.mode.is_empty()));
                    map.set(
                        &scope,
                        &mode,
                        &chord,
                        Binding {
                            target: Target::Disabled,
                            description: None,
                        },
                    );
                    bound
                })
                .unwrap_or(false))
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "list",
            "Return effective bindings as `{ scope, mode, chord, action?, desc?, source }` rows sorted by scope, mode and chord. `scope` is `\"\"` for global bindings and `source` is `\"default\"` or `\"user\"`. `mode` is the canonical short form and `chord` the display form. `opts.scope` lists only that scope.",
            &["opts"],
            move |lua, opts: Option<mlua::Table>| -> LuaResult<mlua::Table> {
                let only = match &opts {
                    Some(opts) => opts.get::<Option<String>>("scope")?,
                    None => None,
                };
                let out = lua.create_table()?;
                let Ok(map) = s.keymaps.lock() else {
                    return Ok(out);
                };
                let mut scopes: Vec<String> = std::iter::once(String::new())
                    .chain(map.scopes().map(|(name, _)| name.to_string()))
                    .collect();
                scopes.extend(map.user_scopes());
                scopes.sort();
                scopes.dedup();
                let mut index = 0;
                for scope in scopes.iter().filter(|scope| only.as_ref().is_none_or(|only| only == *scope)) {
                    for row in map.effective(scope) {
                        index += 1;
                        out.set(index, row_table(lua, scope, &row)?)?;
                    }
                }
                Ok(out)
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "scopes",
            "Return declared keymap scopes as `{ name, desc?, extends?, actions = { { name, desc, keys } } }` rows sorted by name. `actions` lists the scope's own actions, in the order of their first default key, then the actions without one by name. `keys` lists the display chords that run the action now, user bindings included.",
            &[],
            move |lua, ()| -> LuaResult<mlua::Table> {
                let out = lua.create_table()?;
                let Ok(map) = s.keymaps.lock() else {
                    return Ok(out);
                };
                for (i, (name, spec)) in map.scopes().enumerate() {
                    let effective = map.effective(name);
                    let row = lua.create_table()?;
                    row.set("name", name)?;
                    row.set("desc", spec.description.as_deref())?;
                    row.set("extends", spec.extends.as_deref())?;
                    let actions = lua.create_table()?;
                    for (j, action) in spec.actions.iter().enumerate() {
                        let entry = lua.create_table()?;
                        entry.set("name", action.name.as_str())?;
                        entry.set("desc", action.description.as_str())?;
                        let keys = lua.create_table()?;
                        for (k, key) in effective
                            .iter()
                            .filter(|row| row.action == Some(action.name.as_str()))
                            .enumerate()
                        {
                            keys.set(k + 1, crate::lua::display_chord_sequence(key.chord))?;
                        }
                        entry.set("keys", keys)?;
                        actions.set(j + 1, entry)?;
                    }
                    row.set("actions", actions)?;
                    out.set(i + 1, row)?;
                }
                Ok(out)
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "key_for",
            "Return the display chord that runs `action` of `scope` in `mode` (default Normal), preferring user bindings, then the shortest chord. Nil when no key runs it. Use it for hints, so they show the user's keys.",
            &["scope", "action", "mode"],
            move |_, (scope, action, mode): (String, String, Option<String>)| -> LuaResult<Option<String>> {
                let mode = canonical_mode("keymap.key_for", mode.as_deref().unwrap_or("n"))?;
                Ok(s.keymaps.lock().ok().and_then(|map| {
                    map.key_for(&scope, &action, &mode)
                        .map(crate::lua::display_chord_sequence)
                }))
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "label",
            "Return `chord` as a short hint label, the form footers and tips use: `<C-r>` is `ctrl+r`, `<M-d>` is `alt+d` and `<CR>` is `enter`. Nil input returns nil, so `smelt.keymap.label(smelt.keymap.key_for(...))` is safe for unbound actions.",
            &["chord"],
            move |_, chord: Option<String>| -> LuaResult<Option<String>> {
                let Some(chord) = chord else { return Ok(None) };
                let canonical = canonical_chord(&s, "keymap.label", &chord)?;
                Ok(Some(crate::lua::label_chord_sequence(&canonical)))
            },
        )?;
    }
    m.fn_(
        "mode",
        "Return the mode keys look bindings up in now: `\"n\"`, `\"i\"` or `\"v\"`. It is the Vim mode of the focused surface, or without Vim, `\"i\"` in a text input and `\"n\"` anywhere else.",
        &[],
        |_, ()| -> LuaResult<&'static str> {
            Ok(focus_chain().map_or("n", |chain| chain.mode()))
        },
    )?;
    m.fn_(
        "active",
        "Return the names of the keymap scopes around focus, innermost first.",
        &[],
        |_, ()| -> LuaResult<Vec<String>> {
            Ok(focus_chain()
                .map(|chain| chain.scopes())
                .unwrap_or_default())
        },
    )?;
    {
        let s = shared.clone();
        m.fn_(
            "run",
            "Run `action` of the innermost scope around focus that implements it, as if its key was pressed. Returns `false` when no scope around focus has the action or the action passed.",
            &["action"],
            move |lua, action: String| -> LuaResult<bool> {
                let Some(chain) = focus_chain() else {
                    return Ok(false);
                };
                let Some((frame, found)) = (0..chain.len())
                    .find_map(|frame| chain.action(frame, &action).map(|found| (frame, found)))
                else {
                    return Ok(false);
                };
                let id = match found {
                    ScopeAction::Lua(id) => id,
                    ScopeAction::Native(call) => {
                        crate::lua::with_ui_host(|host| host.run_native_action(call));
                        return Ok(true);
                    }
                };
                let func = {
                    let callbacks = s.callbacks.lock().map_err(|_| {
                        LuaError::RuntimeError("keymap.run: callback registry poisoned".into())
                    })?;
                    match callbacks.get(&id) {
                        Some(handle) => lua.registry_value::<mlua::Function>(&handle.key)?,
                        None => return Ok(false),
                    }
                };
                let ctx = lua.create_table()?;
                ctx.set("scope", chain.frames()[frame].scope)?;
                Ok(!matches!(func.call::<mlua::Value>(ctx)?, mlua::Value::Boolean(false)))
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "prefixes",
            "Return effective bindings around focus that extend `pending` as `{ mode, chord, suffix, desc? }` rows, inner scopes shadowing outer ones. `pending` may be empty to list top-level bindings, otherwise it is canonicalized with the current leader. `mode` accepts the same forms as `set` and replaces the focused mode; outside an app, global bindings are listed in Normal mode.",
            &["pending", "mode"],
            move |lua, (pending, mode): (String, Option<String>)| -> LuaResult<mlua::Table> {
                let api = "keymap.prefixes";
                let pending = if pending.is_empty() {
                    String::new()
                } else {
                    canonical_chord(&s, api, &pending)?
                };
                let mode = mode.map(|mode| canonical_mode(api, &mode)).transpose()?;
                let rows = match focus_chain() {
                    Some(chain) => {
                        let mut frames = chain.frames();
                        if let Some(mode) = &mode {
                            for frame in &mut frames {
                                frame.mode = mode;
                            }
                        }
                        s.keymaps
                            .lock()
                            .map(|map| map.prefixes(&frames, &pending))
                            .unwrap_or_default()
                    }
                    None => {
                        let frames = [smelt_core::lua::keymaps::Frame {
                            scope: "",
                            mode: mode.as_deref().unwrap_or("n"),
                        }];
                        s.keymaps
                            .lock()
                            .map(|map| map.prefixes(&frames, &pending))
                            .unwrap_or_default()
                    }
                };
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
