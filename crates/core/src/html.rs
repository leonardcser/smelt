//! HTML capability - pure parsing over `scraper`, no I/O. Exposes title,
//! links, plain text, and a markdown projection for LLM consumption.

use scraper::{ElementRef, Html, Selector};
use smelt_buffer::{cell_width, text};
use std::collections::HashSet;
use url::Url;

const SKIP_ELEMENTS: &[&str] = &[
    "script", "style", "noscript", "iframe", "object", "embed", "meta", "link", "svg",
];

fn trim_owned(value: String) -> String {
    text::trim_whitespace(&value).to_owned()
}

pub(crate) fn title(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("title").ok()?;
    doc.select(&sel)
        .next()
        .map(|el| trim_owned(el.text().collect()))
        .filter(|s| !s.is_empty())
}

/// Extract `<a href>` targets, resolved against `base_url`. Output preserves insertion order, deduplicated.
pub(crate) fn links(html: &str, base_url: Option<&str>) -> Vec<String> {
    let doc = Html::parse_document(html);
    let Ok(sel) = Selector::parse("a[href]") else {
        return Vec::new();
    };

    let base = base_url.and_then(|s| Url::parse(s).ok());
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();

    for el in doc.select(&sel) {
        let Some(href) = el.value().attr("href") else {
            continue;
        };
        let resolved = match &base {
            Some(b) => b.join(href).map(|u| u.to_string()).unwrap_or_default(),
            None => href.to_string(),
        };
        if resolved.is_empty() {
            continue;
        }
        if seen.insert(resolved.clone()) {
            out.push(resolved);
        }
    }
    out
}

#[derive(Debug, Clone)]
pub(crate) struct DdgResult {
    pub(crate) title: String,
    pub(crate) link: String,
    pub(crate) description: String,
}

/// Parse `html.duckduckgo.com/html/` results. Returns at most 20 entries;
/// rows with empty title or unresolvable link are skipped.
pub(crate) fn parse_ddg_results(html: &str) -> Vec<DdgResult> {
    let doc = Html::parse_document(html);
    let result_sel = match Selector::parse("div.result, div.web-result") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let title_sel = match Selector::parse("a.result__a") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let snippet_sel = match Selector::parse("a.result__snippet") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let mut results = Vec::new();
    for el in doc.select(&result_sel) {
        if results.len() >= 20 {
            break;
        }
        let Some(title_el) = el.select(&title_sel).next() else {
            continue;
        };
        let title = trim_owned(title_el.text().collect());
        if title.is_empty() {
            continue;
        }
        let raw_href = title_el.value().attr("href").unwrap_or("");
        let link = extract_ddg_url(raw_href);
        if link.is_empty() {
            continue;
        }
        let description = el
            .select(&snippet_sel)
            .next()
            .map(|s| trim_owned(s.text().collect()))
            .unwrap_or_default();
        results.push(DdgResult {
            title,
            link,
            description,
        });
    }
    results
}

fn extract_ddg_url(ddg_url: &str) -> String {
    if ddg_url.contains("uddg=") {
        if let Some(start) = ddg_url.find("uddg=") {
            let after = &ddg_url[start + 5..];
            let encoded = if let Some(end) = after.find('&') {
                &after[..end]
            } else {
                after
            };
            return url::form_urlencoded::parse(encoded.as_bytes())
                .next()
                .map(|(k, v)| {
                    if v.is_empty() {
                        k.to_string()
                    } else {
                        format!("{k}={v}")
                    }
                })
                .unwrap_or_default();
        }
    }
    if ddg_url.starts_with("http://") || ddg_url.starts_with("https://") {
        return ddg_url.to_string();
    }
    String::new()
}

/// Plain-text DOM projection: skips script/style, collapses whitespace,
/// block elements introduce newlines. Not a faithful renderer.
pub(crate) fn to_text(html: &str) -> String {
    let doc = Html::parse_document(html);
    let mut out = String::new();
    if let Some(root) = doc.tree.root().first_child() {
        walk(&root, &mut out);
    }
    collapse_whitespace(&out)
}

