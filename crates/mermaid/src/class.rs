//! `classDiagram`.

use crate::canvas::Stroke;
use crate::graph::{Align, Dir, Edge, Graph, Marker, Section};
use crate::text::{label_lines, label_text, sanitize};
use crate::{keyword, syntax, Document, Error};

const IGNORED: &[&str] = &[
    "note", "cssClass", "click", "link", "callback", "style", "classDef", "accTitle", "accDescr",
];

#[derive(Default)]
struct Members {
    annotation: Option<String>,
    attributes: Vec<String>,
    methods: Vec<String>,
}

pub(crate) fn parse(doc: &Document) -> Result<Graph, Error> {
    let mut g = Graph::new(Dir::TD);
    g.title = doc.title.clone();
    let mut members: Vec<Members> = Vec::new();
    let mut body: Option<usize> = None;
    let mut namespaces = 0usize;
    for line in &doc.lines {
        let line = line.trim_end_matches(';').trim();
        if line.is_empty() {
            continue;
        }
        if let Some(i) = body {
            if line == "}" {
                body = None;
            } else {
                add_member(&mut members[i], line);
            }
            continue;
        }
        if line == "}" {
            namespaces = namespaces.checked_sub(1).ok_or_else(|| syntax(line))?;
            continue;
        }
        if let Some(d) = keyword(line, "direction") {
            g.dir = Dir::parse(d).ok_or_else(|| syntax(line))?;
            continue;
        }
        if IGNORED.iter().any(|k| keyword(line, k).is_some()) || line.starts_with("accDescr") {
            continue;
        }
        if keyword(line, "namespace").is_some() {
            namespaces += 1;
            continue;
        }
        if let Some(rest) = keyword(line, "class") {
            let (decl, open) = match rest.strip_suffix('{') {
                Some(d) => (d.trim(), true),
                None => (rest, false),
            };
            let i = class(&mut g, &mut members, decl)?;
            if open {
                body = Some(i);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("<<") {
            let (annotation, name) = rest.split_once(">>").ok_or_else(|| syntax(line))?;
            let i = class(&mut g, &mut members, name)?;
            members[i].annotation = Some(format!("«{}»", annotation.trim()));
            continue;
        }
        if let Some(rel) = relation(line) {
            let from = class(&mut g, &mut members, rel.left)?;
            let to = class(&mut g, &mut members, rel.right)?;
            let mut edge = Edge::new(from, to);
            edge.stroke = rel.stroke;
            edge.tail = rel.tail;
            edge.head = rel.head;
            edge.label = relation_label(rel.label, rel.cards);
            g.add_edge(edge)?;
            continue;
        }
        if let Some((name, member)) = line.split_once(':') {
            let i = class(&mut g, &mut members, name)?;
            add_member(&mut members[i], member.trim());
            continue;
        }
        class(&mut g, &mut members, line)?;
    }
    if body.is_some() || namespaces > 0 {
        return Err(Error::Syntax("unclosed block".into()));
    }
    for (i, m) in members.into_iter().enumerate() {
        let node = &mut g.nodes[i];
        let mut title = node.sections[0].lines.clone();
        if let Some(a) = m.annotation {
            title.insert(0, a);
        }
        node.set_label(title);
        for list in [m.attributes, m.methods] {
            if !list.is_empty() {
                node.sections.push(Section {
                    lines: list,
                    align: Align::Left,
                });
            }
        }
    }
    Ok(g)
}

/// Turns `List~int~` into `List<int>`.
fn generics(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut open = false;
    for c in s.chars() {
        if c == '~' {
            out.push(if open { '>' } else { '<' });
            open = !open;
        } else {
            out.push(c);
        }
    }
    out
}

fn class(g: &mut Graph, members: &mut Vec<Members>, decl: &str) -> Result<usize, Error> {
    let decl = decl.trim();
    let decl = decl.split(":::").next().unwrap_or(decl).trim();
    let (name, label) = match decl.split_once('[') {
        Some((name, rest)) => (name.trim(), Some(rest.trim_end_matches(']'))),
        None => (decl, None),
    };
    let id = name.split('~').next().unwrap_or(name).trim();
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(syntax(decl));
    }
    let i = g.node(id)?;
    if members.len() <= i {
        members.resize_with(i + 1, Members::default);
    }
    if let Some(label) = label {
        g.nodes[i].set_label(label_lines(label));
    } else if name.contains('~') {
        g.nodes[i].set_label(vec![generics(name)]);
    }
    Ok(i)
}

fn add_member(m: &mut Members, raw: &str) {
    let raw = raw.trim();
    if let Some(a) = raw.strip_prefix("<<").and_then(|r| r.strip_suffix(">>")) {
        m.annotation = Some(format!("«{}»", a.trim()));
        return;
    }
    let text = sanitize(&generics(raw.trim_end_matches(['$', '*'])));
    if text.is_empty() {
        return;
    }
    if text.contains('(') {
        m.methods.push(text);
    } else {
        m.attributes.push(text);
    }
}

struct Relation<'a> {
    left: &'a str,
    right: &'a str,
    stroke: Stroke,
    tail: Option<Marker>,
    head: Option<Marker>,
    cards: (Option<String>, Option<String>),
    label: Option<&'a str>,
}

