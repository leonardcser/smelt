use super::*;

#[test]
fn file_rows_preserve_provider_data_and_explicit_presentation() {
    let mut app = TestApp::builder().build();
    app.run_lua_result(r##"
        local file_row = require('smelt.widgets.file_row')
        local item = { label = 'source\nname.rs', path = 'source\nname.rs', kind = 'file' }
        smelt.settings.file_icons = true
        smelt.settings.file_icon_colors = true
        local row = file_row.picker(item)
        assert(row.label == 'source name.rs' and row.prefix ~= nil)
        assert(item.label == 'source\nname.rs' and item.prefix == nil and item.prefix_style == nil)
        smelt.settings.file_icons = false
        assert(file_row.picker(item).prefix == nil)
        smelt.settings.file_icons = true
        smelt.settings.file_icon_colors = false
        assert(file_row.picker(item).prefix_style == nil)
        smelt.settings.file_icon_colors = true
        local dir = file_row.picker({ label = 'src/', path = 'src', kind = 'dir' })
        assert(dir.prefix == ' ' and type(dir.prefix_style) == 'table')
        smelt.picker.new({ items = { dir }, placement = 'center' }):close()
        smelt.theme.set('Directory', { fg = '#123456', bold = true, italic = true })
        dir = file_row.picker({ label = 'src/', path = 'src', kind = 'dir' })
        assert(dir.prefix_style.bold and dir.prefix_style.italic)
        local loading = file_row.picker({ label = 'searching…' })
        assert(loading.prefix == nil)
        smelt.path.icon = function() error('explicit prefix must bypass icon lookup') end
        local explicit = { label = 'custom', path = 'custom.rs', prefix = '', prefix_style = { fg = false }, label_style = { fg = '#123456', bold = true } }
        row = file_row.picker(explicit)
        assert(row.prefix == '' and row.prefix_style.fg == false and row.label_style.bold)
        local spans = file_row.render(explicit).spans
        assert(spans[1].text == '' and spans[2].style.fg == '#123456')
    "##).unwrap();
}

#[test]
fn completer_renders_display_rows_without_changing_accepted_candidates() {
    let mut app = TestApp::builder().build();
    app.run_lua_result(r##"
        original = { label = 'original candidate', id = 'stable-id' }
        local synthetic = { label = 'provider status', _synthetic = true }
        smelt.prompt.register_completer({
            detect = function(text) if text == '~' then return 0 end end,
            matches = function() return { original, synthetic } end,
            prefix = 'default ',
            render = function(item)
                assert(item == original)
                return { label = 'custom row', prefix = '', prefix_style = { fg = false }, label_style = { bold = true, underline = true }, description = 'custom description' }
            end,
            on_select = function(item) assert(item == original) end,
            accept = function(item) accepted = item end,
        })
        local ok, err = pcall(smelt.prompt.register_completer, {
            detect = function() end, items = function() return {} end,
            query = function() return '' end, accept = function() end, render = false,
        })
        assert(not ok and tostring(err):find('spec.render must be a function', 1, true))
    "##).unwrap();
    app.type_text("~");
    let grid = app.render_to_frame();
    let row = grid
        .rows
        .iter()
        .position(|line| line.contains("custom row"))
        .unwrap();
    let col = grid.rows[row].find("custom row").unwrap();
    assert!(grid.styles[row][col].bold && grid.styles[row][col].underline);
    let frame = grid.text();
    assert!(
        frame.contains("custom row") && frame.contains("custom description"),
        "{frame}"
    );
    assert!(frame.contains("provider status"), "{frame}");
    assert!(
        !frame.contains("original candidate") && !frame.contains("default custom row"),
        "{frame}"
    );
    app.press(KeyCode::Enter);
    app.run_lua_result(
        r#"
        assert(accepted == original and accepted.id == 'stable-id')
        assert(original.label == 'original candidate' and original.prefix == nil)
    "#,
    )
    .unwrap();
}

#[test]
fn picker_renderers_preserve_original_items_for_floating_and_prompt_pickers() {
    for placement in ["center", "prompt_docked"] {
        let mut app = TestApp::builder().build();
        app.run_lua_result(&format!(r#"
            original = {{ label = 'original candidate', id = 'stable' }}
            local other = {{ label = 'unrelated candidate', id = 'other' }}
            smelt.theme.set('Directory', {{ fg = '#123456', bold = true, italic = true }})
            smelt.spawn(function()
                local result = smelt.picker.open({{
                    placement = '{placement}', items = {{ original, other }},
                    render = function(item)
                        assert(item == original or item == other)
                        if '{placement}' == 'center' then return 'custom ' .. item.id end
                        return {{ label = 'custom ' .. item.id, prefix = 'folder ', prefix_style = 'Directory' }}
                    end,
                    on_select = function(item) assert(item == original) end,
                }})
                assert(result.item == original and result.index == 1)
                assert(original.label == 'original candidate' and original.prefix == nil)
                render_accepted = true
            end)
        "#)).unwrap();
        drive_lua_tasks(&mut app);
        if placement == "prompt_docked" {
            app.type_text("original");
        }
        let grid = app.render_to_frame();
        if placement == "prompt_docked" {
            let row = grid
                .rows
                .iter()
                .position(|line| line.contains("folder custom stable"))
                .unwrap();
            let col = grid.rows[row].find("folder").unwrap();
            assert!(grid.styles[row][col].bold && grid.styles[row][col].italic);
        }
        let frame = grid.text();
        assert!(
            frame.contains("custom stable") && !frame.contains("original candidate"),
            "{frame}"
        );
        app.press(KeyCode::Enter);
        drive_lua_tasks(&mut app);
        app.run_lua_result("assert(render_accepted)").unwrap();
    }
}

#[test]
fn prompt_picker_preserves_duplicate_string_index() {
    let mut app = TestApp::builder().build();
    app.run_lua_result(
        r#"
        smelt.spawn(function()
            local result = smelt.picker.open({
                placement = 'prompt_docked', items = { 'duplicate', 'duplicate' }, selected = 2,
                render = function(item) assert(type(item) == 'string'); return item end,
            })
            assert(result.item == 'duplicate' and result.index == 2)
            string_accepted = true
        end)
    "#,
    )
    .unwrap();
    drive_lua_tasks(&mut app);
    app.press(KeyCode::Enter);
    drive_lua_tasks(&mut app);
    app.run_lua_result("assert(string_accepted)").unwrap();
}

#[test]
fn shared_file_row_override_customizes_finder_and_both_completers() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("alpha.rs"), "fn alpha() {}\n").unwrap();
    std::fs::write(dir.path().join("beta.rs"), "fn beta() {}\n").unwrap();
    for mode in ["finder", "at", "path"] {
        let mut app = TestApp::builder().with_cwd(dir.path()).build();
        app.set_terminal_size(120, 30);
        app.run_lua_result(r#"
            require('smelt.widgets.file_row').picker = function(item)
                return { label = 'custom ' .. item.label, prefix = '> ' }
            end
            smelt.files.search = function()
                return { items = { { label = 'alpha.rs', path = 'alpha.rs', kind = 'file' } }, total_matched = 1, total_files = 1 }
            end
        "#).unwrap();
        match mode {
            "finder" => app.run_lua_result("smelt.cmd.run('files')").unwrap(),
            "at" => app.type_text("@"),
            "path" => {
                app.type_text("./");
                app.press(KeyCode::Tab);
            }
            _ => unreachable!(),
        }
        let frame = app.render_to_frame().text();
        assert!(frame.contains("> custom alpha.rs"), "{mode}:\n{frame}");
    }
}