fn walk(node: &ego_tree::NodeRef<scraper::node::Node>, out: &mut String) {
    use scraper::node::Node;
    match node.value() {
        Node::Element(el) => {
            let name = el.name();
            if SKIP_ELEMENTS.contains(&name) {
                return;
            }
            let block = matches!(
                name,
                "p" | "div"
                    | "br"
                    | "li"
                    | "tr"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "section"
                    | "article"
                    | "header"
                    | "footer"
                    | "blockquote"
            );
            for child in node.children() {
                walk(&child, out);
            }
            if block {
                out.push('\n');
            }
        }
        Node::Text(text) => {
            out.push_str(text);
        }
        _ => {
            for child in node.children() {
                walk(&child, out);
            }
        }
    }
}

/// Markdown projection of a fetched page: title, body as markdown, and
/// outbound links (deduplicated, fragment-stripped, capped at 50; `javascript:` /
/// `mailto:` / `tel:` / fragment-only targets dropped).
#[derive(Debug, Clone)]
pub(crate) struct Markdown {
    pub(crate) title: Option<String>,
    pub(crate) content: String,
    pub(crate) links: Vec<String>,
}

pub(crate) fn to_markdown(html: &str, base_url: Option<&str>) -> Markdown {
    let doc = Html::parse_document(html);

    let title = Selector::parse("title").ok().and_then(|sel| {
        doc.select(&sel)
            .next()
            .map(|el| trim_owned(el.text().collect()))
            .filter(|s| !s.is_empty())
    });

    let base = base_url.and_then(|s| Url::parse(s).ok());
    let mut links: Vec<String> = Vec::new();
    if let (Some(base), Ok(sel)) = (base.as_ref(), Selector::parse("a[href]")) {
        let mut seen: HashSet<String> = HashSet::new();
        for el in doc.select(&sel) {
            if links.len() >= 50 {
                break;
            }
            let Some(href) = el.value().attr("href") else {
                continue;
            };
            let href = text::trim_whitespace(href);
            if href.is_empty()
                || href.starts_with("javascript:")
                || href.starts_with("mailto:")
                || href.starts_with("tel:")
                || href.starts_with('#')
            {
                continue;
            }
            let Ok(mut resolved) = base.join(href) else {
                continue;
            };
            resolved.set_fragment(None);
            let s = resolved.to_string();
            if seen.insert(s.clone()) {
                links.push(s);
            }
        }
    }

    let content = match Selector::parse("body")
        .ok()
        .and_then(|s| doc.select(&s).next())
    {
        Some(body) => {
            let mut out = String::new();
            html_to_md(body, &mut out);
            collapse_blank_lines(&out)
        }
        None => to_text(html),
    };

    Markdown {
        title,
        content,
        links,
    }
}

