//! Lua-API fuzz target. `smelt_loop` exercises the TUI by feeding
//! terminal/engine events; `text_ops` hits the buffer-text primitives
//! directly. This target fills the third gap: the **Lua bindings layer**
//! (`crates/{core,tui}/src/lua/api/*`), which only runs when plugin Lua
//! code calls into it. Coverage measurement showed 14 of 27 Lua-API
//! modules sit under 50% - that's the surface this target attacks.
//!
//! Each `LuaOp` corresponds to one `smelt.*` call (or `/reload`). Ops
//! are serialised into a single Lua snippet per scenario and executed
//! against a fresh `TestApp`. The harness asserts the existing
//! `assert_invariants` floor (text / UI / session / resource) after every
//! batch, plus reload-survival invariants when `LuaOp::Reload` lands.
//!
//! Why a single batched `lua.load(...).exec()` and not one snippet per
//! op? Mutations have to thread through Lua locals (`local b1 = ...`),
//! and re-loading a new chunk per op would lose them. We build the
//! whole scenario as one chunk with a stable handle-pool layout
//! (`__fuzz.bufs[i]`, `__fuzz.wins[i]`, `__fuzz.overlays[i]`,
//! `__fuzz.paints[i]`) so later ops can reference earlier outputs.

use crate::SwarmWeights;
use arbitrary::{Arbitrary, Unstructured};
use serde::{Deserialize, Serialize};
use tui::app::test_harness::TestApp;

/// Bounded JSON-ish value for `state.<key> = value`. Mirrors `ArgsBag`
/// but flatter - Lua-side state assignment doesn't need deep tables to
/// reach meaningful coverage of the state-slot path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ArbValue {
    Nil,
    Bool(bool),
    Int(i32),
    Str(String),
}

impl<'a> Arbitrary<'a> for ArbValue {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        match u.int_in_range(0u8..=4)? {
            0 => Ok(ArbValue::Nil),
            1 => Ok(ArbValue::Bool(u.arbitrary()?)),
            2 => Ok(ArbValue::Int(u.arbitrary()?)),
            _ => {
                let len = u.int_in_range(0..=16)?;
                let bytes: Vec<u8> = (0..len)
                    .map(|_| u.arbitrary::<u8>())
                    .collect::<Result<_, _>>()?;
                Ok(ArbValue::Str(String::from_utf8_lossy(&bytes).into_owned()))
            }
        }
    }
}

impl ArbValue {
    fn emit(&self, out: &mut String) {
        match self {
            ArbValue::Nil => out.push_str("nil"),
            ArbValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            ArbValue::Int(i) => out.push_str(&i.to_string()),
            ArbValue::Str(s) => emit_lua_string(out, s),
        }
    }
}

/// Layout shape for `smelt.overlay.new`. Stays small on purpose: vbox
/// of two leaves and hbox of two leaves cover the multi-leaf measure
/// path; bare leaf covers the single-cell path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ArbLayout {
    /// Single leaf wrapping a tracked win by index.
    Leaf { win_idx: u8, with_measure: bool },
    /// Two leaves stacked vertically.
    Vbox { a: u8, b: u8 },
    /// Two leaves side by side.
    Hbox { a: u8, b: u8 },
}

impl<'a> Arbitrary<'a> for ArbLayout {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        match u.int_in_range(0u8..=99)? {
            0..=59 => Ok(ArbLayout::Leaf {
                win_idx: u.arbitrary()?,
                with_measure: u.arbitrary()?,
            }),
            60..=79 => Ok(ArbLayout::Vbox {
                a: u.arbitrary()?,
                b: u.arbitrary()?,
            }),
            _ => Ok(ArbLayout::Hbox {
                a: u.arbitrary()?,
                b: u.arbitrary()?,
            }),
        }
    }
}

