//! `smelt.buf` — buffer creation, line/source mutation, extmarks. UiHost-only.

use crate::lua::LuaShared;
use lua_doc_derive::lua_module;
use lua_doc_derive::{LuaAlias, LuaOpts};
use mlua::prelude::*;
use smelt_core::lua::doc::register_ui_fn;
use std::sync::Arc;

/// Where a virtual-text chunk is rendered relative to the line.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.buf.VirtTextPos")]
pub enum LuaVirtTextPos {
    /// Inserted inline at `(row, col)`, shifting real text after it.
    Inline,
    /// Painted on top of existing text at `(row, col)`.
    Overlay,
    /// Right-aligned at the end of the screen line.
    RightAlign,
    /// Appended after the last column (default).
    Eol,
}

impl From<LuaVirtTextPos> for smelt_core::buffer::VirtTextPos {
    fn from(p: LuaVirtTextPos) -> Self {
        use smelt_core::buffer::VirtTextPos;
        match p {
            LuaVirtTextPos::Inline => VirtTextPos::Inline,
            LuaVirtTextPos::Overlay => VirtTextPos::Overlay,
            LuaVirtTextPos::RightAlign => VirtTextPos::RightAlign,
            LuaVirtTextPos::Eol => VirtTextPos::Eol,
        }
    }
}

/// Options accepted by `smelt.buf.set_extmark`. Mirrors a useful subset
/// of `nvim_buf_set_extmark`'s keyset; pick highlight or virt-text
/// fields, not both.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.buf.ExtmarkOpts")]
pub struct LuaExtmarkOpts {
    /// Retarget an existing mark by id instead of allocating a new one.
    pub id: Option<u32>,
    /// 1-based end row (inclusive). `nil` keeps the mark single-line.
    pub end_row: Option<u64>,
    /// End column for highlight ranges.
    pub end_col: Option<u64>,
    /// Higher-priority marks paint over lower-priority ones.
    #[lua(default)]
    pub priority: u32,
    /// If true, the mark sticks with text inserted to its right.
    pub right_gravity: Option<bool>,
    /// Right-gravity flag for the end-of-range cursor.
    pub end_right_gravity: Option<bool>,

    /// Theme group whose style is applied as the highlight base.
    pub hl_group: Option<String>,
    /// Theme group whose foreground overrides `hl_group`.
    pub fg: Option<String>,
    /// Theme group whose background overrides `hl_group`.
    pub bg: Option<String>,
    /// Force-bold the highlight.
    pub bold: Option<bool>,
    /// Force-dim the highlight.
    pub dim: Option<bool>,
    /// Force-italic the highlight.
    pub italic: Option<bool>,
    /// Extend the highlight past the last column to fill the EOL.
    pub hl_eol: Option<bool>,
    /// Paint this highlight only on the window's cursor row. Lets you decorate
    /// the selected list item (or any selection-aware sub-range) without re-rendering
    /// on every selection change.
    pub on_cursor_row: Option<bool>,

    /// Virtual-text chunk to render alongside the line.
    pub virt_text: Option<String>,
    /// Theme group applied to the virt-text chunk.
    pub virt_text_hl: Option<String>,
    /// Where the virt-text appears relative to the line.
    pub virt_text_pos: Option<LuaVirtTextPos>,

    /// If false, the range is skipped by mouse selection.
    pub selectable: Option<bool>,
    /// Override the yanked string when the user copies this range.
    pub yank_as: Option<String>,
}