fn html_to_md(el: ElementRef, out: &mut String) {
    let tag = el.value().name();
    if SKIP_ELEMENTS.contains(&tag) {
        return;
    }

    match tag {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = tag[1..].parse::<usize>().unwrap_or(1);
            out.push('\n');
            for _ in 0..level {
                out.push('#');
            }
            out.push(' ');
            collect_inline_text(el, out);
            out.push_str("\n\n");
        }
        "p" | "div" | "section" | "article" | "main" | "header" | "footer" | "nav" | "aside" => {
            let is_block = matches!(tag, "p" | "div");
            if is_block {
                ensure_blank_line(out);
            }
            walk_children(el, out);
            if is_block {
                out.push('\n');
            }
        }
        "br" => out.push('\n'),
        "hr" => out.push_str("\n---\n\n"),
        "a" => {
            let href = el.value().attr("href").unwrap_or("");
            let mut link_text = String::new();
            collect_inline_text(el, &mut link_text);
            let trimmed_link_text = text::trim_whitespace(&link_text);
            if trimmed_link_text.is_empty() {
                out.push_str(href);
            } else if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
                out.push_str(&link_text);
            } else {
                out.push('[');
                out.push_str(trimmed_link_text);
                out.push_str("](");
                out.push_str(href);
                out.push(')');
            }
        }
        "img" => {
            let alt = el.value().attr("alt").unwrap_or("");
            let src = el.value().attr("src").unwrap_or("");
            if !src.is_empty() {
                out.push_str("![");
                out.push_str(alt);
                out.push_str("](");
                out.push_str(src);
                out.push(')');
            }
        }
        "strong" | "b" => {
            out.push_str("**");
            collect_inline_text(el, out);
            out.push_str("**");
        }
        "em" | "i" => {
            out.push('*');
            collect_inline_text(el, out);
            out.push('*');
        }
        "code" => {
            out.push('`');
            collect_inline_text(el, out);
            out.push('`');
        }
        "pre" => {
            ensure_blank_line(out);
            out.push_str("```\n");
            for desc in el.descendants() {
                if let Some(t) = desc.value().as_text() {
                    out.push_str(t);
                }
            }
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n\n");
        }
        "ul" | "ol" => {
            ensure_blank_line(out);
            let ordered = tag == "ol";
            let mut idx = 1u32;
            for child in el.children() {
                if let Some(li) = ElementRef::wrap(child) {
                    if li.value().name() == "li" {
                        if ordered {
                            out.push_str(&format!("{idx}. "));
                            idx += 1;
                        } else {
                            out.push_str("- ");
                        }
                        collect_inline_text(li, out);
                        out.push('\n');
                    }
                }
            }
            out.push('\n');
        }
        "blockquote" => {
            ensure_blank_line(out);
            let mut inner = String::new();
            walk_children(el, &mut inner);
            for line in text::trim_whitespace(&inner).lines() {
                out.push_str("> ");
                out.push_str(line);
                out.push('\n');
            }
            out.push('\n');
        }
        "table" => {
            ensure_blank_line(out);
            render_table(el, out);
            out.push('\n');
        }
        _ => walk_children(el, out),
    }
}

fn walk_children(el: ElementRef, out: &mut String) {
    for child in el.children() {
        if let Some(t) = child.value().as_text() {
            out.push_str(t);
        } else if let Some(child_el) = ElementRef::wrap(child) {
            html_to_md(child_el, out);
        }
    }
}

fn collect_inline_text(el: ElementRef, out: &mut String) {
    for child in el.children() {
        if let Some(t) = child.value().as_text() {
            out.push_str(t);
        } else if let Some(child_el) = ElementRef::wrap(child) {
            let tag = child_el.value().name();
            if SKIP_ELEMENTS.contains(&tag) {
                continue;
            }
            match tag {
                "strong" | "b" => {
                    out.push_str("**");
                    collect_inline_text(child_el, out);
                    out.push_str("**");
                }
                "em" | "i" => {
                    out.push('*');
                    collect_inline_text(child_el, out);
                    out.push('*');
                }
                "code" => {
                    out.push('`');
                    collect_inline_text(child_el, out);
                    out.push('`');
                }
                "a" => html_to_md(child_el, out),
                "br" => out.push('\n'),
                _ => collect_inline_text(child_el, out),
            }
        }
    }
}