/// One Lua-API call. Variant weights are tuned (see `Arbitrary` impl
/// below) to over-sample state-creating ops (`OverlayNew`, `Reload`,
/// `PaintRegister`) over the cheap probes (`StateGet`, `KeymapBind`)
/// so each scenario reaches deeper state in fewer ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LuaOp {
    /// `smelt.buf.new({ name = ... })`. Names are picked from a small
    /// pool ("fuzz.buf.<n>"), with `None` producing an anonymous buf.
    /// Same name twice exercises the NamedSlots dedup path.
    BufNew { name_slot: Option<u8> },

    /// `smelt.win.new(buf, { name = ... })`. `buf_idx % pool.len()`
    /// picks a live buf; if the pool is empty, the op no-ops.
    WinNew { buf_idx: u8, name_slot: Option<u8> },

    /// `smelt.overlay.new({ layout = ..., name = ..., keymap_count = N })`.
    /// Picks a layout shape; `keymap_count % 3` overlay-scoped bindings
    /// are attached. Exercises overlay keymap registration + cleanup.
    OverlayNew {
        layout: ArbLayout,
        name_slot: Option<u8>,
        keymap_count: u8,
    },

    /// `:remove()` on a tracked handle. `kind % 4` picks the pool
    /// (bufs/wins/overlays/paints), `idx % pool.len()` picks the
    /// victim. The local handle entry is set to `nil` so subsequent
    /// ops don't re-target it. Catches the `ce76000e` NamedSlots
    /// stale-binding-on-close bug class.
    Remove { kind: u8, idx: u8 },

    /// `/reload` pipeline. Wipes registries, re-runs `init.lua`,
    /// re-fires `on_ready` with `ctx.kind = "reload"`. Asserts named
    /// resources keep stable ids and anonymous ones are reaped.
    Reload,

    /// `smelt.paint.register(fn, { name = ... })`. `body_kind` picks
    /// a small body (no-op, write one cell, write a row) - the body
    /// itself only matters when the overlay actually renders.
    PaintRegister {
        name_slot: Option<u8>,
        body_kind: u8,
    },

    /// `smelt.state(slot).<key> = value`. `slot % POOL` picks one of
    /// ~8 named slots so the state-sweep code path sees both live and
    /// stale slots across reloads.
    StateSet {
        slot: u8,
        key: String,
        value: ArbValue,
    },

    /// `smelt.state(slot).<key>` read. Exercises lazy slot creation +
    /// key-miss return path.
    StateGet { slot: u8, key: String },

    /// `smelt.cmd.register(name, handler)`. Same name twice exercises
    /// the override path; new name on `/reload` exercises the wipe.
    CmdRegister { name_slot: u8, handler_kind: u8 },

    /// `smelt.cmd.run(name)`. Pulls from the same name pool as
    /// `CmdRegister`; bogus names exercise the not-found path.
    CmdInvoke { name_slot: u8 },

    /// `smelt.keymap.set(scope, chord, fn)`. `scope_kind % 3` picks
    /// ""/"prompt"/"content". Chord drawn from a small chord pool so
    /// collisions are likely.
    KeymapSet {
        scope_kind: u8,
        chord_slot: u8,
        handler_kind: u8,
    },

    /// Call one of the recently-added read-only Lua APIs: `win:scroll()`
    /// getter, `paint:rect()`, `smelt.transcript.blocks()`,
    /// `smelt.text.fit(s, w)`, `win:content_width()`,
    /// `smelt.session.text(id)`, `smelt.session.texts({ids})`.
    /// `kind % 7` picks the probe. These surfaces aren't reached by the
    /// lifecycle ops above - exercising them keeps the new
    /// render-pipeline and session-search accessors honest under fuzz.
    ProbeRead { kind: u8, target_idx: u8 },

    /// Call a `smelt.<module>.<name>` function selected by index into
    /// the [`smelt_core::lua::doc::snapshot`] registry - the same
    /// enumeration that powers `cargo xtask gen-lua-docs`. Any function
    /// added via `LuaMod::fn_` flows into this op automatically with
    /// no hand-written entry, so coverage of the Lua API surface keeps
    /// up with the surface itself instead of silently rotting.
    ///
    /// `arg_kind % 5` picks an arg shape (`nil`, `""`, `0`, `true`, `{}`).
    /// Most calls will fail with type errors (wrapped in `pcall`), but the
    /// host-side type-checking + arg-conversion paths still execute.
    ApiProbe { fn_idx: u16, arg_kind: u8 },

    /// Execute an arbitrary Lua snippet verbatim. **Not produced by the
    /// fuzz generator** - `build_lua_op` doesn't emit this variant. It
    /// exists so hand-authored regression seeds (under
    /// `fuzz/seeds/<target>/regression/`) can reproduce a specific bug
    /// deterministically: instead of guessing which `ApiProbe { fn_idx, arg_kind }`
    /// reduced to "the call that crashed", the seed just says
    /// `LuaSnippet { code = "smelt.os.unsetenv('')" }`. Stable across
    /// `Arbitrary` impl changes (raw libFuzzer-byte seeds aren't).
    LuaSnippet { code: String },

    /// `smelt.work.busy("label")` - push a busy token, store the
    /// returned `Reg` under `slot % POOL`. Exercises the per-app
    /// `BusyStack`: push ordering, label drift, reactive `work_*` cell
    /// publishing, and crucially the cleanup path (reload must wipe
    /// every token; `Reg:remove()` from `WorkBusyRelease` must pop the
    /// exact pushed id).
    WorkBusyAcquire { slot: u8, label_slot: u8 },

    /// `__fuzz.regs[slot]:remove()` - release a token captured by an
    /// earlier `WorkBusyAcquire`. No-op if the slot is nil; same chord
    /// the production code uses (`Reg:remove`) so the BusyStack release
    /// path with mismatched ids surfaces here too.
    WorkBusyRelease { slot: u8 },

    /// `smelt.engine.reload_when_idle()` - schedule host-owned reload from
    /// inside a Lua chunk. The runner drains it only after the chunk returns,
    /// matching the production safe-point behavior.
    ScheduleReload,

    /// High-validity API recipes. Unlike `ApiProbe`, these are curated
    /// multi-call snippets with well-typed arguments, so the target spends
    /// more time in success paths (resource creation, rule compilation,
    /// command callbacks) instead of only type-error conversion.
    ApiRecipe { kind: u8 },
}