#[lua_module(
    name = "smelt.buf",
    doc = "Buffer creation, line/source mutation, extmarks, and yank. UiHost-only — buffers are terminal-screen backing stores that windows render into."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let buf_tbl = lua.create_table()?;
    register_ui_fn(
        &buf_tbl,
        "smelt.buf",
        "text",
        "Return the buffer's full source as a string. `nil` if no buffer has that id.",
        &["buf"],
        lua,
        |_, id: u64| -> LuaResult<Option<String>> {
            Ok(crate::lua::try_with_app(|app| {
                app.ui
                    .buf(crate::smelt_term::BufId(id))
                    .map(|b| b.source().to_string())
            })
            .flatten())
        },
    )?;

    {
        let s = shared.clone();
        register_ui_fn(
            &buf_tbl,
            "smelt.buf",
            "create",
            "Create a new buffer and return its id. `opts.mode` selects a `BufFormat` parser; `opts.readonly` blocks edits via the public mutators.",
            &["opts"],
            lua,
            move |_, (opts,): (Option<mlua::Table>,)| -> LuaResult<u64> {
                let format = match opts.as_ref() {
                    Some(t) => match t.get::<Option<String>>("mode")? {
                        Some(mode) => Some(
                            crate::format::BufFormat::from_lua_spec(&mode, t)
                                .map_err(|e| LuaError::RuntimeError(format!("buf.create: {e}")))?,
                        ),
                        None => None,
                    },
                    None => None,
                };
                let readonly: bool = opts
                    .as_ref()
                    .and_then(|t| t.get::<bool>("readonly").ok())
                    .unwrap_or(false);
                let editable: bool = opts
                    .as_ref()
                    .and_then(|t| t.get::<bool>("editable").ok())
                    .unwrap_or(false);
                let undo_limit: Option<usize> = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<u64>>("undo").ok())
                    .flatten()
                    .map(|n| n as usize);
                let id = s
                    .next_buf_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                crate::lua::with_app(|app| {
                    match app.ui.buf_create_with_id(
                        crate::smelt_term::BufId(id),
                        crate::smelt_term::BufCreateOpts::default(),
                    ) {
                        Ok(bid) => {
                            if let Some(buf) = app.ui.buf_mut(bid) {
                                buf.readonly = readonly;
                                if let Some(fmt) = format {
                                    buf.set_parser(fmt.into_parser());
                                }
                                if editable {
                                    let limit = undo_limit.or(Some(100));
                                    buf.history = crate::smelt_term::UndoHistory::new(limit);
                                }
                            }
                        }
                        Err(clash) => {
                            app.notify_error(format!("buf.create: id {} already in use", clash.0));
                        }
                    }
                });
                Ok(id)
            },
        )?;
    }

    register_ui_fn(
        &buf_tbl,
        "smelt.buf",
        "set_readonly",
        "Toggle a buffer's read-only flag. Read-only buffers reject `set_lines`/`set_source`.",
        &["buf", "ro"],
        lua,
        |_, (id, ro): (u64, bool)| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                if let Some(buf) = app.ui.buf_mut(crate::smelt_term::BufId(id)) {
                    buf.readonly = ro;
                }
            });
            Ok(())
        },
    )?;

    register_ui_fn(
        &buf_tbl,
        "smelt.buf",
        "set_lines",
        "Replace every line of the buffer with the strings in `lines`.",
        &["buf", "lines"],
        lua,
        |_, (id, lines): (u64, Vec<String>)| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                if let Some(buf) = app.ui.buf_mut(crate::smelt_term::BufId(id)) {
                    buf.set_all_lines(lines);
                }
            });
            Ok(())
        },
    )?;

    register_ui_fn(
        &buf_tbl,
        "smelt.buf",
        "get_line",
        "Read a single line by 1-based index. Returns `nil` when out of range.",
        &["buf", "line"],
        lua,
        |_, (id, line_idx): (u64, u64)| -> LuaResult<Option<String>> {
            let line0 = match line_idx.checked_sub(1) {
                Some(n) => n as usize,
                None => return Ok(None),
            };
            let text = crate::lua::with_app(|app| {
                app.ui
                    .buf(crate::smelt_term::BufId(id))
                    .and_then(|b| b.get_line(line0).map(|s| s.to_string()))
            });
            Ok(text)
        },
    )?;

    register_ui_fn(
        &buf_tbl,
        "smelt.buf",
        "set_source",
        "Replace the buffer's full source text in one call. Cheaper than `set_lines` when you already have the joined string.",
        &["buf", "source"],
        lua,
        |_, (id, source): (u64, String)|  -> LuaResult<()>{
            crate::lua::with_app(|app| {
                if let Some(buf) = app.ui.buf_mut(crate::smelt_term::BufId(id)) {
                    buf.set_source(source);
                }
            });
            Ok(())
        },
    )?;

    register_ui_fn(
        &buf_tbl,
        "smelt.buf",
        "set_styled_lines",
        "Replace the buffer with a list of styled lines. Each line is a sequence of span tables: `{ text, style?, syntax? }`. `style = { hl?, dim?, bold?, italic?, fg?, bg? }` — `hl` is a theme group name; `fg`/`bg` are theme group names whose fg/bg axis is extracted (matches `set_extmark`). `syntax` runs the span text through the inline syntax highlighter (`\"bash\"`, `\"rust\"`, …) and overrides per-character fg; `style` modifiers still apply. An empty span list emits a blank line.",
        &["buf", "lines"],
        lua,
        set_styled_lines,
    )?;

    register_ui_fn(
        &buf_tbl,
        "smelt.buf",
        "set_extmark",
        "Place a highlight or virt-text extmark at `(row, col)` (row is 1-based). `opts` mirrors `nvim_buf_set_extmark`'s keyset; pass `opts.id` to retarget an existing mark. Returns the new extmark id.",
        &["buf", "ns", "row", "col", "opts"],
        lua,
        set_extmark,
    )?;

    register_ui_fn(
        &buf_tbl,
        "smelt.buf",
        "create_namespace",
        "Look up or allocate a stable namespace id for `name`. Repeated calls with the same name return the same id.",
        &["name"],
        lua,
        |_, (name,): (String,)| Ok(smelt_core::buffer::create_namespace(&name).0),
    )?;

    register_ui_fn(
        &buf_tbl,
        "smelt.buf",
        "clear_namespace",
        "Drop every extmark owned by `ns` between `[line_start, line_end)` (1-based, inclusive start, exclusive end). Defaults clear the whole buffer so plugins that repaint a namespace each tick (perf panel, ghost text) don't have to track ids.",
        &["buf", "ns", "line_start", "line_end"],
        lua,
        |_, (id, ns, start, end_): (u64, u32, Option<i64>, Option<i64>)| {
            use smelt_core::buffer::NsId;
            let start_line = match start {
                Some(n) if n > 0 => (n as usize).saturating_sub(1),
                _ => 0,
            };
            let end_line = match end_ {
                Some(n) if n > 0 => n as usize,
                _ => usize::MAX,
            };
            crate::lua::with_app(|app| {
                if let Some(buf) = app.ui.buf_mut(crate::smelt_term::BufId(id)) {
                    buf.clear_namespace(NsId(ns), start_line, end_line);
                }
            });
            Ok(())
        },
    )?;

    smelt.set("buf", buf_tbl)?;
    Ok(())
}