fn ensure_blank_line(out: &mut String) {
    if out.is_empty() {
        return;
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.ends_with("\n\n") {
        out.push('\n');
    }
}

fn render_table(table: ElementRef, out: &mut String) {
    let row_sel = match Selector::parse("tr") {
        Ok(s) => s,
        Err(_) => return,
    };
    let th_sel = match Selector::parse("th") {
        Ok(s) => s,
        Err(_) => return,
    };
    let td_sel = match Selector::parse("td") {
        Ok(s) => s,
        Err(_) => return,
    };

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut has_header = false;

    for row in table.select(&row_sel) {
        let ths: Vec<String> = row
            .select(&th_sel)
            .map(|c| trim_owned(c.text().collect()))
            .collect();
        if !ths.is_empty() {
            has_header = true;
            rows.push(ths);
            continue;
        }
        let tds: Vec<String> = row
            .select(&td_sel)
            .map(|c| trim_owned(c.text().collect()))
            .collect();
        if !tds.is_empty() {
            rows.push(tds);
        }
    }

    if rows.is_empty() {
        return;
    }

    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    for row in &rows {
        out.push('|');
        for i in 0..cols {
            out.push(' ');
            out.push_str(row.get(i).map(|s| s.as_str()).unwrap_or(""));
            out.push_str(" |");
        }
        out.push('\n');
        if has_header && std::ptr::eq(row, &rows[0]) {
            out.push('|');
            for _ in 0..cols {
                out.push_str(" --- |");
            }
            out.push('\n');
        }
    }
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_count = 0u32;
    for line in s.lines() {
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                out.push('\n');
            }
        } else {
            blank_count = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    text::trim_whitespace(&out).to_owned()
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = true;
    let mut last_was_newline = true;
    for grapheme in cell_width::graphemes(s) {
        if grapheme.chars().all(char::is_whitespace) {
            if grapheme.contains('\n') || grapheme.contains('\r') {
                if !last_was_newline {
                    out.push('\n');
                }
                last_was_newline = true;
                last_was_space = true;
            } else if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push_str(grapheme);
            last_was_space = false;
            last_was_newline = false;
        }
    }
    text::trim_whitespace(&out).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_finds_document_title() {
        let html = "<html><head><title>  hello  </title></head><body></body></html>";
        assert_eq!(title(html), Some("hello".into()));
    }

    #[test]
    fn html_whitespace_normalization_keeps_graphemes_atomic() {
        let html = "<html><head><title> \u{301}title\u{600} </title></head><body></body></html>";
        assert_eq!(title(html).as_deref(), Some(" \u{301}title\u{600} "));
        assert_eq!(
            collapse_whitespace(" \u{301}x\u{600} "),
            " \u{301}x\u{600} "
        );
    }

    #[test]
    fn title_returns_none_when_missing() {
        assert!(title("<html><body>x</body></html>").is_none());
    }

    #[test]
    fn links_resolve_against_base() {
        let html =
            r#"<a href="/foo">A</a><a href="https://example.com/b">B</a><a href="/foo">A</a>"#;
        let l = links(html, Some("https://docs.rs/x/y"));
        assert_eq!(l.len(), 2);
        assert_eq!(l[0], "https://docs.rs/foo");
        assert_eq!(l[1], "https://example.com/b");
    }

    #[test]
    fn to_text_strips_tags_and_collapses_space() {
        let html =
            "<html><body><p>Hello   <b>world</b></p><script>x=1</script><p>Bye</p></body></html>";
        let t = to_text(html);
        assert!(t.contains("Hello world"));
        assert!(t.contains("Bye"));
        assert!(!t.contains("x=1"));
    }

    #[test]
    fn to_text_skips_styles() {
        let html = "<html><body><style>a{}</style><p>Hi</p></body></html>";
        assert_eq!(to_text(html).trim(), "Hi");
    }

    // ── title ──────────────────────────────────────────────────────

    #[test]
    fn title_preserves_literal_content_per_html_rcdata_rules() {
        // HTML5 parses <title> as RCDATA: nested tags are kept verbatim.
        let html = "<title>a <em>b</em> c</title>";
        assert_eq!(title(html), Some("a <em>b</em> c".into()));
    }

    #[test]
    fn title_returns_none_when_blank() {
        let html = "<title>   </title>";
        assert!(title(html).is_none());
    }

    // ── links ──────────────────────────────────────────────────────

    #[test]
    fn links_without_base_returns_raw_hrefs() {
        let html = r#"<a href="/foo">A</a><a href="/bar">B</a>"#;
        let l = links(html, None);
        assert_eq!(l, vec!["/foo", "/bar"]);
    }

    #[test]
    fn links_dedupes_repeated_targets() {
        let html = r#"<a href="x">1</a><a href="x">2</a><a href="x">3</a>"#;
        let l = links(html, None);
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn links_skips_missing_href() {
        let html = r#"<a>no href</a><a href="ok">yes</a>"#;
        let l = links(html, None);
        assert_eq!(l, vec!["ok"]);
    }

    #[test]
    fn links_invalid_base_falls_back_to_no_resolution() {
        let html = r#"<a href="/foo">a</a>"#;
        let l = links(html, Some("not-a-url"));
        // url::Url::parse fails on "not-a-url"; base becomes None, raw href returned.
        assert_eq!(l, vec!["/foo"]);
    }

    // ── DDG result parsing ─────────────────────────────────────────

    #[test]
    fn parse_ddg_results_extracts_basic_fields() {
        let html = r#"
            <div class="result">
                <a class="result__a" href="https://example.com">Example</a>
                <a class="result__snippet">A short description.</a>
            </div>
        "#;
        let r = parse_ddg_results(html);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].title, "Example");
        assert_eq!(r[0].link, "https://example.com");
        assert_eq!(r[0].description, "A short description.");
    }

    #[test]
    fn parse_ddg_results_extracts_url_from_uddg_param() {
        let html = r#"
            <div class="result">
                <a class="result__a" href="/l/?uddg=https%3A%2F%2Freal.example%2Fpage&rut=abc">Real</a>
            </div>
        "#;
        let r = parse_ddg_results(html);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].link, "https://real.example/page");
    }

    #[test]
    fn parse_ddg_results_skips_empty_title() {
        let html = r#"
            <div class="result">
                <a class="result__a" href="https://example.com"></a>
            </div>
        "#;
        let r = parse_ddg_results(html);
        assert!(r.is_empty());
    }

    #[test]
    fn parse_ddg_results_skips_when_link_extraction_fails() {
        let html = r#"
            <div class="result">
                <a class="result__a" href="ftp://nope">Title</a>
            </div>
        "#;
        // extract_ddg_url returns "" for non-http/uddg, so the entry is dropped.
        let r = parse_ddg_results(html);
        assert!(r.is_empty());
    }

    #[test]
    fn parse_ddg_results_caps_at_twenty() {
        let mut html = String::new();
        for i in 0..25 {
            html.push_str(&format!(
                r#"<div class="result"><a class="result__a" href="https://e{i}.com">t{i}</a></div>"#
            ));
        }
        let r = parse_ddg_results(&html);
        assert_eq!(r.len(), 20);
    }

    #[test]
    fn parse_ddg_results_web_result_class_alternate_works() {
        let html = r#"
            <div class="web-result">
                <a class="result__a" href="https://x.com">X</a>
            </div>
        "#;
        let r = parse_ddg_results(html);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].link, "https://x.com");
    }

    // ── to_text ────────────────────────────────────────────────────

    #[test]
    fn to_text_inserts_newlines_between_blocks() {
        let html = "<p>one</p><p>two</p>";
        let t = to_text(html);
        assert!(t.contains("one"));
        assert!(t.contains("two"));
        // Block elements introduce a newline between them.
        let one = t.find("one").unwrap();
        let two = t.find("two").unwrap();
        assert!(t[one..two].contains('\n'));
    }

    #[test]
    fn to_text_skips_iframes_and_svg() {
        let html = "<p>before</p><iframe>inside-frame</iframe><svg>vector</svg><p>after</p>";
        let t = to_text(html);
        assert!(!t.contains("inside-frame"));
        assert!(!t.contains("vector"));
        assert!(t.contains("before"));
        assert!(t.contains("after"));
    }

    // ── to_markdown - headings + paragraphs ───────────────────────

    #[test]
    fn to_markdown_renders_headings() {
        let html = "<body><h1>Big</h1><h2>Small</h2></body>";
        let md = to_markdown(html, None);
        assert!(md.content.contains("# Big"));
        assert!(md.content.contains("## Small"));
    }

    #[test]
    fn to_markdown_renders_paragraphs() {
        let html = "<body><p>hello</p><p>world</p></body>";
        let md = to_markdown(html, None);
        assert!(md.content.contains("hello"));
        assert!(md.content.contains("world"));
    }

    #[test]
    fn to_markdown_extracts_title_from_head() {
        let html = "<html><head><title>Doc</title></head><body><p>x</p></body></html>";
        let md = to_markdown(html, None);
        assert_eq!(md.title.as_deref(), Some("Doc"));
    }

    // ── to_markdown - inline formatting ────────────────────────────

    #[test]
    fn to_markdown_emits_bold_for_strong_and_b() {
        let html = "<body><p>a <strong>x</strong> b <b>y</b></p></body>";
        let md = to_markdown(html, None);
        assert!(md.content.contains("**x**"));
        assert!(md.content.contains("**y**"));
    }

    #[test]
    fn to_markdown_emits_italic_for_em_and_i() {
        let html = "<body><p><em>e</em> <i>i</i></p></body>";
        let md = to_markdown(html, None);
        assert!(md.content.contains("*e*"));
        assert!(md.content.contains("*i*"));
    }

    #[test]
    fn to_markdown_emits_inline_code() {
        let html = "<body><p>see <code>fn x()</code></p></body>";
        let md = to_markdown(html, None);
        assert!(md.content.contains("`fn x()`"));
    }

    // ── to_markdown - links + images ───────────────────────────────

    #[test]
    fn to_markdown_emits_markdown_link_syntax() {
        let html = r#"<body><p><a href="https://x.com">site</a></p></body>"#;
        let md = to_markdown(html, None);
        assert!(md.content.contains("[site](https://x.com)"));
    }

    #[test]
    fn to_markdown_unwraps_link_to_text_when_href_is_fragment_only() {
        let html = r##"<body><p><a href="#anchor">text</a></p></body>"##;
        let md = to_markdown(html, None);
        assert!(md.content.contains("text"));
        assert!(!md.content.contains("](#anchor"));
    }

    #[test]
    fn to_markdown_uses_href_alone_when_link_text_is_empty() {
        let html = r#"<body><a href="https://x.com"></a></body>"#;
        let md = to_markdown(html, None);
        assert!(md.content.contains("https://x.com"));
        assert!(!md.content.contains("[]"));
    }

    #[test]
    fn to_markdown_emits_image_syntax() {
        let html = r#"<body><img src="pic.png" alt="cap"></body>"#;
        let md = to_markdown(html, None);
        assert!(md.content.contains("![cap](pic.png)"));
    }

    #[test]
    fn to_markdown_skips_image_with_empty_src() {
        let html = r#"<body><img alt="x"></body>"#;
        let md = to_markdown(html, None);
        assert!(!md.content.contains("!["));
    }

    // ── to_markdown - pre / hr / br ────────────────────────────────

    #[test]
    fn to_markdown_wraps_pre_in_triple_backticks() {
        let html = "<body><pre>line1\nline2</pre></body>";
        let md = to_markdown(html, None);
        assert!(md.content.contains("```"));
        assert!(md.content.contains("line1"));
        assert!(md.content.contains("line2"));
    }

    #[test]
    fn to_markdown_renders_hr() {
        let html = "<body><p>a</p><hr><p>b</p></body>";
        let md = to_markdown(html, None);
        assert!(md.content.contains("---"));
    }

    #[test]
    fn to_markdown_br_becomes_newline() {
        let html = "<body><p>a<br>b</p></body>";
        let md = to_markdown(html, None);
        let a = md.content.find('a').unwrap();
        let b = md.content.find('b').unwrap();
        assert!(md.content[a..b].contains('\n'));
    }

    // ── to_markdown - lists ────────────────────────────────────────

    #[test]
    fn to_markdown_renders_unordered_list_with_dashes() {
        let html = "<body><ul><li>one</li><li>two</li></ul></body>";
        let md = to_markdown(html, None);
        assert!(md.content.contains("- one"));
        assert!(md.content.contains("- two"));
    }

    #[test]
    fn to_markdown_renders_ordered_list_with_numbers() {
        let html = "<body><ol><li>first</li><li>second</li></ol></body>";
        let md = to_markdown(html, None);
        assert!(md.content.contains("1. first"));
        assert!(md.content.contains("2. second"));
    }

    // ── to_markdown - blockquote ───────────────────────────────────

    #[test]
    fn to_markdown_prefixes_blockquote_lines_with_gt() {
        let html = "<body><blockquote>quoted text</blockquote></body>";
        let md = to_markdown(html, None);
        assert!(md.content.contains("> quoted text"));
    }

    // ── to_markdown - tables ───────────────────────────────────────

    #[test]
    fn to_markdown_renders_table_with_header_separator() {
        let html = "<body><table>\
            <tr><th>name</th><th>val</th></tr>\
            <tr><td>a</td><td>1</td></tr>\
            </table></body>";
        let md = to_markdown(html, None);
        assert!(md.content.contains("| name | val |"));
        assert!(md.content.contains("| --- | --- |"));
        assert!(md.content.contains("| a | 1 |"));
    }

    #[test]
    fn to_markdown_handles_table_without_header() {
        let html = "<body><table><tr><td>x</td><td>y</td></tr></table></body>";
        let md = to_markdown(html, None);
        assert!(md.content.contains("| x | y |"));
        assert!(!md.content.contains("---"));
    }

    // ── to_markdown - links collection ─────────────────────────────

    #[test]
    fn to_markdown_collects_outbound_links_when_base_url_given() {
        let html = r#"<body>
            <a href="/foo">a</a>
            <a href="https://other.com/x">b</a>
        </body>"#;
        let md = to_markdown(html, Some("https://example.com"));
        assert_eq!(md.links.len(), 2);
        assert!(md.links.contains(&"https://example.com/foo".to_string()));
        assert!(md.links.contains(&"https://other.com/x".to_string()));
    }

    #[test]
    fn to_markdown_drops_javascript_mailto_tel_fragment_links() {
        let html = r##"<body>
            <a href="javascript:void(0)">js</a>
            <a href="mailto:x@y.com">m</a>
            <a href="tel:+15551234">t</a>
            <a href="#anchor">a</a>
            <a href="https://keep.com">k</a>
        </body>"##;
        let md = to_markdown(html, Some("https://example.com"));
        assert_eq!(md.links, vec!["https://keep.com/"]);
    }

    #[test]
    fn to_markdown_strips_fragments_from_collected_links() {
        let html = r##"<body><a href="/page#section">a</a></body>"##;
        let md = to_markdown(html, Some("https://example.com"));
        assert_eq!(md.links, vec!["https://example.com/page"]);
    }

    #[test]
    fn to_markdown_dedupes_collected_links() {
        let html = r#"<body>
            <a href="/a">first</a>
            <a href="/a">second</a>
        </body>"#;
        let md = to_markdown(html, Some("https://example.com"));
        assert_eq!(md.links.len(), 1);
    }

    #[test]
    fn to_markdown_caps_collected_links_at_fifty() {
        let mut html = String::from("<body>");
        for i in 0..70 {
            html.push_str(&format!(r#"<a href="/p{i}">x</a>"#));
        }
        html.push_str("</body>");
        let md = to_markdown(&html, Some("https://example.com"));
        assert_eq!(md.links.len(), 50);
    }

    #[test]
    fn to_markdown_skips_link_collection_when_no_base_url() {
        let html = r#"<body><a href="/a">x</a></body>"#;
        let md = to_markdown(html, None);
        assert!(md.links.is_empty());
    }

    // ── extract_ddg_url ────────────────────────────────────────────

    #[test]
    fn extract_ddg_url_returns_direct_http_urls() {
        assert_eq!(
            extract_ddg_url("https://example.com/page"),
            "https://example.com/page"
        );
        assert_eq!(extract_ddg_url("http://x.org"), "http://x.org");
    }

    #[test]
    fn extract_ddg_url_returns_empty_for_unknown_scheme() {
        assert_eq!(extract_ddg_url("ftp://x"), "");
        assert_eq!(extract_ddg_url("/relative"), "");
    }

    #[test]
    fn extract_ddg_url_decodes_uddg_param() {
        let url = "/l/?uddg=https%3A%2F%2Fdest.example%2Fpath&rut=x";
        assert_eq!(extract_ddg_url(url), "https://dest.example/path");
    }
}