impl LuaOp {
    /// Short human label for status-line / log display. Mirrors
    /// [`crate::FuzzOp::label`] - keeps the per-variant text next to
    /// the variant definition so adding a `LuaOp` is one edit.
    pub fn label(&self) -> String {
        use LuaOp::*;
        match self {
            BufNew { name_slot } => match name_slot {
                Some(n) => format!("buf.new (name slot {n})"),
                None => "buf.new (anon)".into(),
            },
            WinNew { buf_idx, name_slot } => match name_slot {
                Some(n) => format!("win.new (buf {buf_idx} -> name slot {n})"),
                None => format!("win.new (buf {buf_idx}, anon)"),
            },
            OverlayNew {
                name_slot,
                keymap_count,
                ..
            } => match name_slot {
                Some(n) => format!("overlay.new (name slot {n}, {keymap_count} kms)"),
                None => format!("overlay.new (anon, {keymap_count} kms)"),
            },
            Remove { kind, idx } => format!("remove kind={kind} idx={idx}"),
            Reload => "reload".into(),
            PaintRegister {
                name_slot,
                body_kind,
            } => match name_slot {
                Some(n) => format!("paint.register (name slot {n}, body {body_kind})"),
                None => format!("paint.register (anon, body {body_kind})"),
            },
            StateSet { slot, key, .. } => format!("state.set slot={slot} key={key:?}"),
            StateGet { slot, key } => format!("state.get slot={slot} key={key:?}"),
            CmdRegister {
                name_slot,
                handler_kind,
            } => format!("cmd.register slot={name_slot} body={handler_kind}"),
            CmdInvoke { name_slot } => format!("cmd.run slot={name_slot}"),
            KeymapSet {
                scope_kind,
                chord_slot,
                ..
            } => format!("keymap.set scope={scope_kind} chord_slot={chord_slot}"),
            ProbeRead { kind, target_idx } => {
                format!("probe_read kind={} idx={target_idx}", kind % 4)
            }
            ApiProbe { fn_idx, arg_kind } => {
                format!("api_probe fn={fn_idx} arg={arg_kind}")
            }
            LuaSnippet { code } => {
                let preview: String = code.chars().take(40).collect();
                format!("lua_snippet {preview:?}")
            }
            WorkBusyAcquire { slot, label_slot } => {
                format!("work.busy slot={slot} label={label_slot}")
            }
            WorkBusyRelease { slot } => format!("work.release slot={slot}"),
            ScheduleReload => "schedule reload".into(),
            ApiRecipe { kind } => format!("api recipe {}", kind % 5),
        }
    }
}

/// Per-variant LuaOp builder. Keep additions local: append one closure and
/// `N_LUAOP_VARIANTS` follows automatically.
type LuaOpBuilder = fn(&mut Unstructured<'_>) -> arbitrary::Result<LuaOp>;

const LUAOP_BUILDERS: &[LuaOpBuilder] = &[
    |u| {
        Ok(LuaOp::BufNew {
            name_slot: opt_slot(u)?,
        })
    },
    |u| {
        Ok(LuaOp::WinNew {
            buf_idx: u.arbitrary()?,
            name_slot: opt_slot(u)?,
        })
    },
    |u| {
        Ok(LuaOp::OverlayNew {
            layout: u.arbitrary()?,
            name_slot: opt_slot(u)?,
            keymap_count: u.arbitrary()?,
        })
    },
    |u| {
        Ok(LuaOp::Remove {
            kind: u.arbitrary()?,
            idx: u.arbitrary()?,
        })
    },
    |_| Ok(LuaOp::Reload),
    |u| {
        Ok(LuaOp::PaintRegister {
            name_slot: opt_slot(u)?,
            body_kind: u.arbitrary()?,
        })
    },
    |u| {
        Ok(LuaOp::StateSet {
            slot: u.arbitrary()?,
            key: arb_short_string(u, 16)?,
            value: u.arbitrary()?,
        })
    },
    |u| {
        Ok(LuaOp::StateGet {
            slot: u.arbitrary()?,
            key: arb_short_string(u, 16)?,
        })
    },
    |u| {
        Ok(LuaOp::CmdRegister {
            name_slot: u.arbitrary()?,
            handler_kind: u.arbitrary()?,
        })
    },
    |u| {
        Ok(LuaOp::CmdInvoke {
            name_slot: u.arbitrary()?,
        })
    },
    |u| {
        Ok(LuaOp::KeymapSet {
            scope_kind: u.arbitrary()?,
            chord_slot: u.arbitrary()?,
            handler_kind: u.arbitrary()?,
        })
    },
    |u| {
        Ok(LuaOp::ProbeRead {
            kind: u.arbitrary()?,
            target_idx: u.arbitrary()?,
        })
    },
    |u| {
        Ok(LuaOp::ApiProbe {
            fn_idx: u.arbitrary()?,
            arg_kind: u.arbitrary()?,
        })
    },
    |u| {
        Ok(LuaOp::WorkBusyAcquire {
            slot: u.arbitrary()?,
            label_slot: u.arbitrary()?,
        })
    },
    |u| {
        Ok(LuaOp::WorkBusyRelease {
            slot: u.arbitrary()?,
        })
    },
    |_| Ok(LuaOp::ScheduleReload),
    |u| {
        Ok(LuaOp::ApiRecipe {
            kind: u.arbitrary()?,
        })
    },
];

/// Total `LuaOp` variant count, derived from the dispatch table so it cannot drift.
pub const N_LUAOP_VARIANTS: usize = LUAOP_BUILDERS.len();

/// Build one `LuaOp` by variant index. Payloads still come from
/// `u.arbitrary()` so the value space stays unrestricted.
fn build_lua_op(idx: usize, u: &mut Unstructured<'_>) -> arbitrary::Result<LuaOp> {
    LUAOP_BUILDERS[idx](u)
}

impl<'a> Arbitrary<'a> for LuaOp {
    /// Default-uniform sample. Production scenarios use [`LuaScenario`]
    /// which draws a per-scenario [`SwarmWeights`] up front and samples
    /// `LuaOp`s from that table; this impl exists for ad-hoc callers
    /// (tests, single-op `Arbitrary`).
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let idx = u.int_in_range(0..=(N_LUAOP_VARIANTS - 1))?;
        build_lua_op(idx, u)
    }
}