/// `smelt.buf.set_styled_lines(buf, lines)`.
///
/// Each line is a sequence of span tables; each span carries `text` plus optional
/// `style = { hl?, dim?, bold?, italic?, fg?, bg? }` and `syntax` (language token
/// routed through `InlineSyntax`). Replaces the buffer's contents wholesale.
fn set_styled_lines(_: &Lua, (id, lines): (u64, mlua::Table)) -> LuaResult<()> {
    use crate::content::to_buffer::render_into_buffer;
    use crate::smelt_term::BufId;
    use smelt_core::content::highlight::InlineSyntax;
    use smelt_core::style::Style;
    use smelt_core::theme::intern;

    struct SpanStyle {
        hl: Option<String>,
        dim: bool,
        bold: bool,
        italic: bool,
        fg: Option<String>,
        bg: Option<String>,
    }

    struct Span {
        text: String,
        style: SpanStyle,
        syntax: Option<String>,
    }

    fn decode_style(tbl: Option<mlua::Table>) -> LuaResult<SpanStyle> {
        let Some(tbl) = tbl else {
            return Ok(SpanStyle {
                hl: None,
                dim: false,
                bold: false,
                italic: false,
                fg: None,
                bg: None,
            });
        };
        Ok(SpanStyle {
            hl: tbl.get::<Option<String>>("hl")?,
            dim: tbl.get::<Option<bool>>("dim")?.unwrap_or(false),
            bold: tbl.get::<Option<bool>>("bold")?.unwrap_or(false),
            italic: tbl.get::<Option<bool>>("italic")?.unwrap_or(false),
            fg: tbl.get::<Option<String>>("fg")?,
            bg: tbl.get::<Option<String>>("bg")?,
        })
    }

    let mut decoded: Vec<Vec<Span>> = Vec::new();
    for value in lines.sequence_values::<mlua::Value>() {
        let value = value?;
        let line_tbl = match value {
            mlua::Value::Table(t) => t,
            mlua::Value::Nil => {
                decoded.push(Vec::new());
                continue;
            }
            other => {
                return Err(mlua::Error::external(format!(
                    "smelt.buf.set_styled_lines: expected line to be a table of spans, got {}",
                    other.type_name()
                )));
            }
        };
        let mut spans: Vec<Span> = Vec::new();
        for span_val in line_tbl.sequence_values::<mlua::Table>() {
            let span_tbl = span_val?;
            let style = decode_style(span_tbl.get::<Option<mlua::Table>>("style")?)?;
            spans.push(Span {
                text: span_tbl.get::<Option<String>>("text")?.unwrap_or_default(),
                style,
                syntax: span_tbl.get::<Option<String>>("syntax")?,
            });
        }
        decoded.push(spans);
    }

    crate::lua::with_app(|app| {
        let theme_snap = app.ui.theme().clone();
        let width = crate::content::term_width() as u16;
        let Some(buf) = app.ui.buf_mut(BufId(id)) else {
            return;
        };
        // Wipe pre-existing content; render_into_buffer otherwise appends past the seed.
        buf.set_all_lines(Vec::new());
        render_into_buffer(buf, width, &theme_snap, |sink| {
            for spans in &decoded {
                for span in spans {
                    let group = span.style.hl.as_deref().map(intern);
                    let mut style = Style::new();
                    style.dim = span.style.dim;
                    style.bold = span.style.bold;
                    style.italic = span.style.italic;
                    if let Some(fg_group) = &span.style.fg {
                        if let Some(fg) = sink.theme().get(fg_group).fg {
                            style.fg = Some(fg);
                        }
                    }
                    if let Some(bg_group) = &span.style.bg {
                        if let Some(bg) = sink.theme().get(bg_group).bg {
                            style.bg = Some(bg);
                        }
                    }
                    sink.push(group, style);
                    if let Some(lang) = &span.syntax {
                        let mut hi = InlineSyntax::new(lang);
                        hi.print_line(sink, &span.text);
                    } else {
                        sink.print(&span.text);
                    }
                    sink.pop_style();
                }
                sink.newline();
            }
        });
    });
    Ok(())
}

