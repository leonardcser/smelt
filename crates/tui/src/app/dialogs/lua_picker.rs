//! Picker float opener for `smelt.api.picker.open` yields. Rust parses
//! `opts` into `PickerItem`s and opens the focusable float; everything
//! else — Up/Down/Ctrl-J/K/Enter/Esc navigation, selection tracking,
//! resolution — lives in `runtime/lua/smelt/picker.lua`, which drives
//! `Ui::picker_set_selected` through `smelt.api.picker.set_selected`
//! and resumes the parked task via `smelt.api.task.resume`.

use super::super::App;
use ui::{Constraint, FloatConfig, Placement, WinId};

/// Open the picker float described by the Lua `opts_key` table. On
/// success, returns the new `WinId` so the caller can resolve the
/// parked task. Returns `Err` when the table is malformed.
pub fn open(app: &mut App, opts_key: mlua::RegistryKey) -> Result<WinId, String> {
    let lua = app.lua.lua();
    let opts: mlua::Table = lua
        .registry_value(&opts_key)
        .map_err(|e| format!("picker opts: {e}"))?;

    let items_tbl: mlua::Table = opts
        .get("items")
        .map_err(|e| format!("picker items: {e}"))?;
    let mut items: Vec<ui::picker::PickerItem> = Vec::new();
    for pair in items_tbl.sequence_values::<mlua::Value>() {
        let v = pair.map_err(|e| format!("picker item: {e}"))?;
        items.push(parse_item(&v)?);
    }
    if items.is_empty() {
        return Err("picker.open: items must be non-empty".into());
    }

    let placement = parse_placement(&opts);
    let title: Option<String> = opts.get("title").ok();

    let float_config = FloatConfig {
        title,
        border: ui::Border::Rounded,
        placement,
        focusable: true,
        blocks_agent: false,
        ..Default::default()
    };

    // Lua picker floats default to non-reversed (top-down) since they
    // don't necessarily dock above a prompt — the plugin controls the
    // placement.
    let win_id = app
        .ui
        .picker_open(float_config, items, 0, Default::default(), false)
        .ok_or_else(|| "picker.open: failed to create float".to_string())?;

    Ok(win_id)
}

fn parse_item(v: &mlua::Value) -> Result<ui::picker::PickerItem, String> {
    match v {
        mlua::Value::String(s) => Ok(ui::picker::PickerItem::new(s.to_string_lossy().to_string())),
        mlua::Value::Table(t) => {
            let label: String = t
                .get("label")
                .map_err(|e| format!("picker item.label: {e}"))?;
            let mut item = ui::picker::PickerItem::new(label);
            if let Ok(desc) = t.get::<String>("description") {
                item = item.with_description(desc);
            }
            if let Ok(prefix) = t.get::<String>("prefix") {
                item = item.with_prefix(prefix);
            }
            Ok(item)
        }
        other => Err(format!(
            "picker item: expected string or table, got {}",
            other.type_name()
        )),
    }
}

fn parse_placement(opts: &mlua::Table) -> Placement {
    let mode: String = opts
        .get("placement")
        .ok()
        .unwrap_or_else(|| "center".to_string());
    match mode.as_str() {
        "bottom" => Placement::dock_bottom_full_width(Constraint::Pct(40)),
        "cursor" => Placement::AnchorCursor {
            row_offset: 1,
            col_offset: 0,
            width: Constraint::Fixed(48),
            height: Constraint::Pct(40),
        },
        _ => Placement::centered(Constraint::Pct(60), Constraint::Pct(50)),
    }
}