fn opt_slot(u: &mut Unstructured<'_>) -> arbitrary::Result<Option<u8>> {
    if u.arbitrary::<bool>()? {
        Ok(Some(u.arbitrary()?))
    } else {
        Ok(None)
    }
}

fn arb_short_string(u: &mut Unstructured<'_>, max_bytes: usize) -> arbitrary::Result<String> {
    let len = u.int_in_range(0..=max_bytes)?;
    let bytes: Vec<u8> = (0..len)
        .map(|_| u.arbitrary::<u8>())
        .collect::<Result<_, _>>()?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LuaScenario {
    pub ops: Vec<LuaOp>,
}

impl<'a> Arbitrary<'a> for LuaScenario {
    /// Per-scenario swarm-weighted op stream. Each seed disables most
    /// `LuaOp` variants and skews the rest wildly so scenarios commit
    /// to one shape of workload (buf-heavy, hook-heavy, paint-heavy…)
    /// rather than uniformly bouncing across all variants. No state
    /// model - the emitted Lua has defensive `#pool == 0` guards so
    /// "act on an empty pool" ops cleanly no-op at runtime; the
    /// generator doesn't need to know what's live. (A prior state-aware
    /// generator added a retry loop and terminated scenarios early on
    /// state-starvation, costing more ops than it saved.) The swarm
    /// table itself isn't persisted in the JSON scenario - `ops` is
    /// what replays.
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let swarm = SwarmWeights::arbitrary(u, N_LUAOP_VARIANTS)?;
        let mut ops = Vec::new();
        while !u.is_empty() && ops.len() < LUA_MAX_OPS {
            let idx = swarm.pick(u)?;
            ops.push(build_lua_op(idx, u)?);
        }
        Ok(LuaScenario { ops })
    }
}

pub const LUA_MAX_OPS: usize = 96;
const NAME_POOL: usize = 8;
const CMD_POOL: usize = 6;
const CHORD_POOL: usize = 8;
const STATE_POOL: usize = 8;

const CHORDS: &[&str] = &[
    "<C-x>", "<C-y>", "<C-z>", "<F2>", "<F3>", "<F4>", "<A-q>", "<A-w>",
];

/// Emit a Lua string literal with embedded NUL / quote / backslash
/// escapes. Cheap subset of `string.format("%q", ...)` that doesn't
/// pull in mlua's formatter.
fn emit_lua_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\{:03}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn name_or_anon(slot: Option<u8>, prefix: &str) -> Option<String> {
    slot.map(|n| format!("{prefix}.{}", (n as usize) % NAME_POOL))
}

/// Emit `local <var> = smelt.<call>(...)` and track the handle in the
/// pool so subsequent ops can reference it via `__fuzz.<pool>[i]`.
fn emit_buf_new(out: &mut String, name_slot: Option<u8>) {
    let name = name_or_anon(name_slot, "fuzz.buf");
    out.push_str(
        "__fuzz.bufs[#__fuzz.bufs+1] = (function()\n  local ok, v = pcall(smelt.buf.new, ",
    );
    match name {
        Some(n) => {
            out.push_str("{ name = ");
            emit_lua_string(out, &n);
            out.push_str(" }");
        }
        None => out.push_str("{}"),
    }
    out.push_str(")\n  return ok and v or nil\nend)()\n");
}

fn emit_win_new(out: &mut String, buf_idx: u8, name_slot: Option<u8>) {
    let name = name_or_anon(name_slot, "fuzz.win");
    out.push_str("(function()\n");
    out.push_str(&format!(
        "  local b = __fuzz.bufs[({} % math.max(1, #__fuzz.bufs)) + 1]\n",
        buf_idx
    ));
    out.push_str("  if not b then return end\n");
    out.push_str("  local ok, v = pcall(smelt.win.new, b, ");
    match name {
        Some(n) => {
            out.push_str("{ name = ");
            emit_lua_string(out, &n);
            out.push_str(" }");
        }
        None => out.push_str("{}"),
    }
    out.push_str(")\n  if ok and v then __fuzz.wins[#__fuzz.wins+1] = v end\nend)()\n");
}

fn emit_layout(out: &mut String, layout: &ArbLayout) {
    match layout {
        ArbLayout::Leaf {
            win_idx,
            with_measure,
        } => {
            out.push_str(&format!(
                "smelt.ui.layout.leaf(__fuzz.wins[({} % math.max(1, #__fuzz.wins)) + 1]",
                win_idx
            ));
            if *with_measure {
                out.push_str(", { measure = { w = 18, h = 4 } }");
            }
            out.push(')');
        }
        ArbLayout::Vbox { a, b } => {
            out.push_str(&format!(
                "smelt.ui.layout.vbox({{ \
                    {{ node = smelt.ui.layout.leaf(__fuzz.wins[({} % math.max(1, #__fuzz.wins)) + 1]), height = 3 }}, \
                    {{ node = smelt.ui.layout.leaf(__fuzz.wins[({} % math.max(1, #__fuzz.wins)) + 1]), height = 3 }} \
                }})",
                a, b
            ));
        }
        ArbLayout::Hbox { a, b } => {
            out.push_str(&format!(
                "smelt.ui.layout.hbox({{ \
                    {{ node = smelt.ui.layout.leaf(__fuzz.wins[({} % math.max(1, #__fuzz.wins)) + 1]), width = 8 }}, \
                    {{ node = smelt.ui.layout.leaf(__fuzz.wins[({} % math.max(1, #__fuzz.wins)) + 1]), width = 8 }} \
                }})",
                a, b
            ));
        }
    }
}

