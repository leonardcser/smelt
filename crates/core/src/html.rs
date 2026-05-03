//! HTML capability — small read-only surface over `scraper`. Pure
//! parsing, no I/O. Exposed to Lua via `crates/tui/src/lua/api/html.rs`
//! and composed by tools that need to digest a fetched page.
//!
//! Read shapes ship for title, links, plain text, and a markdown
//! projection that consumers like `web_fetch` use to feed an LLM
//! extractor.

use scraper::{ElementRef, Html, Selector};
use std::collections::HashSet;
use url::Url;

const SKIP_ELEMENTS: &[&str] = &[
    "script", "style", "noscript", "iframe", "object", "embed", "meta", "link", "svg",
];

/// Extract the document title, if present.
pub(crate) fn title(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("title").ok()?;
    doc.select(&sel)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Extract `<a href>` targets, resolved against `base_url` when given.
/// Output is unique-preserving-insertion-order.
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

/// One DuckDuckGo HTML search result row: title text, resolved
/// destination URL, and an optional snippet.
#[derive(Debug, Clone)]
pub(crate) struct DdgResult {
    pub(crate) title: String,
    pub(crate) link: String,
    pub(crate) description: String,
}

/// Parse the DuckDuckGo HTML results page (`html.duckduckgo.com/html/`)
/// into a list of [`DdgResult`]s. Returns at most 20 entries — enough
/// for the model, soft enough on token budget. Empty title or empty
/// resolved link skips the row.
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
        let title: String = title_el.text().collect::<String>().trim().to_string();
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
            .map(|s| s.text().collect::<String>().trim().to_string())
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

/// Plain-text projection: walks the DOM, skips script/style/etc, joins
/// visible text with spaces. Whitespace is collapsed to single spaces;
/// blocks introduce a newline. Good enough for "read what the page
/// says"; not a faithful renderer.
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

/// Combined markdown projection: page title, body content rendered as
/// markdown, and filtered outbound links resolved against `base_url`.
/// Used by `web_fetch` to digest a fetched page into a single payload
/// before extraction. Links are deduplicated, fragment-stripped, and
/// capped at 50; `javascript:` / `mailto:` / `tel:` / pure-fragment
/// targets are dropped.
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
            .map(|el| el.text().collect::<String>().trim().to_string())
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
            let href = href.trim();
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
            if link_text.trim().is_empty() {
                out.push_str(href);
            } else if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
                out.push_str(&link_text);
            } else {
                out.push('[');
                out.push_str(link_text.trim());
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
            for line in inner.trim().lines() {
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
            .map(|c| c.text().collect::<String>().trim().to_string())
            .collect();
        if !ths.is_empty() {
            has_header = true;
            rows.push(ths);
            continue;
        }
        let tds: Vec<String> = row
            .select(&td_sel)
            .map(|c| c.text().collect::<String>().trim().to_string())
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
    out.trim().to_string()
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = true;
    let mut last_was_newline = true;
    for ch in s.chars() {
        if ch == '\n' {
            if !last_was_newline {
                out.push('\n');
            }
            last_was_newline = true;
            last_was_space = true;
        } else if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(ch);
            last_was_space = false;
            last_was_newline = false;
        }
    }
    out.trim().to_string()
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
}
