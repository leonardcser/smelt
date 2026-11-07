//! `flowchart` and `graph` diagrams.

use std::collections::HashMap;

use crate::canvas::Stroke;
use crate::graph::{Dir, Edge, Graph, Marker, Shape};
use crate::text::label_lines;
use crate::{keyword, split_top, syntax, Document, Error};

const IGNORED: &[&str] = &[
    "classDef",
    "class",
    "style",
    "linkStyle",
    "click",
    "accTitle",
    "accDescr",
    "direction",
];

pub(crate) fn parse(doc: &Document) -> Result<Graph, Error> {
    let dir = match doc.args.split_whitespace().next() {
        Some(d) => Dir::parse(d.trim_end_matches(';')).ok_or_else(|| syntax(&doc.args))?,
        None => Dir::TD,
    };
    let mut g = Graph::new(dir);
    g.title = doc.title.clone();
    let mut subgraphs: HashMap<String, Vec<String>> = HashMap::new();
    let mut depth = 0usize;
    for line in &doc.lines {
        for stmt in split_top(line, ';') {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            if let Some(rest) = keyword(stmt, "subgraph") {
                let (id, title) = subgraph_header(rest);
                subgraphs.insert(id, title);
                depth += 1;
                continue;
            }
            if stmt == "end" {
                depth = depth.checked_sub(1).ok_or_else(|| syntax(stmt))?;
                continue;
            }
            if IGNORED.iter().any(|k| keyword(stmt, k).is_some()) || stmt.starts_with("accDescr") {
                continue;
            }
            let mut parser = Parser::new(stmt, &mut g);
            if parser.chain().is_err() {
                return Err(parser.fatal.take().unwrap_or_else(|| syntax(stmt)));
            }
        }
    }
    for node in &mut g.nodes {
        let untouched = node.sections[0].lines == label_lines(&node.id);
        if let Some(title) = subgraphs.get(&node.id).filter(|_| untouched) {
            node.set_label(title.clone());
        }
    }
    Ok(g)
}

fn subgraph_header(rest: &str) -> (String, Vec<String>) {
    let rest = rest.trim();
    if let Some(open) = rest.find('[').filter(|_| rest.ends_with(']')) {
        let id = rest[..open].trim().to_string();
        return (id, label_lines(&rest[open + 1..rest.len() - 1]));
    }
    (rest.to_string(), label_lines(rest))
}

struct Link {
    stroke: Stroke,
    head: Option<Marker>,
    tail: Option<Marker>,
    label: Vec<String>,
    minlen: usize,
    visible: bool,
}

struct Parser<'a, 'g> {
    s: &'a str,
    i: usize,
    g: &'g mut Graph,
    /// Error that must not be reported as a syntax error.
    fatal: Option<Error>,
}

type Res<T> = Result<T, ()>;

impl<'a, 'g> Parser<'a, 'g> {
    fn new(s: &'a str, g: &'g mut Graph) -> Self {
        Self {
            s,
            i: 0,
            g,
            fatal: None,
        }
    }

    fn rest(&self) -> &'a str {
        &self.s[self.i..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.rest().chars().nth(n)
    }

    fn eat(&mut self, prefix: &str) -> bool {
        if self.rest().starts_with(prefix) {
            self.i += prefix.len();
            true
        } else {
            false
        }
    }

    fn ws(&mut self) {
        let trimmed = self.rest().trim_start();
        self.i = self.s.len() - trimmed.len();
    }

    fn count(&mut self, c: char) -> usize {
        let n = self.rest().chars().take_while(|&x| x == c).count();
        self.i += n * c.len_utf8();
        n
    }