fn emit_overlay_new(out: &mut String, layout: &ArbLayout, name_slot: Option<u8>, keymap_count: u8) {
    let name = name_or_anon(name_slot, "fuzz.ov");
    out.push_str("(function()\n");
    out.push_str("  if #__fuzz.wins == 0 then return end\n");
    out.push_str("  local opts = { anchor = \"screen_at\", corner = \"nw\", row = 0, col = 0, width = 20, height = 6, layout = ");
    emit_layout(out, layout);
    out.push_str(" }\n");
    if let Some(n) = name {
        out.push_str("  opts.name = ");
        emit_lua_string(out, &n);
        out.push('\n');
    }
    let km = (keymap_count as usize) % 3;
    if km > 0 {
        out.push_str("  opts.keymaps = {\n");
        for i in 0..km {
            let chord = CHORDS[i % CHORDS.len()];
            out.push_str(&format!(
                "    {{ key = \"{chord}\", on_press = function() end }},\n"
            ));
        }
        out.push_str("  }\n");
    }
    out.push_str("  local ok, v = pcall(smelt.overlay.new, opts)\n");
    out.push_str("  if ok and v then __fuzz.overlays[#__fuzz.overlays+1] = v end\nend)()\n");
}

fn emit_remove(out: &mut String, kind: u8, idx: u8) {
    let pool = match kind % 4 {
        0 => "bufs",
        1 => "wins",
        2 => "overlays",
        _ => "paints",
    };
    // `table.remove` (not `pool[i] = nil`) so live indices stay dense.
    // The `n == 0` guard handles the (common) case where the swarm
    // emits Remove against an empty pool - the generator no longer
    // pre-filters such ops.
    out.push_str("(function()\n");
    out.push_str(&format!("  local n = #__fuzz.{pool}\n"));
    out.push_str("  if n == 0 then return end\n");
    out.push_str(&format!("  local i = ({idx} % n) + 1\n"));
    out.push_str(&format!("  local h = __fuzz.{pool}[i]\n"));
    out.push_str("  if h and h.remove then pcall(h.remove, h) end\n");
    out.push_str(&format!("  table.remove(__fuzz.{pool}, i)\n"));
    out.push_str("end)()\n");
}

fn emit_paint_register(out: &mut String, name_slot: Option<u8>, body_kind: u8) {
    let name = name_or_anon(name_slot, "fuzz.paint");
    out.push_str("(function()\n");
    let body = match body_kind % 3 {
        0 => "function(_slice, _ctx) end",
        1 => "function(slice, _ctx) if slice and slice.put then slice:put(0, 0, 'x', nil) end end",
        _ => "function(slice, _ctx) if slice and slice.put then for c = 0, 3 do slice:put(c, 0, '.', nil) end end end",
    };
    out.push_str(&format!("  local body = {body}\n"));
    out.push_str("  local opts = ");
    match name {
        Some(n) => {
            out.push_str("{ name = ");
            emit_lua_string(out, &n);
            out.push_str(" }");
        }
        None => out.push_str("nil"),
    }
    out.push('\n');
    out.push_str("  local ok, v = pcall(smelt.paint.register, body, opts)\n");
    out.push_str("  if ok and v then __fuzz.paints[#__fuzz.paints+1] = v end\nend)()\n");
}

fn emit_state_set(out: &mut String, slot: u8, key: &str, value: &ArbValue) {
    let s = (slot as usize) % STATE_POOL;
    out.push_str(&format!(
        "(function()\n  local s = smelt.state(\"fuzz.state.{s}\")\n  local ok = pcall(function() s["
    ));
    emit_lua_string(out, key);
    out.push_str("] = ");
    value.emit(out);
    out.push_str(" end)\nend)()\n");
}

fn emit_state_get(out: &mut String, slot: u8, key: &str) {
    let s = (slot as usize) % STATE_POOL;
    out.push_str(&format!(
        "(function()\n  local s = smelt.state(\"fuzz.state.{s}\")\n  local _ = s["
    ));
    emit_lua_string(out, key);
    out.push_str("]\nend)()\n");
}

