const PS_LUA: &str = include_str!("../../../runtime/lua/smelt/commands/ps.lua");
const LABEL_VALUE_LUA: &str = include_str!("../../../runtime/lua/smelt/label_value.lua");
const SESSION_LUA: &str = include_str!("../../../runtime/lua/smelt/session.lua");

#[test]
fn session_tree_orders_nested_forks_and_prefixes() {
    let lua = mlua::Lua::new();
    lua.load("smelt = { session = {} }")
        .exec()
        .expect("init smelt table");
    lua.load(SESSION_LUA).exec().expect("load session helpers");

    let rows: mlua::Table = lua
        .load(
            r#"
            local entries = {
              { id = "old", updated_at_ms = 1, created_at_ms = 1 },
              { id = "root", updated_at_ms = 2, created_at_ms = 2 },
              { id = "fork_a", parent_id = "root", updated_at_ms = 3, created_at_ms = 3 },
              { id = "fork_b", parent_id = "root", updated_at_ms = 4, created_at_ms = 4 },
              { id = "nested", parent_id = "fork_b", updated_at_ms = 5, created_at_ms = 5 },
            }
            local out = smelt.session.tree(entries, { order = "asc" })
            local rows = {}
            for i, e in ipairs(out) do
              rows[i] = (e.tree_prefix or "") .. e.id .. ":" .. tostring(e.tree_sort_value)
            end
            return rows
            "#,
        )
        .eval()
        .expect("evaluate tree");
    let got: Vec<String> = rows
        .sequence_values::<String>()
        .collect::<Result<_, _>>()
        .expect("rows");

    assert_eq!(
        got,
        [
            "old:1",
            "root:5",
            "├─ fork_a:3",
            "└─ fork_b:5",
            "   └─ nested:5",
        ]
    );
}

#[test]
fn ps_details_dialog_uses_list_dialog_height() {
    assert!(PS_LUA.contains("local DIALOG_HEIGHT = \"60%\""));
    assert!(PS_LUA.contains("height = DIALOG_HEIGHT"));
    assert!(PS_LUA.contains("height      = DIALOG_HEIGHT"));
    assert!(!PS_LUA.contains("max_height = \"70%\""));
}

#[test]
fn ps_details_meta_values_are_label_value_rows() {
    assert!(
        PS_LUA.contains("local label_value = smelt.label_value or require(\"smelt.label_value\")")
    );
    assert!(PS_LUA.contains("append_label_value(lines, \"command\""));
    assert!(LABEL_VALUE_LUA.contains("local separator = opts.separator or \"  \""));
    assert!(!PS_LUA.contains("key .. \":\""));
    assert!(!PS_LUA.contains("META_KEY_WIDTH"));
    assert!(!PS_LUA.contains("styled_lines ="));
}