    fn chain(&mut self) -> Res<()> {
        let mut left = self.group()?;
        loop {
            self.ws();
            if self.peek().is_none() {
                return Ok(());
            }
            let link = self.link()?;
            self.ws();
            let right = self.group()?;
            if link.visible {
                for &a in &left {
                    for &b in &right {
                        let mut e = Edge::new(a, b);
                        e.stroke = link.stroke;
                        e.head = link.head;
                        e.tail = link.tail;
                        e.label = link.label.clone();
                        e.minlen = link.minlen;
                        self.g.add_edge(e).map_err(|e| self.fatal = Some(e))?;
                    }
                }
            }
            left = right;
        }
    }

    fn group(&mut self) -> Res<Vec<usize>> {
        let mut nodes = vec![self.node()?];
        loop {
            let save = self.i;
            self.ws();
            if self.eat("&") {
                self.ws();
                nodes.push(self.node()?);
            } else {
                self.i = save;
                return Ok(nodes);
            }
        }
    }

    fn ident(&mut self) -> &'a str {
        let start = self.i;
        let chars: Vec<char> = self.rest().chars().collect();
        let mut k = 0;
        while k < chars.len() {
            let c = chars[k];
            let joiner = c == '-' && chars.get(k + 1).is_some_and(|n| n.is_alphanumeric());
            if c.is_alphanumeric() || c == '_' || joiner {
                k += 1;
            } else {
                break;
            }
        }
        let len: usize = chars[..k].iter().map(|c| c.len_utf8()).sum();
        self.i += len;
        &self.s[start..start + len]
    }

    fn node(&mut self) -> Res<usize> {
        let id = self.ident();
        if id.is_empty() {
            return Err(());
        }
        let idx = self.g.node(id).map_err(|e| self.fatal = Some(e))?;
        if let Some((shape, label)) = self.shape(id)? {
            let node = &mut self.g.nodes[idx];
            node.shape = shape;
            node.set_label(label);
        }
        if self.eat(":::") {
            self.ident();
        }
        Ok(idx)
    }

    fn shape(&mut self, id: &str) -> Res<Option<(Shape, Vec<String>)>> {
        const SHAPES: &[(&str, &[(&str, Shape)])] = &[
            ("(((", &[(")))", Shape::DoubleCircle)]),
            ("((", &[("))", Shape::Circle)]),
            ("([", &[("])", Shape::Round)]),
            ("[[", &[("]]", Shape::Subroutine)]),
            ("[(", &[(")]", Shape::Cylinder)]),
            ("{{", &[("}}", Shape::Diamond)]),
            (
                "[/",
                &[
                    ("/]", Shape::Slanted(['╱', '┐', '└', '╱'])),
                    ("\\]", Shape::Slanted(['╱', '╲', '└', '┘'])),
                ],
            ),
            (
                "[\\",
                &[
                    ("\\]", Shape::Slanted(['┌', '╲', '╲', '┘'])),
                    ("/]", Shape::Slanted(['┌', '┐', '╲', '╱'])),
                ],
            ),
            (">", &[("]", Shape::Slanted(['╲', '┐', '╱', '┘']))]),
            ("[", &[("]", Shape::Rect)]),
            ("(", &[(")", Shape::Round)]),
            ("{", &[("}", Shape::Diamond)]),
        ];
        if self.eat("@{") {
            return self.shape_object(id).map(Some);
        }
        for (open, closers) in SHAPES {
            if !self.rest().starts_with(open) {
                continue;
            }
            self.i += open.len();
            let body = self.rest();
            let quoted_end = body
                .starts_with('"')
                .then(|| body[1..].find('"').map(|q| q + 2))
                .flatten();
            let search_from = quoted_end.unwrap_or(0);
            let found = closers
                .iter()
                .filter_map(|(close, shape)| {
                    body[search_from..]
                        .find(close)
                        .map(|at| (search_from + at, *close, *shape))
                })
                .min_by_key(|&(at, _, _)| at);
            let (at, close, shape) = found.ok_or(())?;
            let label = label_lines(&body[..at]);
            self.i += at + close.len();
            return Ok(Some((shape, label)));
        }
        Ok(None)
    }

    /// `@{ shape: diam, label: "Text" }` node metadata.
    fn shape_object(&mut self, id: &str) -> Res<(Shape, Vec<String>)> {
        let body = self.rest();
        let end = body.find('}').ok_or(())?;
        self.i += end + 1;
        let mut shape = Shape::Rect;
        let mut label = None;
        for field in split_top(&body[..end], ',') {
            let Some((key, value)) = field.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "label" => label = Some(label_lines(value)),
                "shape" => {
                    shape = match value {
                        "rounded" | "stadium" | "event" | "terminal" | "pill" => Shape::Round,
                        "subroutine" | "subproc" | "fr-rect" => Shape::Subroutine,
                        "cyl" | "database" | "db" | "cylinder" => Shape::Cylinder,
                        "circle" | "circ" | "sm-circ" | "start" => Shape::Circle,
                        "dbl-circ" | "double-circle" | "fr-circ" | "stop" => Shape::DoubleCircle,
                        "diam" | "diamond" | "decision" | "question" | "hex" | "hexagon"
                        | "prepare" => Shape::Diamond,
                        _ => Shape::Rect,
                    }
                }
                _ => {}
            }
        }
        Ok((shape, label.unwrap_or_else(|| label_lines(id))))
    }

    fn marker(c: char) -> Option<Marker> {
        match c {
            '>' => Some(Marker::Arrow),
            'o' => Some(Marker::Circle),
            'x' => Some(Marker::Cross),
            _ => None,
        }
    }

    /// Consumes an end marker when one follows and is not the start of a
    /// node id.
    fn head(&mut self) -> Option<Marker> {
        let c = self.peek()?;
        let m = Self::marker(c)?;
        let letter = c != '>';
        if letter
            && self
                .peek_at(1)
                .is_some_and(|n| n.is_alphanumeric() || n == '_')
        {
            return None;
        }
        self.i += 1;
        Some(m)
    }

    fn link(&mut self) -> Res<Link> {
        // Edge ids: `A e1@--> B`.
        let save = self.i;
        let id = self.ident();
        if id.is_empty() || !self.eat("@") {
            self.i = save;
        }
        let mut tail = None;
        if self.eat("<") {
            tail = Some(Marker::Arrow);
        } else if let Some(m) = self.peek().and_then(Self::marker).filter(|_| {
            matches!(
                (self.peek_at(1), self.peek_at(2)),
                (Some('-'), Some('-' | '.')) | (Some('='), Some('='))
            )
        }) {
            self.i += 1;
            tail = Some(m);
        }
        let mut link = Link {
            stroke: Stroke::Solid,
            head: None,
            tail,
            label: Vec::new(),
            minlen: 1,
            visible: true,
        };
        match self.peek() {
            Some('~') => {
                if self.count('~') < 3 {
                    return Err(());
                }
                link.visible = false;
            }
            Some('=') => {
                link.stroke = Stroke::Thick;
                let n = self.count('=');
                self.finish_run(&mut link, n, '=', "==")?;
            }
            Some('-') => {
                self.i += 1;
                if self.peek() == Some('.') {
                    link.stroke = Stroke::Dotted;
                    let dots = self.count('.');
                    if self.eat("-") {
                        link.head = self.head();
                        link.minlen = dots;
                    } else if dots == 1 {
                        let close = self.rest().find(".-").ok_or(())?;
                        link.label = label_lines(&self.rest()[..close]);
                        self.i += close;
                        let dots = self.count('.');
                        self.eat("-");
                        link.head = self.head();
                        link.minlen = dots;
                    } else {
                        return Err(());
                    }
                } else {
                    let n = 1 + self.count('-');
                    self.finish_run(&mut link, n, '-', "--")?;
                }
            }
            _ => return Err(()),
        }
        self.ws();
        if self.eat("|") {
            let rest = self.rest();
            let from = match rest.trim_start().strip_prefix('"') {
                Some(q) => rest.len() - q.len() + q.find('"').ok_or(())? + 1,
                None => 0,
            };
            let end = from + rest[from..].find('|').ok_or(())?;
            link.label = label_lines(&self.rest()[..end]);
            self.i += end + 1;
        }
        Ok(link)
    }

    /// Completes a solid or thick link after its first run of `n` line
    /// characters, including the `-- text -->` labelled form.
    fn finish_run(&mut self, link: &mut Link, n: usize, c: char, open: &str) -> Res<()> {
        if n < 2 {
            return Err(());
        }
        link.head = self.head();
        if link.head.is_some() {
            link.minlen = n - 1;
            return Ok(());
        }
        let labelled = n == 2 && self.peek().is_some_and(char::is_whitespace);
        if !labelled {
            link.minlen = (n - 2).max(1);
            return Ok(());
        }
        let close = self.rest().find(open).ok_or(())?;
        link.label = label_lines(&self.rest()[..close]);
        self.i += close;
        let m = self.count(c);
        link.head = self.head();
        link.minlen = if link.head.is_some() {
            m - 1
        } else {
            (m - 2).max(1)
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(src: &str) -> Graph {
        let doc = Document::parse(src).unwrap();
        parse(&doc).unwrap()
    }

    fn edges(g: &Graph) -> Vec<String> {
        g.edges
            .iter()
            .map(|e| {
                format!(
                    "{}->{} {:?} {:?}/{:?} {:?} {}",
                    g.nodes[e.from].id,
                    g.nodes[e.to].id,
                    e.stroke,
                    e.tail,
                    e.head,
                    e.label,
                    e.minlen
                )
            })
            .collect()
    }

    #[test]
    fn parses_link_forms() {
        let g = graph(
            "flowchart LR\nA-->B\nB --- C\nC -.-> D\nD == go ==> E\nE -- yes --> F\nF -->|no| A\nA <--> C\nA --o B\nB --x C\nA ---> F\nB -. maybe .-> D",
        );
        assert_eq!(
            edges(&g),
            vec![
                "A->B Solid None/Some(Arrow) [] 1",
                "B->C Solid None/None [] 1",
                "C->D Dotted None/Some(Arrow) [] 1",
                "D->E Thick None/Some(Arrow) [\"go\"] 1",
                "E->F Solid None/Some(Arrow) [\"yes\"] 1",
                "F->A Solid None/Some(Arrow) [\"no\"] 1",
                "A->C Solid Some(Arrow)/Some(Arrow) [] 1",
                "A->B Solid None/Some(Circle) [] 1",
                "B->C Solid None/Some(Cross) [] 1",
                "A->F Solid None/Some(Arrow) [] 2",
                "B->D Dotted None/Some(Arrow) [\"maybe\"] 1",
            ]
        );
    }

    #[test]
    fn parses_shapes_groups_and_statements() {
        let g = graph(
            "graph TD;\nA[\"Start [here]\"] & B((Ring)) --> C{Ok?}; C --> D[(DB)]:::warm\nsubgraph S [Storage]\nD\nend\nS --> E>Flag]\nclassDef warm fill:#f96",
        );
        let summary: Vec<String> = g
            .nodes
            .iter()
            .map(|n| format!("{} {:?} {:?}", n.id, n.shape, n.sections[0].lines))
            .collect();
        assert_eq!(
            summary,
            vec![
                "A Rect [\"Start [here]\"]",
                "B Circle [\"Ring\"]",
                "C Diamond [\"Ok?\"]",
                "D Cylinder [\"DB\"]",
                "S Rect [\"Storage\"]",
                "E Slanted(['╲', '┐', '╱', '┘']) [\"Flag\"]",
            ]
        );
        assert_eq!(g.edges.len(), 4);
    }

    #[test]
    fn rejects_garbage() {
        let doc = Document::parse("flowchart TD\nA --> \n").unwrap();
        assert!(parse(&doc).is_err());
        let doc = Document::parse("flowchart TD\nA ?? B").unwrap();
        assert!(parse(&doc).is_err());
    }
}