fn emit_cmd_register(out: &mut String, name_slot: u8, handler_kind: u8) {
    let n = (name_slot as usize) % CMD_POOL;
    // Body variants 3..=5 are **re-entrant**: the registered handler
    // calls back into the Rust API mid-execution. This drives the
    // Rust→Lua-callback→Rust path that flat top-level call sequences
    // can't reach - bug class is "second Rust call mutates state the
    // outer Rust call still holds a reference to". Variant 5 emits
    // mutual recursion across two `fuzz.cmd.*` slots (bounded by Lua
    // stack depth + the host's command-dispatch reentry guard).
    let body = match handler_kind % 6 {
        0 => "function() end",
        1 => "function() smelt.state(\"fuzz.cmd_count\").n = (smelt.state(\"fuzz.cmd_count\").n or 0) + 1 end",
        2 => "function() error(\"fuzz cmd error\") end",
        // Re-entrant: create a buf from inside the handler. Stresses
        // the lifetime of any buf-registry lock the outer dispatch
        // holds.
        3 => "function() pcall(smelt.buf.new, {}) end",
        // Re-entrant: invoke another command from this handler. Pairs
        // with the existing `fuzz.cmd.<n>` namespace so a sibling
        // command may recursively invoke us back.
        4 => "function() pcall(smelt.cmd.run, \"fuzz.cmd.0\") end",
        // Re-entrant: read+write state, then invoke another command.
        _ => "function() local s = smelt.state(\"fuzz.cmd_reentry\"); s.depth = (s.depth or 0) + 1; if (s.depth or 0) < 4 then pcall(smelt.cmd.run, \"fuzz.cmd.\" .. ((s.depth or 0) % 6)) end; s.depth = (s.depth or 1) - 1 end",
    };
    out.push_str(&format!(
        "pcall(smelt.cmd.register, \"fuzz.cmd.{n}\", {body})\n"
    ));
}

fn emit_cmd_invoke(out: &mut String, name_slot: u8) {
    let n = (name_slot as usize) % CMD_POOL;
    out.push_str(&format!("pcall(smelt.cmd.run, \"fuzz.cmd.{n}\")\n"));
}

fn emit_keymap_set(out: &mut String, scope_kind: u8, chord_slot: u8, handler_kind: u8) {
    let scope = match scope_kind % 3 {
        0 => "",
        1 => "prompt",
        _ => "content",
    };
    let chord = CHORDS[(chord_slot as usize) % CHORD_POOL];
    let body = match handler_kind % 3 {
        0 => "function() end",
        1 => "function() return false end",
        _ => "function() smelt.state(\"fuzz.km_fires\").n = (smelt.state(\"fuzz.km_fires\").n or 0) + 1 end",
    };
    out.push_str(&format!(
        "pcall(smelt.keymap.set, \"{scope}\", \"{chord}\", {body})\n"
    ));
}

/// Build the full Lua chunk for `ops`. `api_metas` is the live
/// `(module, name)` enumeration from `TestApp::lua_doc_snapshot()` -
/// `LuaOp::ApiProbe` indices mod into it at emit time so the fuzz
/// surface tracks the API surface automatically. Returns the snippet
/// string.
pub fn build_snippet(ops: &[LuaOp], api_metas: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(2048);
    // Handle pool - declared as Lua-side tables so emitted ops can
    // index into them. Initialised fresh each scenario; on /reload
    // these locals survive (they're in the eval frame, not the
    // bundled-plugin frame that gets wiped).
    out.push_str(
        "local __fuzz = { bufs = {}, wins = {}, overlays = {}, paints = {}, regs = {} }\n",
    );
    for op in ops {
        match op {
            LuaOp::BufNew { name_slot } => emit_buf_new(&mut out, *name_slot),
            LuaOp::WinNew { buf_idx, name_slot } => emit_win_new(&mut out, *buf_idx, *name_slot),
            LuaOp::OverlayNew {
                layout,
                name_slot,
                keymap_count,
            } => emit_overlay_new(&mut out, layout, *name_slot, *keymap_count),
            LuaOp::Remove { kind, idx } => emit_remove(&mut out, *kind, *idx),
            LuaOp::Reload => {
                // /reload is a host-side call - emit a sentinel the
                // runner picks up by splitting the op stream. We can't
                // reload from inside a running chunk because mlua
                // can't recursively bring up its own runtime.
                out.push_str("-- @reload@\n");
            }
            LuaOp::PaintRegister {
                name_slot,
                body_kind,
            } => emit_paint_register(&mut out, *name_slot, *body_kind),
            LuaOp::StateSet { slot, key, value } => emit_state_set(&mut out, *slot, key, value),
            LuaOp::StateGet { slot, key } => emit_state_get(&mut out, *slot, key),
            LuaOp::CmdRegister {
                name_slot,
                handler_kind,
            } => emit_cmd_register(&mut out, *name_slot, *handler_kind),
            LuaOp::CmdInvoke { name_slot } => emit_cmd_invoke(&mut out, *name_slot),
            LuaOp::KeymapSet {
                scope_kind,
                chord_slot,
                handler_kind,
            } => emit_keymap_set(&mut out, *scope_kind, *chord_slot, *handler_kind),
            LuaOp::ProbeRead { kind, target_idx } => emit_probe_read(&mut out, *kind, *target_idx),
            LuaOp::ApiProbe { fn_idx, arg_kind } => {
                emit_api_probe(&mut out, *fn_idx, *arg_kind, api_metas);
            }
            LuaOp::LuaSnippet { code } => {
                // Wrap in pcall so a regression scenario that hits a
                // Lua-level error still gets to the post-scenario
                // invariants - those are what catch the real bug
                // (panic in a binding, leaked handle, …).
                out.push_str("pcall(function()\n");
                out.push_str(code);
                out.push_str("\nend)\n");
            }
            LuaOp::WorkBusyAcquire { slot, label_slot } => {
                emit_work_busy_acquire(&mut out, *slot, *label_slot);
            }
            LuaOp::WorkBusyRelease { slot } => emit_work_busy_release(&mut out, *slot),
            LuaOp::ScheduleReload => {
                out.push_str("pcall(function() smelt.engine.reload_when_idle() end)\n");
            }
            LuaOp::ApiRecipe { kind } => emit_api_recipe(&mut out, *kind),
        }
    }
    out
}