/// `smelt.buf.set_extmark(buf, ns, row, col, opts) -> extmark_id`.
/// `row` is 1-based. `opts.id` retargets an existing mark.
/// Highlight: `hl_group` applies a full style; `fg`/`bg` override individual axes;
/// unknown groups silently resolve to default. VirtText: pass `virt_text`.
fn set_extmark(
    _: &Lua,
    (id, ns, row, col, opts): (u64, u32, u64, u64, Option<LuaExtmarkOpts>),
) -> LuaResult<u64> {
    use crate::smelt_term::BufId;
    use smelt_core::buffer::{ExtmarkId, ExtmarkOpts, NsId};

    let Some(row0) = row.checked_sub(1) else {
        return Ok(0);
    };
    let row0 = row0 as usize;
    let col0 = col as usize;

    let opts = opts.unwrap_or_default();

    let end_row: Option<usize> = opts
        .end_row
        .and_then(|n| n.checked_sub(1).map(|x| x as usize));
    let end_col: Option<usize> = opts.end_col.map(|n| n as usize);
    let mark_id: Option<ExtmarkId> = opts.id.map(ExtmarkId);

    let mut payload_opts = if let Some(text) = opts.virt_text.clone() {
        let mut o = ExtmarkOpts::virt_text(text, opts.virt_text_hl.clone());
        if let Some(pos) = opts.virt_text_pos {
            o = o.with_virt_pos(pos.into());
        }
        o
    } else {
        let style = build_highlight_style(&opts);
        let meta = smelt_core::buffer::SpanMeta {
            selectable: opts.selectable.unwrap_or(true),
            copy_as: opts.yank_as.clone(),
        };
        let mut o = ExtmarkOpts::highlight(end_col.unwrap_or(col0), style, meta);
        if opts.hl_eol == Some(true) {
            o = o.with_hl_eol(true);
        }
        if opts.on_cursor_row == Some(true) {
            o = o.with_on_cursor_row(true);
        }
        o
    };

    payload_opts.end_row = end_row;
    if !matches!(
        payload_opts.payload,
        smelt_core::buffer::ExtmarkPayload::Highlight { .. }
    ) {
        payload_opts.end_col = end_col;
    }
    payload_opts.priority = opts.priority;
    payload_opts.right_gravity = opts.right_gravity.unwrap_or(true);
    payload_opts.end_right_gravity = opts.end_right_gravity.unwrap_or(false);
    payload_opts.id = mark_id;

    let new_id = crate::lua::with_app(|app| {
        app.ui
            .buf_mut(BufId(id))
            .map(|buf| buf.set_extmark(NsId(ns), row0, col0, payload_opts))
    })
    .map(|eid: ExtmarkId| eid.0 as u64)
    .unwrap_or(0);
    Ok(new_id)
}

fn build_highlight_style(opts: &LuaExtmarkOpts) -> crate::smelt_term::SpanStyle {
    use smelt_core::style::Style;

    // Unknown group names silently resolve to default (nvim parity).
    // `hl_group` sets the full base style; `fg`/`bg` override individual color axes.
    let resolve_group =
        |name: &str| -> Style { crate::lua::with_app(|app| app.ui.theme().get(name)) };

    let mut style = match opts.hl_group.as_deref() {
        Some(name) => resolve_group(name),
        None => Style::default(),
    };
    if let Some(name) = opts.fg.as_deref() {
        style.fg = resolve_group(name).fg;
    }
    if let Some(name) = opts.bg.as_deref() {
        style.bg = resolve_group(name).bg;
    }
    if let Some(b) = opts.bold {
        style.bold = b;
    }
    if let Some(b) = opts.dim {
        style.dim = b;
    }
    if let Some(b) = opts.italic {
        style.italic = b;
    }
    style
}