fn marker(token: &str) -> Option<Marker> {
    match token {
        "<|" | "|>" => Some(Marker::Triangle),
        "*" => Some(Marker::Diamond),
        "o" => Some(Marker::DiamondHollow),
        "<" | ">" => Some(Marker::Arrow),
        _ => None,
    }
}

/// Parses `A "1" *-- "many" B : label`.
fn relation(line: &str) -> Option<Relation<'_>> {
    let (rel_part, label) = match line.split_once(':') {
        Some((r, l)) => (r, Some(l.trim())),
        None => (line, None),
    };
    let solid = rel_part.find("--");
    let dotted = rel_part.find("..");
    let (at, stroke) = match (solid, dotted) {
        (Some(s), Some(d)) if d < s => (d, Stroke::Dotted),
        (Some(s), _) => (s, Stroke::Solid),
        (None, Some(d)) => (d, Stroke::Dotted),
        (None, None) => return None,
    };
    let before = &rel_part[..at];
    let after = &rel_part[at + 2..];
    let boundary = |c: Option<char>| c.is_none_or(|c| c.is_whitespace() || c == '"');
    let (before, tail) = ["<|", "*", "o", "<"]
        .iter()
        .find_map(|t| {
            let rest = before.strip_suffix(t)?;
            (*t != "o" || boundary(rest.chars().last())).then(|| (rest, marker(t)))
        })
        .unwrap_or((before, None));
    let (after, head) = ["|>", "*", "o", ">"]
        .iter()
        .find_map(|t| {
            let rest = after.strip_prefix(t)?;
            (*t != "o" || boundary(rest.chars().next())).then(|| (rest, marker(t)))
        })
        .unwrap_or((after, None));
    let (left, left_card) = split_card(before, true);
    let (right, right_card) = split_card(after, false);
    if left.is_empty() || right.is_empty() {
        return None;
    }
    Some(Relation {
        left,
        right,
        stroke,
        tail,
        head,
        cards: (left_card, right_card),
        label: label.filter(|l| !l.is_empty()),
    })
}

/// Separates a quoted cardinality from the class name next to it.
fn split_card(s: &str, card_after: bool) -> (&str, Option<String>) {
    let s = s.trim();
    let quoted = if card_after {
        s.strip_suffix('"')
            .and_then(|r| r.rfind('"').map(|q| (&r[..q], &r[q + 1..])))
    } else {
        s.strip_prefix('"')
            .and_then(|r| r.find('"').map(|q| (&r[q + 1..], &r[..q])))
    };
    match quoted {
        Some((name, card)) => (name.trim(), Some(label_text(card))),
        None => (s, None),
    }
}

pub(crate) fn relation_label(
    label: Option<&str>,
    cards: (Option<String>, Option<String>),
) -> Vec<String> {
    let mut text = label.map(label_text).unwrap_or_default();
    let cards = match cards {
        (None, None) => None,
        (a, b) => Some(format!(
            "{} to {}",
            a.as_deref().unwrap_or("?"),
            b.as_deref().unwrap_or("?")
        )),
    };
    if let Some(cards) = cards {
        if text.is_empty() {
            text = format!("({cards})");
        } else {
            text = format!("{text} ({cards})");
        }
    }
    if text.is_empty() {
        Vec::new()
    } else {
        vec![text]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_relations() {
        let r = relation("Order \"1\" *-- \"1..*\" LineItem : contains").unwrap();
        assert_eq!((r.left, r.right), ("Order", "LineItem"));
        assert_eq!((r.tail, r.head), (Some(Marker::Diamond), None));
        assert_eq!(r.cards, (Some("1".into()), Some("1..*".into())));
        assert_eq!(r.label, Some("contains"));
        let r = relation("Animal <|-- Duck").unwrap();
        assert_eq!(
            (r.tail, r.head, r.stroke),
            (Some(Marker::Triangle), None, Stroke::Solid)
        );
        let r = relation("Shape ..|> Drawable").unwrap();
        assert_eq!(
            (r.tail, r.head, r.stroke),
            (None, Some(Marker::Triangle), Stroke::Dotted)
        );
        let r = relation("Pool o-- Lane").unwrap();
        assert_eq!(r.tail, Some(Marker::DiamondHollow));
        assert!(relation("Foo : +bar()").is_none());
    }
}
