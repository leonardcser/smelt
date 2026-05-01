//! HTML capability — small read-only surface over `scraper`. Pure
//! parsing, no I/O. Exposed to Lua via `crates/tui/src/lua/api/html.rs`
//! and composed by tools that need to digest a fetched page.
//!
//! Today this module ships the obvious read shapes: title, links, and
//! a tag-stripped plain-text projection. The full HTML→markdown
//! converter from `engine/tools/web_shared.rs` migrates here when its
//! caller (the `web_fetch` tool) moves to Lua in P5.b.

use scraper::{Html, Selector};
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
