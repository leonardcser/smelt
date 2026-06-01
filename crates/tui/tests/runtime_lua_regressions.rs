const PS_LUA: &str = include_str!("../../../runtime/lua/smelt/commands/ps.lua");

#[test]
fn ps_details_dialog_uses_list_dialog_height() {
    assert!(PS_LUA.contains("local DIALOG_HEIGHT = \"60%\""));
    assert!(PS_LUA.contains("height = DIALOG_HEIGHT"));
    assert!(PS_LUA.contains("height      = DIALOG_HEIGHT"));
    assert!(!PS_LUA.contains("max_height = \"70%\""));
}

#[test]
fn ps_details_meta_values_are_left_aligned() {
    assert!(PS_LUA.contains("local META_KEY_WIDTH = 10"));
    assert!(
        PS_LUA.contains("string.format(\"%-\" .. tostring(META_KEY_WIDTH) .. \"s\", key .. \":\")")
    );
    assert!(!PS_LUA.contains("{ text = \" \" .. format_duration"));
    assert!(!PS_LUA.contains("{ text = \" \" .. output_state"));
}
