//! Markdown rendering inside assistant text blocks. Each story
//! drives `EngineEvent::Text` so the renderer goes through the real
//! `parse_inline_spans` / `render_markdown_table` / `render_code_block`
//! pipeline. The snapshot captures spacing, chrome, and highlight
//! attribution byte-for-byte.

use protocol::EngineEvent;

use crate::app_story;

app_story!(text_block_markdown_with_code_fence, |ctx| {
    // Fenced code blocks become `Block::CodeLine`s during streaming,
    // which the renderer pipes through `render_code_block` — syntax
    // highlighting in the snapshot proves the highlight pipeline is
    // wired end-to-end (not just inside the dialog diff renderer).
    ctx.set_viewport(60, 16);
    ctx.engine(EngineEvent::Text {
        content: "Here's the fix:\n\n```rust\nfn add(a: i64, b: i64) -> i64 {\n    a.checked_add(b).expect(\"overflow\")\n}\n```\n\nDone.".into(),
    });
    ctx.assert_snapshot();
});

app_story!(text_block_markdown_headings, |ctx| {
    // H1–H3 hit the `SmeltHeading` highlight group (bold).
    ctx.set_viewport(50, 12);
    ctx.engine(EngineEvent::Text {
        content: "# Title\n## Section\n### Subsection\n\nBody text follows.".into(),
    });
    ctx.assert_snapshot();
});

app_story!(text_block_markdown_lists, |ctx| {
    // Bulleted, numbered, and a nested item. List prefixes (`- `,
    // `1. `, indented `- `) render dim via `split_list_prefix`.
    ctx.set_viewport(50, 14);
    ctx.engine(EngineEvent::Text {
        content: "- first item\n- second item\n  - nested item\n\n1. ordered one\n2. ordered two"
            .into(),
    });
    ctx.assert_snapshot();
});

app_story!(text_block_markdown_blockquote, |ctx| {
    // `> …` lines render dim + italic.
    ctx.set_viewport(60, 10);
    ctx.engine(EngineEvent::Text {
        content: "Quoting the docs:\n\n> Always validate at the boundary.\n> Never trust caller input.\n\nGot it.".into(),
    });
    ctx.assert_snapshot();
});

app_story!(text_block_markdown_table, |ctx| {
    // Pipe-table runs through `render_markdown_table` — separate path
    // from regular line rendering, complete with alignment + borders.
    ctx.set_viewport(60, 12);
    ctx.engine(EngineEvent::Text {
        content: "| feature | status | notes |\n|---|---|---|\n| parser | done | streaming |\n| renderer | wip | tables |".into(),
    });
    ctx.assert_snapshot();
});

app_story!(text_block_markdown_horizontal_rule, |ctx| {
    // `---` between paragraphs renders via `render_horizontal_rule`.
    ctx.set_viewport(50, 10);
    ctx.engine(EngineEvent::Text {
        content: "Before the rule.\n\n---\n\nAfter the rule.".into(),
    });
    ctx.assert_snapshot();
});

app_story!(text_block_markdown_inline_emphasis, |ctx| {
    // `parse_inline_spans` covers **bold**, *italic*, `code`, and
    // ~~strikethrough~~. The styles snapshot captures each attr span.
    ctx.set_viewport(60, 8);
    ctx.engine(EngineEvent::Text {
        content: "Use **bold** for emphasis, *italic* for nuance, `code` for symbols, and ~~strike~~ to retract.".into(),
    });
    ctx.assert_snapshot();
});

app_story!(text_block_markdown_link, |ctx| {
    // `[label](url)` becomes an inline link span via `parse_inline_spans`.
    // Both autolinks (`<https://…>`) and inline links should land on the
    // accent group so they're visually distinct from body text.
    ctx.set_viewport(60, 8);
    ctx.engine(EngineEvent::Text {
        content:
            "See the [docs](https://example.com/docs) or visit <https://example.com> directly."
                .into(),
    });
    ctx.assert_snapshot();
});

app_story!(text_block_markdown_soft_wrap, |ctx| {
    // A single long paragraph forces the renderer to wrap on word
    // boundaries inside the transcript's content width. Catches off-by-
    // one bugs in the wrap accumulator and the chrome's right margin.
    ctx.set_viewport(50, 10);
    ctx.engine(EngineEvent::Text {
        content: "This is one long paragraph that has to wrap across several visible rows because the viewport is narrower than the sentence and we want to exercise the soft-wrap path through render_paragraph.".into(),
    });
    ctx.assert_snapshot();
});

app_story!(text_block_markdown_mixed_nested_lists, |ctx| {
    // Bulleted list with a nested ordered list and an ordered list with
    // a nested bullet. Exercises `split_list_prefix` recursion on both
    // marker kinds at depth.
    ctx.set_viewport(50, 14);
    ctx.engine(EngineEvent::Text {
        content: "- top bullet\n  1. nested ordered one\n  2. nested ordered two\n- another top bullet\n\n1. ordered top\n   - nested bullet\n   - nested bullet two\n2. ordered tail".into(),
    });
    ctx.assert_snapshot();
});

app_story!(text_block_markdown_wide_chars, |ctx| {
    // CJK + emoji exercise the wide-cell path: each glyph occupies two
    // columns, so the wrap accumulator must count visual width (not
    // chars). Regressions here surface as either chrome that's too short
    // or text that runs past the right edge.
    ctx.set_viewport(40, 10);
    ctx.engine(EngineEvent::Text {
        content: "日本語のテスト 🚀\n\n绝对要测试一下中文换行 with mixed ascii.".into(),
    });
    ctx.assert_snapshot();
});

app_story!(text_block_markdown_nested_code_fence, |ctx| {
    // Multiple fenced blocks back to back stress the streaming code-
    // line boundary: each fence must close cleanly so the next fence
    // restarts highlighting in the correct language.
    ctx.set_viewport(60, 18);
    ctx.engine(EngineEvent::Text {
        content: "First in rust:\n\n```rust\nlet x = 1;\n```\n\nThen in python:\n\n```python\nx = 1\nprint(x)\n```\n\nDone.".into(),
    });
    ctx.assert_snapshot();
});

app_story!(text_block_markdown_full_document, |ctx| {
    // End-to-end smoke for the markdown renderer: heading + paragraph
    // with inline marks + bullet list + blockquote + hr + fenced code
    // + table, in production order. Catches spacing/seam regressions
    // between block kinds that single-feature stories miss.
    ctx.set_viewport(70, 30);
    ctx.engine(EngineEvent::Text {
        content: concat!(
            "# Release notes\n",
            "\n",
            "The **0.6** release ships *streaming markdown* with `inline code` and\n",
            "~~legacy~~ paths removed.\n",
            "\n",
            "## Highlights\n",
            "- table renderer matches the dialog one\n",
            "- syntax highlighting in fenced code\n",
            "- ~~bug~~ fix: trailing newline in headings\n",
            "\n",
            "> Heads up: the API changed in one place — see below.\n",
            "\n",
            "---\n",
            "\n",
            "```rust\n",
            "fn run() -> Result<()> {\n",
            "    println!(\"hello\");\n",
            "    Ok(())\n",
            "}\n",
            "```\n",
            "\n",
            "| change | who |\n",
            "|---|---|\n",
            "| parser | core |\n",
            "| diff   | tui |"
        )
        .into(),
    });
    ctx.assert_snapshot();
});