/// Emit curated, well-typed Lua API sequences. These complement `ApiProbe`:
/// probes keep breadth over the generated API registry, recipes keep depth in
/// success paths where resources are created, callbacks run, and permissions
/// rules are compiled/evaluated.
fn emit_api_recipe(out: &mut String, kind: u8) {
    match kind % 5 {
        0 => out.push_str(
            r#"pcall(function()
  local b = smelt.buf.new({ name = "fuzz.recipe.buf" })
  local w = smelt.win.new(b, { name = "fuzz.recipe.win" })
  local p = smelt.paint.register(function(slice, _ctx)
    if slice and slice.put then slice:put(0, 0, "r", nil) end
  end, { name = "fuzz.recipe.paint" })
  local ov = smelt.overlay.new({
    name = "fuzz.recipe.overlay",
    anchor = "screen_at", corner = "nw", row = 1, col = 1, width = 12, height = 3,
    layout = smelt.ui.layout.leaf(w),
    keymaps = { { key = "<C-r>", on_press = function() smelt.state("fuzz.recipe").hit = true end } },
  })
  if p and p.rect then p:rect() end
  if w and w.content_width then w:content_width() end
  if ov and ov.remove then ov:remove() end
end)
"#,
        ),
        1 => out.push_str(
            r#"pcall(function()
  smelt.permissions.set_rules({
    default = {
      tools = { allow = { "bash", "web_fetch" }, ask = { "edit" }, deny = { "danger" } },
      bash = { allow = { "git status*", "ls*" }, ask = { "cat *" }, deny = { "rm*", "sudo*" } },
      web_fetch = { allow = { "https://example.com/*" }, deny = { "http://*" } },
    },
    plan = { tools = { ask = { "bash" } }, bash = { allow = { "pwd" } } },
  })
  smelt.permissions.check_tool("normal", "bash")
  smelt.permissions.check("normal", "bash", "git status --short")
  smelt.permissions.check("normal", "bash", "rm -rf target")
  smelt.permissions.list()
end)
"#,
        ),
        2 => out.push_str(
            r#"pcall(function()
  smelt.cmd.register("fuzz.recipe.cmd", function()
    local s = smelt.state("fuzz.recipe.cmd")
    s.count = (s.count or 0) + 1
    smelt.text.fit("recipe", 4)
  end)
  smelt.cmd.run("fuzz.recipe.cmd")
  smelt.keymap.set("prompt", "<C-r>", function() smelt.cmd.run("fuzz.recipe.cmd") end)
end)
"#,
        ),
        3 => out.push_str(
            r#"pcall(function()
  local reg = smelt.work.busy("fuzz.recipe.busy")
  smelt.engine.reload_when_idle()
  if reg then reg:remove() end
end)
"#,
        ),
        _ => out.push_str(
            r#"pcall(function()
  smelt.permissions.set_rules({
    default = {
      tools = { allow = { "bash" }, ask = { "web_fetch" }, deny = { "danger" } },
      bash = { allow = { "pwd", "echo *" }, ask = { "cat *" }, deny = { "rm*" } },
    },
  })
  smelt.permissions.list()
  smelt.permissions.check_tool("normal", "bash")
  smelt.permissions.check("normal", "bash", "echo hello")
end)
"#,
        ),
    }
}

/// `__fuzz.regs[slot] = smelt.work.busy("fuzz.label.<n>")`. The slot
/// pool is 6-wide so collisions on the same slot exercise the
/// "overwrite then leak the prior `Reg`" case - the prior token must
/// then survive only on the BusyStack (with no client handle), making
/// reload-time cleanup the only release path.
fn emit_work_busy_acquire(out: &mut String, slot: u8, label_slot: u8) {
    let slot = (slot as usize) % 6 + 1;
    let label = label_slot as usize % 4;
    out.push_str(&format!(
        "pcall(function() __fuzz.regs[{slot}] = smelt.work.busy(\"fuzz.label.{label}\") end)\n"
    ));
}

/// `__fuzz.regs[slot]:remove()` - pop the matching busy token from
/// the BusyStack. Drops the slot afterwards so a follow-up release
/// no-ops cleanly (matches plugin code that nils out the Reg after
/// release).
fn emit_work_busy_release(out: &mut String, slot: u8) {
    let slot = (slot as usize) % 6 + 1;
    out.push_str(&format!(
        "pcall(function() if __fuzz.regs[{slot}] then __fuzz.regs[{slot}]:remove(); __fuzz.regs[{slot}] = nil end end)\n"
    ));
}

/// Emit a `pcall(<module>.<name>, <arg>)` for one of the api_metas
/// entries. Indices mod into the slice so a u16 maps onto any-sized
/// API surface. `arg_kind % 5` picks the arg shape; most calls will
/// fail with type errors but the host-side conversion path still runs.
fn emit_api_probe(out: &mut String, fn_idx: u16, arg_kind: u8, api_metas: &[(&str, &str)]) {
    if api_metas.is_empty() {
        return;
    }
    let (module, name) = api_metas[(fn_idx as usize) % api_metas.len()];
    // Skip private (`__`-prefixed) entries - they're internals not
    // intended for plugin callers, and probing them just creates noise.
    if name.starts_with("__") {
        return;
    }
    let arg = match arg_kind % 5 {
        0 => "nil",
        1 => "\"\"",
        2 => "0",
        3 => "true",
        _ => "{}",
    };
    out.push_str(&format!(
        "pcall(function() return {module}.{name}({arg}) end)\n"
    ));
}

