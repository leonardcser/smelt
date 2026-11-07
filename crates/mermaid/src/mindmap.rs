//! `mindmap`, drawn as an indented tree.

use crate::text::{self, label_lines, width};
use crate::{syntax, Document, Error};

const MAX_NODES: usize = 500;

struct Node {
    lines: Vec<String>,
    children: Vec<usize>,
}

pub(crate) fn render(doc: &Document, max_width: usize) -> Result<String, Error> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for raw in &doc.raw {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("::icon(") {
            continue;
        }
        let indent = raw.len() - raw.trim_start().len();
        while stack.last().is_some_and(|&(i, _)| i >= indent) {
            stack.pop();
        }
        if stack.is_empty() && !nodes.is_empty() {
            return Err(Error::Syntax("mindmap has more than one root".into()));
        }
        if nodes.len() >= MAX_NODES {
            return Err(Error::TooLarge);
        }
        let lines = label_lines(node_text(line));
        if lines.is_empty() {
            return Err(syntax(line));
        }
        let id = nodes.len();
        nodes.push(Node {
            lines,
            children: Vec::new(),
        });
        if let Some(&(_, parent)) = stack.last() {
            nodes[parent].children.push(id);
        }
        stack.push((indent, id));
    }
    if nodes.is_empty() {
        return Err(Error::Syntax("mindmap is empty".into()));
    }
    let mut out = Vec::new();
    let root_w = max_width.max(1);
    out.extend(text::wrap_lines(&nodes[0].lines, root_w));
    let mut ok = true;
    for (k, &child) in nodes[0].children.iter().enumerate() {
        let last = k + 1 == nodes[0].children.len();
        draw(&nodes, child, "", last, max_width, &mut out, &mut ok);
    }
    if !ok {
        return Err(Error::TooWide);
    }
    let body = out.join("\n");
    Ok(match &doc.title {
        Some(t) => format!("{}\n\n{body}", text::wrap(t, max_width).join("\n")),
        None => body,
    })
}

fn draw(
    nodes: &[Node],
    n: usize,
    prefix: &str,
    last: bool,
    max_width: usize,
    out: &mut Vec<String>,
    ok: &mut bool,
) {
    let (branch, cont) = if last {
        ("└── ", "    ")
    } else {
        ("├── ", "│   ")
    };
    let avail = max_width.saturating_sub(width(prefix) + 4);
    if avail < 6 {
        *ok = false;
        return;
    }
    let child_prefix = format!("{prefix}{cont}");
    for (k, line) in text::wrap_lines(&nodes[n].lines, avail).iter().enumerate() {
        let lead = if k == 0 { branch } else { cont };
        out.push(format!("{prefix}{lead}{line}"));
    }
    for (k, &child) in nodes[n].children.iter().enumerate() {
        let last = k + 1 == nodes[n].children.len();
        draw(nodes, child, &child_prefix, last, max_width, out, ok);
    }
}

/// Text inside a node's shape delimiters, without its id or class.
fn node_text(line: &str) -> &str {
    let line = line.split(":::").next().unwrap_or(line).trim();
    let Some(open) = line.find(['[', '(', '{', ')']) else {
        return line;
    };
    let inner = line[open..].trim_start_matches(['[', '(', '{', ')']);
    let inner = inner.trim_end_matches([']', ')', '}', '(']);
    if inner.is_empty() {
        line
    } else {
        inner
    }
}
