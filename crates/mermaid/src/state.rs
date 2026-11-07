//! `stateDiagram` and `stateDiagram-v2`.
//!
//! Composite states are flattened: the composite becomes a state whose
//! dotted edge points at the first inner state, and each composite gets its
//! own end marker.

use crate::canvas::Stroke;
use crate::graph::{Align, Dir, Edge, Graph, Section, Shape};
use crate::text::label_lines;
use crate::{keyword, syntax, Document, Error};

const IGNORED: &[&str] = &[
    "classDef", "class", "style", "hide", "scale", "accTitle", "accDescr",
];

pub(crate) fn parse(doc: &Document) -> Result<Graph, Error> {
    let mut g = Graph::new(Dir::TD);
    g.title = doc.title.clone();
    let mut scopes: Vec<String> = Vec::new();
    let mut in_note = false;
    for line in &doc.lines {
        let line = line.trim_end_matches(';').trim();
        if in_note {
            in_note = line != "end note";
            continue;
        }
        if line.is_empty() || line == "--" {
            continue;
        }
        if let Some(rest) = keyword(line, "note") {
            in_note = !rest.contains(':');
            continue;
        }
        if let Some(d) = keyword(line, "direction") {
            g.dir = Dir::parse(d).ok_or_else(|| syntax(line))?;
            continue;
        }
        if IGNORED.iter().any(|k| keyword(line, k).is_some()) || line.starts_with("accDescr") {
            continue;
        }
        if line == "}" {
            scopes.pop().ok_or_else(|| syntax(line))?;
            continue;
        }
        if let Some(rest) = keyword(line, "state") {
            declare(&mut g, &mut scopes, rest)?;
            continue;
        }
        if let Some((lhs, rhs)) = line.split_once("-->") {
            let (target, label) = match rhs.split_once(':') {
                Some((t, l)) => (t, label_lines(l)),
                None => (rhs, Vec::new()),
            };
            let scope = scopes.last().cloned();
            let (from, inner_start) = endpoint(&mut g, lhs, scope.as_deref(), true)?;
            let (to, _) = endpoint(&mut g, target, scope.as_deref(), false)?;
            let mut edge = Edge::new(from, to);
            edge.label = label;
            if inner_start {
                edge.stroke = Stroke::Dotted;
            }
            g.add_edge(edge)?;
            continue;
        }
        if let Some((id, desc)) = line.split_once(':') {
            let i = state(&mut g, id)?;
            describe(&mut g, i, desc);
            continue;
        }
        if valid_id(line) {
            state(&mut g, line)?;
            continue;
        }
        return Err(syntax(line));
    }
    if !scopes.is_empty() {
        return Err(Error::Syntax("unclosed composite state".into()));
    }
    Ok(g)
}

fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
}

fn clean_id(s: &str) -> &str {
    let s = s.trim();
    s.split(":::").next().unwrap_or(s).trim()
}

fn state(g: &mut Graph, id: &str) -> Result<usize, Error> {
    let id = clean_id(id);
    if !valid_id(id) {
        return Err(syntax(id));
    }
    let i = g.node(id)?;
    if g.nodes[i].shape == Shape::Rect {
        g.nodes[i].shape = Shape::Round;
    }
    Ok(i)
}

fn describe(g: &mut Graph, i: usize, desc: &str) {
    let lines = label_lines(desc);
    let node = &mut g.nodes[i];
    match node.sections.get_mut(1) {
        Some(section) => section.lines.extend(lines),
        None => node.sections.push(Section {
            lines,
            align: Align::Left,
        }),
    }
}

/// Resolves a transition endpoint. `[*]` is the scope's start when it is
/// the source and the scope's end when it is the target. Returns whether
/// the endpoint is a composite's own start.
fn endpoint(
    g: &mut Graph,
    raw: &str,
    scope: Option<&str>,
    source: bool,
) -> Result<(usize, bool), Error> {
    let raw = clean_id(raw);
    if raw != "[*]" {
        return Ok((state(g, raw)?, false));
    }
    match (scope, source) {
        (Some(composite), true) => Ok((g.find(composite).ok_or_else(|| syntax(raw))?, true)),
        (scope, _) => {
            let (key, glyph) = if source {
                ("start", '●')
            } else {
                ("end", '◉')
            };
            let id = format!("\u{0}{key}:{}", scope.unwrap_or(""));
            let i = g.node(&id)?;
            g.nodes[i].shape = Shape::Dot(glyph);
            Ok((i, false))
        }
    }
}

/// `state` statements: aliases, composites and pseudo-states.
fn declare(g: &mut Graph, scopes: &mut Vec<String>, rest: &str) -> Result<(), Error> {
    let rest = rest.trim();
    let (rest, composite) = match rest.strip_suffix('{') {
        Some(r) => (r.trim(), true),
        None => (rest, false),
    };
    let id = if let Some(quoted) = rest.strip_prefix('"') {
        let (desc, after) = quoted.split_once('"').ok_or_else(|| syntax(rest))?;
        let id = keyword(after.trim(), "as").ok_or_else(|| syntax(rest))?;
        let i = state(g, id)?;
        g.nodes[i].set_label(label_lines(desc));
        id.to_string()
    } else if let Some((id, kind)) = rest.split_once("<<") {
        let i = state(g, id)?;
        let kind = kind.trim_end_matches(">>").trim();
        g.nodes[i].shape = match kind {
            "choice" => Shape::Dot('◇'),
            "fork" | "join" => Shape::Dot('▬'),
            _ => return Err(syntax(rest)),
        };
        id.trim().to_string()
    } else if let Some((id, desc)) = rest.split_once(':') {
        let i = state(g, id)?;
        describe(g, i, desc);
        id.trim().to_string()
    } else if let Some((id, alias)) = rest.split_once(" as ") {
        let i = state(g, alias)?;
        g.nodes[i].set_label(label_lines(id));
        alias.trim().to_string()
    } else {
        state(g, rest)?;
        rest.to_string()
    };
    if composite {
        scopes.push(clean_id(&id).to_string());
    }
    Ok(())
}
