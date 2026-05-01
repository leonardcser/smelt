//! Confirm dialog — built-in tool approvals.
//!
//! The dialog itself lives in `runtime/lua/smelt/dialogs/confirm.lua`.
//! This file holds the one Rust-side primitive the Lua orchestrator
//! still calls — [`render_title_into_buf`], which fills the title
//! buffer with ` tool: desc Allow?` (bash-highlit when the tool is
//! `bash`). The inline bash highlight on the desc keeps title
//! composition Rust-side until we have a span-level Lua API.
//!
//! Option labels (yes / no + dynamic "always allow …" entries) and
//! the `~/`-rewritten cwd label are built in Lua directly from the
//! request payload (`outside_dir` + `approval_patterns` +
//! `smelt.os.{cwd,home}`); resolution rides a stable decision-label
//! string (`"yes"` / `"always_session"` / …) — see
//! `lua/api/confirm.rs::parse_decision`.
//!
//! Summary + preview buffers are composed in Lua via
//! `smelt.{diff,syntax,bash,notebook}.render`.
//!
//! Plugin tools drive their own dialogs through `smelt.ui.dialog.open`.

use super::super::TuiApp;
use crate::app::transcript_model::ConfirmRequest;
use crate::content::display::{ColorRole, ColorValue};
use crate::content::layout_out::SpanCollector;
use ui::BufId;

/// Render the ` tool: desc Allow?` title into `buf_id`. The tool name
/// shows in the accent color; the desc is bash-highlit when the tool
/// is `bash`. Multi-line bash commands show only the first line in the
/// title — the rest renders into the preview panel via
/// `smelt.bash.render`.
///
/// Lua creates the buffer via `smelt.buf.create` and asks Rust to
/// fill it; the inline bash highlight on the desc keeps title
/// composition Rust-side until we have a span-level Lua API.
pub(crate) fn render_title_into_buf(app: &mut TuiApp, buf_id: BufId, req: &ConfirmRequest) {
    let theme_snap = app.ui.theme().clone();
    let width = crate::content::term_width() as u16;
    let is_bash = req.tool_name == "bash";
    let multi_line_bash = is_bash && req.desc.lines().count() > 1;

    if let Some(buf) = app.ui.buf_mut(buf_id) {
        crate::content::to_buffer::render_into_buffer(buf, width, &theme_snap, |sink| {
            render_title(sink, &req.tool_name, &req.desc, multi_line_bash, is_bash);
            sink.print(" Allow?");
            sink.newline();
        });
    }
}

fn render_title(
    sink: &mut SpanCollector,
    tool_name: &str,
    desc: &str,
    truncate_to_first_line: bool,
    is_bash: bool,
) {
    use crate::content::highlight::BashHighlighter;
    let shown = if truncate_to_first_line {
        desc.lines().next().unwrap_or("")
    } else {
        desc
    };
    sink.print(" ");
    sink.push_fg(ColorValue::Role(ColorRole::Accent));
    sink.print(tool_name);
    sink.pop_style();
    sink.print(": ");
    if is_bash {
        let mut bh = BashHighlighter::new();
        bh.print_line(sink, shown);
    } else {
        sink.print(shown);
    }
    sink.newline();
}
