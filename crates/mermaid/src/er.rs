//! `erDiagram`.

use crate::canvas::Stroke;
use crate::class::relation_label;
use crate::graph::{Align, Dir, Edge, Graph, Section};
use crate::text::{label_lines, label_text, width};
use crate::{keyword, syntax, Document, Error};

pub(crate) fn parse(doc: &Document) -> Result<Graph, Error> {
    let mut g = Graph::new(Dir::TD);
    g.title = doc.title.clone();
    let mut attributes: Vec<Vec<[String; 4]>> = Vec::new();
    let mut body: Option<usize> = None;
    for line in &doc.lines {
        let line = line.trim_end_matches(';').trim();
        if line.is_empty() {
            continue;
        }
        if let Some(i) = body {
            if line == "}" {
                body = None;
            } else {
                attributes[i].push(attribute(line).ok_or_else(|| syntax(line))?);
            }
            continue;
        }
        if let Some(d) = keyword(line, "direction") {
            g.dir = Dir::parse(d).ok_or_else(|| syntax(line))?;
            continue;
        }
        if keyword(line, "accTitle").is_some() || line.starts_with("accDescr") {
            continue;
        }
        if let Some(decl) = line.strip_suffix('{') {
            let i = entity(&mut g, &mut attributes, decl)?;
            body = Some(i);
            continue;
        }
        let (rel, label) = match line.split_once(':') {
            Some((r, l)) => (r, Some(l.trim())),
            None => (line, None),
        };
        let parts: Vec<&str> = rel.split_whitespace().collect();
        match parts.as_slice() {
            [a, op, b] => {
                let (left, stroke, right) = relationship(op).ok_or_else(|| syntax(line))?;
                let from = entity(&mut g, &mut attributes, a)?;
                let to = entity(&mut g, &mut attributes, b)?;
                let mut edge = Edge::new(from, to);
                edge.head = None;
                edge.stroke = stroke;
                edge.label = relation_label(label, (Some(left.into()), Some(right.into())));
                g.add_edge(edge)?;
            }
            [a] if label.is_none() => {
                entity(&mut g, &mut attributes, a)?;
            }
            _ => return Err(syntax(line)),
        }
    }
    if body.is_some() {
        return Err(Error::Syntax("unclosed entity".into()));
    }
    for (i, rows) in attributes.into_iter().enumerate() {
        if rows.is_empty() {
            continue;
        }
        let mut widths = [0usize; 4];
        for row in &rows {
            for (w, cell) in widths.iter_mut().zip(row) {
                *w = (*w).max(width(cell));
            }
        }
        let lines = rows
            .iter()
            .map(|row| {
                let mut line = String::new();
                for (k, cell) in row.iter().enumerate() {
                    if widths[k] == 0 {
                        continue;
                    }
                    if !line.is_empty() {
                        line.push(' ');
                    }
                    line.push_str(cell);
                    line.push_str(&" ".repeat(widths[k] - width(cell)));
                }
                line.trim_end().to_string()
            })
            .collect();
        g.nodes[i].sections.push(Section {
            lines,
            align: Align::Left,
        });
    }
    Ok(g)
}

fn entity(
    g: &mut Graph,
    attributes: &mut Vec<Vec<[String; 4]>>,
    decl: &str,
) -> Result<usize, Error> {
    let decl = decl.trim();
    let (id, alias) = match decl.split_once('[') {
        Some((id, rest)) => (id.trim(), Some(rest.trim_end_matches(']'))),
        None => (decl, None),
    };
    let id = id.trim_matches('"');
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return Err(syntax(decl));
    }
    let i = g.node(id)?;
    if attributes.len() <= i {
        attributes.resize(i + 1, Vec::new());
    }
    if let Some(alias) = alias {
        g.nodes[i].set_label(label_lines(alias));
    }
    Ok(i)
}

/// `type name [PK, FK] ["comment"]` as type, name, keys and comment.
fn attribute(line: &str) -> Option<[String; 4]> {
    let (head, comment) = match line.split_once('"') {
        Some((h, c)) => (h, format!("\"{}\"", label_text(c.trim_end_matches('"')))),
        None => (line, String::new()),
    };
    let mut words = head.split_whitespace();
    let ty = words.next()?.to_string();
    let name = words.next()?.to_string();
    let keys: Vec<&str> = words
        .flat_map(|w| w.split(','))
        .filter(|k| !k.is_empty())
        .collect();
    Some([ty, name, keys.join(","), comment])
}

fn card(token: &str) -> Option<&'static str> {
    match token {
        "||" => Some("1"),
        "|o" | "o|" => Some("0..1"),
        "}|" | "|{" => Some("1..*"),
        "}o" | "o{" => Some("0..*"),
        _ => None,
    }
}

/// Splits `||--o{` into its two cardinalities and line style.
fn relationship(op: &str) -> Option<(&'static str, Stroke, &'static str)> {
    if op.len() != 6 || !op.is_ascii() {
        return None;
    }
    let stroke = match &op[2..4] {
        "--" => Stroke::Solid,
        ".." => Stroke::Dotted,
        _ => return None,
    };
    Some((card(&op[..2])?, stroke, card(&op[4..])?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_relationship_tokens() {
        assert_eq!(relationship("||--o{"), Some(("1", Stroke::Solid, "0..*")));
        assert_eq!(
            relationship("}|..|o"),
            Some(("1..*", Stroke::Dotted, "0..1"))
        );
        assert_eq!(relationship("--"), None);
        assert_eq!(
            attribute("int order_id PK, FK \"order key\""),
            Some([
                "int".into(),
                "order_id".into(),
                "PK,FK".into(),
                "\"order key\"".into()
            ])
        );
    }
}
