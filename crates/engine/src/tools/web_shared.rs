use std::sync::atomic::{AtomicUsize, Ordering};
use url::Url;

static UA_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Return a glob pattern that matches all URLs on the same domain.
/// e.g. "https://docs.rs/foo/bar" -> "https://docs.rs/*"
pub(crate) fn domain_pattern(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let scheme = parsed.scheme();
    let host = parsed.host_str()?;
    Some(format!("{scheme}://{host}/*"))
}

const USER_AGENTS: &[&str] = &[
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:133.0) Gecko/20100101 Firefox/133.0",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:133.0) Gecko/20100101 Firefox/133.0",
    "Mozilla/5.0 (X11; Linux x86_64; rv:133.0) Gecko/20100101 Firefox/133.0",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.2 Safari/605.1.15",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 Edg/131.0.0.0",
    "Mozilla/5.0 (iPhone; CPU iPhone OS 18_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.2 Mobile/15E148 Safari/604.1",
    "Mozilla/5.0 (Linux; Android 14; SM-S911B) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:132.0) Gecko/20100101 Firefox/132.0",
    "Mozilla/5.0 (X11; Ubuntu; Linux x86_64; rv:132.0) Gecko/20100101 Firefox/132.0",
    "Mozilla/5.0 (iPad; CPU OS 18_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.2 Mobile/15E148 Safari/604.1",
    "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 OPR/116.0.0.0",
];

pub(crate) fn next_user_agent() -> &'static str {
    // 80% round-robin, 20% random
    let idx = UA_COUNTER.fetch_add(1, Ordering::Relaxed);
    if idx.is_multiple_of(5) {
        // ~20%: pick pseudo-random based on counter mixing
        let mixed = idx.wrapping_mul(6364136223846793005).wrapping_add(1);
        USER_AGENTS[mixed % USER_AGENTS.len()]
    } else {
        USER_AGENTS[idx % USER_AGENTS.len()]
    }
}

const SKIP_ELEMENTS: &[&str] = &[
    "script", "style", "noscript", "iframe", "object", "embed", "meta", "link", "svg",
];

/// Parse HTML once and extract all content (title, links, body) in a single pass.
pub(crate) struct ParsedHtml {
    pub(crate) title: Option<String>,
    pub(crate) links: Vec<String>,
    doc: scraper::Html,
}

impl ParsedHtml {
    pub(crate) fn parse(html: &str, base_url: Option<&url::Url>) -> Self {
        use scraper::{Html, Selector};
        use std::collections::HashSet;

        let doc = Html::parse_document(html);

        let title = {
            let sel = Selector::parse("title").unwrap();
            doc.select(&sel)
                .next()
                .map(|el| el.text().collect::<String>().trim().to_string())
        };

        let links = if let Some(base) = base_url {
            let sel = Selector::parse("a[href]").unwrap();
            let mut seen = HashSet::new();
            let mut links = Vec::new();
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
            links
        } else {
            vec![]
        };

        Self { title, links, doc }
    }

    /// Convert to markdown, stripping non-content elements in a single pass.
    pub(crate) fn to_markdown(&self) -> String {
        use scraper::Selector;

        let body_sel = Selector::parse("body").unwrap();
        let root = self.doc.select(&body_sel).next();
        match root {
            Some(el) => {
                let mut out = String::new();
                html_to_md(el, &mut out);
                collapse_blank_lines(&out)
            }
            None => self.to_text(),
        }
    }

    /// Extract text content, stripping all tags.
    pub(crate) fn to_text(&self) -> String {
        use scraper::Selector;

        let skip = Selector::parse("script, style, noscript, iframe, object, embed, svg").unwrap();
        let body_sel = Selector::parse("body").unwrap();

        let mut text = String::new();
        fn collect(node: scraper::ElementRef, skip: &Selector, out: &mut String) {
            if skip.matches(&node) {
                return;
            }
            for child in node.children() {
                if let Some(t) = child.value().as_text() {
                    let trimmed = t.trim();
                    if !trimmed.is_empty() {
                        if !out.is_empty() {
                            out.push(' ');
                        }
                        out.push_str(trimmed);
                    }
                } else if let Some(el) = scraper::ElementRef::wrap(child) {
                    collect(el, skip, out);
                }
            }
        }

        if let Some(body) = self.doc.select(&body_sel).next() {
            collect(body, &skip, &mut text);
        }
        text
    }
}

/// Recursively convert an HTML element to markdown.
fn html_to_md(el: scraper::ElementRef, out: &mut String) {
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
            // Inside <pre>, collect raw text preserving whitespace.
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
                if let Some(li) = scraper::ElementRef::wrap(child) {
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

fn walk_children(el: scraper::ElementRef, out: &mut String) {
    for child in el.children() {
        if let Some(t) = child.value().as_text() {
            out.push_str(t);
        } else if let Some(child_el) = scraper::ElementRef::wrap(child) {
            html_to_md(child_el, out);
        }
    }
}

fn collect_inline_text(el: scraper::ElementRef, out: &mut String) {
    for child in el.children() {
        if let Some(t) = child.value().as_text() {
            out.push_str(t);
        } else if let Some(child_el) = scraper::ElementRef::wrap(child) {
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
                "a" => {
                    html_to_md(child_el, out);
                }
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

fn render_table(table: scraper::ElementRef, out: &mut String) {
    use scraper::Selector;

    let row_sel = Selector::parse("tr").unwrap();
    let th_sel = Selector::parse("th").unwrap();
    let td_sel = Selector::parse("td").unwrap();

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
        // Insert separator after header row
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

/// Truncate output to max lines/bytes, appending a note if truncated.
pub(crate) fn truncate_output(text: &str, max_lines: usize, max_bytes: usize) -> String {
    let mut lines: Vec<&str> = text.lines().collect();
    let mut truncated = false;

    if lines.len() > max_lines {
        lines.truncate(max_lines);
        truncated = true;
    }

    let mut result = lines.join("\n");
    if result.len() > max_bytes {
        result.truncate(result.floor_char_boundary(max_bytes));
        truncated = true;
    }

    if truncated {
        result.push_str("\n\n[output truncated]");
    }
    result
}