/// `kind % 7` picks one of the recently-added read-only APIs:
/// 0: `win:scroll()` (getter) on a tracked win
/// 1: `paint:rect()` on a tracked paint handle
/// 2: `smelt.transcript.blocks()`
/// 3: `smelt.text.fit(string, width)`
/// 4: `win:content_width()` on a tracked win
/// 5: `smelt.session.text(id)` against a missing id (sidecar fallback path)
/// 6: `smelt.session.texts({ids})` parallel batch read
/// All wrapped in `pcall` so a missing handle or an API regression
/// surfaces as a fuzz-visible failure (panic) rather than a silent miss.
fn emit_probe_read(out: &mut String, kind: u8, target_idx: u8) {
    let idx = (target_idx as usize) % 8 + 1;
    match kind % 7 {
        0 => {
            out.push_str(&format!(
                "do local w = __fuzz.wins[{idx}]; if w then pcall(function() return w:scroll() end) end end\n"
            ));
        }
        1 => {
            out.push_str(&format!(
                "do local p = __fuzz.paints[{idx}]; if p then pcall(function() return p:rect() end) end end\n"
            ));
        }
        2 => {
            out.push_str(
                "pcall(function() return smelt.transcript and smelt.transcript.blocks and smelt.transcript.blocks() end)\n",
            );
        }
        3 => {
            out.push_str(
                "pcall(function() return smelt.text and smelt.text.fit and smelt.text.fit(\"abc\", 5) end)\n",
            );
        }
        4 => {
            out.push_str(&format!(
                "do local w = __fuzz.wins[{idx}]; if w then pcall(function() return w:content_width() end) end end\n"
            ));
        }
        5 => {
            out.push_str(
                "pcall(function() return smelt.session and smelt.session.text and smelt.session.text(\"__fuzz_missing\") end)\n",
            );
        }
        _ => {
            out.push_str(
                "pcall(function() return smelt.session and smelt.session.texts and smelt.session.texts({ \"__fuzz_missing_a\", \"__fuzz_missing_b\" }) end)\n",
            );
        }
    }
}

/// Run one scenario end-to-end. The build emits a `-- @reload@` line
/// for every `LuaOp::Reload`; the runner splits on that line so each
/// resulting segment is one chunk, with `app.reload_lua()` interleaved.
/// Invariants assert after every segment AND after every reload -
/// failures stay attached to whichever op caused them.
pub fn run_lua_scenario(scenario: LuaScenario) {
    crate::runtime::with_current_thread_runtime("lua_loop", || {
        let mut app = TestApp::builder().build();
        // Capture the live Lua doc-registry enumeration *after* the TUI
        // LuaRuntime has finished registering every `LuaMod::fn_`. The
        // snapshot drives `LuaOp::ApiProbe` emit so the fuzz surface
        // tracks the API surface automatically.
        let api_metas = app.lua_doc_snapshot();
        let take = scenario.ops.len().min(LUA_MAX_OPS);
        let ops = &scenario.ops[..take];
        let snippet = build_snippet(ops, &api_metas);

        let segments: Vec<&str> = snippet.split("-- @reload@\n").collect();
        for (i, segment) in segments.iter().enumerate() {
            if !segment.trim().is_empty() {
                // Lua-level errors aren't fuzz failures - the snippet
                // intentionally tolerates type errors via `pcall`. Real
                // bugs surface through `assert_invariants`, Rust panics
                // inside binding code, or the FFI ledger detecting a
                // dangling `RegistryKey`.
                let _ = app.run_lua(segment);
                app.assert_invariants();
                // FFI ledger: force a full Lua GC and verify every Rust-
                // side `LuaHandle` still resolves in the mlua registry.
                // Without this, a path that drops a `RegistryKey` without
                // calling `remove` survives latently - only manifesting
                // when something else tries to invoke the dead handle,
                // potentially much later or never. Running it between
                // segments pins the failure to the op batch responsible.
                app.assert_lua_handles_alive();
                if app.drain_idle_work() {
                    app.assert_lua_handles_alive();
                }
                app.assert_invariants();
            }
            // Reload BETWEEN segments (matches the sentinel position).
            // Skipped after the last segment so the final invariant check
            // observes terminal state, not post-reload state.
            if i + 1 < segments.len() {
                app.reload_lua();
                app.assert_invariants();
                // `/reload` is the heaviest GC-and-rebuild surface in the
                // Lua API - re-check per-field liveness afterward so a
                // reload that forgot to re-register a named handle surfaces
                // here.
                app.assert_lua_handles_alive();
            }
        }
        // Post-scenario steady-state leak check: do two more reloads from
        // current state and assert the live-handle count is stable between
        // them. Catches reload-path leaks (handles created during reload
        // but never dropped) that the per-field walk above can't see - it
        // only checks tracked fields, not the global counter.
        drop(segments);
        app.assert_no_handle_leak_across_reload();
    });
}
