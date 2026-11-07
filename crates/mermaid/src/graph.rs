//! Node-and-edge diagrams drawn with a layered layout.
//!
//! Ranks come from longest-path layering after breaking cycles. Long edges
//! get one dummy node per crossed rank, and each edge label becomes a dummy
//! node of its own. Barycenter sweeps and adjacent swaps reduce crossings,
//! then isotonic regression places nodes near their neighbors' centers.
//! Every edge gets a separate port on each box side. In the channel between
//! two ranks, each edge segment that changes column gets its own track, so
//! two edges never share a line.
//!
//! All geometry is computed along two abstract axes: "along" follows ranks
//! and "cross" runs across them. Mapping to screen space happens only when
//! drawing, so all four directions share one layout.

use std::collections::HashMap;

use crate::canvas::{Canvas, Frame, Stroke, DOWN, LEFT, RIGHT, ROUNDED, SQUARE, UP};
use crate::text::{self, width};
use crate::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dir {
    TD,
    BT,
    LR,
    RL,
}

impl Dir {
    pub(crate) fn parse(s: &str) -> Option<Dir> {
        match s.trim().to_ascii_uppercase().as_str() {
            "TD" | "TB" => Some(Dir::TD),
            "BT" => Some(Dir::BT),
            "LR" => Some(Dir::LR),
            "RL" => Some(Dir::RL),
            _ => None,
        }
    }

    fn vertical(self) -> bool {
        matches!(self, Dir::TD | Dir::BT)
    }

    /// The vertical direction to fall back to when a horizontal layout
    /// does not fit.
    fn turned(self) -> Dir {
        match self {
            Dir::LR => Dir::TD,
            Dir::RL => Dir::BT,
            other => other,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    Rect,
    Round,
    Subroutine,
    Cylinder,
    Circle,
    DoubleCircle,
    Diamond,
    /// Rectangle with the given corners cut by slants: top-left,
    /// top-right, bottom-left, bottom-right.
    Slanted([char; 4]),
    /// A single glyph with no border, such as a state start marker.
    Dot(char),
}

impl Shape {
    fn frame(self) -> Frame {
        match self {
            Shape::Rect => SQUARE,
            Shape::Round | Shape::Circle => ROUNDED,
            Shape::Subroutine => Frame::new('─', '║', ['╓', '╖', '╙', '╜']),
            Shape::Cylinder => Frame::new('═', '│', ['╒', '╕', '╘', '╛']),
            Shape::DoubleCircle => Frame::new('═', '║', ['╔', '╗', '╚', '╝']),
            Shape::Diamond => Frame::new('─', '│', ['╱', '╲', '╲', '╱']),
            Shape::Slanted(corners) => Frame::new('─', '│', corners),
            Shape::Dot(_) => SQUARE,
        }
    }

    fn padding(self) -> usize {
        match self {
            Shape::Circle | Shape::DoubleCircle => 2,
            _ => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Align {
    Center,
    Left,
}

#[derive(Clone, Debug)]
pub(crate) struct Section {
    pub lines: Vec<String>,
    pub align: Align,
}

#[derive(Clone, Debug)]
pub(crate) struct Node {
    pub id: String,
    pub shape: Shape,
    pub sections: Vec<Section>,
}

impl Node {
    pub(crate) fn set_label(&mut self, lines: Vec<String>) {
        self.sections = vec![Section {
            lines,
            align: Align::Center,
        }];
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Marker {
    Arrow,
    Triangle,
    Diamond,
    DiamondHollow,
    Circle,
    Cross,
}

impl Marker {
    /// Glyph for a marker that points in screen direction `toward`.
    fn glyph(self, toward: u8) -> char {
        let pick = |up, right, down, left| match toward {
            UP => up,
            RIGHT => right,
            DOWN => down,
            _ => left,
        };
        match self {
            Marker::Arrow => pick('▲', '▶', '▼', '◀'),
            Marker::Triangle => pick('△', '▷', '▽', '◁'),
            Marker::Diamond => '◆',
            Marker::DiamondHollow => '◇',
            Marker::Circle => '○',
            Marker::Cross => '×',
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Edge {
    pub from: usize,
    pub to: usize,
    pub label: Vec<String>,
    pub stroke: Stroke,
    /// Marker drawn where the edge meets `to`.
    pub head: Option<Marker>,
    /// Marker drawn where the edge meets `from`.
    pub tail: Option<Marker>,
    /// Minimum number of ranks the edge spans.
    pub minlen: usize,
}

impl Edge {
    pub(crate) fn new(from: usize, to: usize) -> Self {
        Self {
            from,
            to,
            label: Vec::new(),
            stroke: Stroke::Solid,
            head: Some(Marker::Arrow),
            tail: None,
            minlen: 1,
        }
    }
}

pub(crate) struct Graph {
    pub dir: Dir,
    pub title: Option<String>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    index: HashMap<String, usize>,
}

/// Upper bounds that keep layout time and output size reasonable.
const MAX_NODES: usize = 200;
const MAX_EDGES: usize = 400;
const MAX_DUMMIES: usize = 4000;

impl Graph {
    pub(crate) fn new(dir: Dir) -> Self {
        Self {
            dir,
            title: None,
            nodes: Vec::new(),
            edges: Vec::new(),
            index: HashMap::new(),
        }
    }

    pub(crate) fn find(&self, id: &str) -> Option<usize> {
        self.index.get(id).copied()
    }

    /// Returns the node with `id`, creating a rectangle labelled with the
    /// id when it does not exist yet.
    pub(crate) fn node(&mut self, id: &str) -> Result<usize, Error> {
        if let Some(&i) = self.index.get(id) {
            return Ok(i);
        }
        if self.nodes.len() >= MAX_NODES {
            return Err(Error::TooLarge);
        }
        let i = self.nodes.len();
        self.nodes.push(Node {
            id: id.to_string(),
            shape: Shape::Rect,
            sections: vec![Section {
                lines: text::label_lines(id),
                align: Align::Center,
            }],
        });
        self.index.insert(id.to_string(), i);
        Ok(i)
    }

    pub(crate) fn add_edge(&mut self, edge: Edge) -> Result<(), Error> {
        if self.edges.len() >= MAX_EDGES {
            return Err(Error::TooLarge);
        }
        self.edges.push(edge);
        Ok(())
    }

    /// Renders within `max_width` columns, wrapping labels and turning
    /// horizontal layouts vertical before giving up.
    pub(crate) fn render(&self, max_width: usize) -> Result<String, Error> {
        if self.nodes.is_empty() {
            return Err(Error::Syntax("diagram has no nodes".into()));
        }
        let mut attempts = vec![(self.dir, 28), (self.dir, 16)];
        if !self.dir.vertical() {
            attempts.extend([(self.dir.turned(), 28), (self.dir.turned(), 16)]);
        }
        attempts.push((self.dir.turned(), 10));
        for (dir, wrap) in attempts {
            let body = Layout::new(self, dir, wrap)?.draw();
            if body.lines().all(|l| width(l) <= max_width) {
                return Ok(match &self.title {
                    Some(title) => {
                        let title = text::wrap(title, max_width).join("\n");
                        format!("{title}\n\n{body}")
                    }
                    None => body,
                });
            }
        }
        Err(Error::TooWide)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Real(usize),
    Dummy,
    Label(usize),
}

/// A node in the layered graph: a real node, a dummy on a long edge, or an
/// edge label.
struct LNode {
    kind: Kind,
    rank: usize,
    along: usize,
    cross: usize,
    /// Space past the cross end reserved for self-loops.
    extra: usize,
    pos: usize,
    r0: usize,
}

impl LNode {
    fn center(&self) -> f64 {
        self.pos as f64 + (self.cross as f64 - 1.0) / 2.0
    }

    fn fixed_pin(&self, real: &[RealBox]) -> Option<usize> {
        match self.kind {
            Kind::Dummy => Some(self.pos),
            Kind::Label(_) => Some(self.pos + self.cross / 2),
            Kind::Real(i) if real[i].dot => Some(self.pos),
            Kind::Real(_) => None,
        }
    }
}

struct RealBox {
    sections: Vec<Section>,
    /// Screen width and height of the drawn box.
    w: usize,
    h: usize,
    dot: bool,
}

/// One rank-to-rank piece of an edge, upper rank first.
struct Seg {
    edge: usize,
    upper: usize,
    lower: usize,
    pin_u: usize,
    pin_l: usize,
    net: usize,
    mark_u: bool,
    mark_l: bool,
    track: Option<usize>,
}

struct Chain {
    nodes: Vec<usize>,
    reversed: bool,
}

struct Channel {
    start: usize,
    top_pad: usize,
    lead: usize,
}

struct Layout<'g> {
    g: &'g Graph,
    dir: Dir,
    real: Vec<RealBox>,
    nodes: Vec<LNode>,
    layers: Vec<Vec<usize>>,
    chains: Vec<Option<Chain>>,
    segs: Vec<Seg>,
    loops: Vec<Vec<usize>>,
    loop_labels: Vec<Vec<String>>,
    edge_labels: Vec<Vec<String>>,
    channels: Vec<Channel>,
    ups: Vec<Vec<usize>>,
    downs: Vec<Vec<usize>>,
    along_total: usize,
    cross_total: usize,
}

impl<'g> Layout<'g> {
    fn new(g: &'g Graph, dir: Dir, wrap: usize) -> Result<Self, Error> {
        let mut lay = Layout {
            g,
            dir,
            real: Vec::new(),
            nodes: Vec::new(),
            layers: Vec::new(),
            chains: Vec::new(),
            segs: Vec::new(),
            loops: vec![Vec::new(); g.nodes.len()],
            loop_labels: vec![Vec::new(); g.nodes.len()],
            edge_labels: g
                .edges
                .iter()
                .map(|e| text::wrap_lines(&e.label, wrap))
                .collect(),
            channels: Vec::new(),
            ups: Vec::new(),
            downs: Vec::new(),
            along_total: 0,
            cross_total: 0,
        };
        lay.measure(wrap);
        let reversed = lay.break_cycles();
        let ranks = lay.rank(&reversed);
        lay.build_chains(&ranks, &reversed)?;
        lay.size_nodes();
        (lay.ups, lay.downs) = lay.adjacency();
        lay.order();
        lay.place();
        lay.assign_pins();
        lay.straighten();
        lay.assign_pins();
        lay.route();
        Ok(lay)
    }

    fn vertical(&self) -> bool {
        self.dir.vertical()
    }

    fn measure(&mut self, wrap: usize) {
        for (i, node) in self.g.nodes.iter().enumerate() {
            let sections: Vec<Section> = node
                .sections
                .iter()
                .map(|s| Section {
                    lines: match s.align {
                        Align::Center => text::wrap_lines(&s.lines, wrap),
                        Align::Left => text::wrap_lines(&s.lines, wrap * 2),
                    },
                    align: s.align,
                })
                .collect();
            let (w, h, dot) = match node.shape {
                Shape::Dot(_) => (1, 1, true),
                shape => {
                    let text_w = sections
                        .iter()
                        .map(|s| text::max_width(&s.lines))
                        .max()
                        .unwrap_or(0);
                    let rows: usize = sections.iter().map(|s| s.lines.len().max(1)).sum();
                    let w = text_w + 2 * shape.padding() + 2;
                    (w.max(5), rows + sections.len().saturating_sub(1) + 2, false)
                }
            };
            self.real.push(RealBox {
                sections,
                w,
                h,
                dot,
            });
            self.nodes.push(LNode {
                kind: Kind::Real(i),
                rank: 0,
                along: 0,
                cross: 0,
                extra: 0,
                pos: 0,
                r0: 0,
            });
        }
        for (e, edge) in self.g.edges.iter().enumerate() {
            if edge.from == edge.to && !self.real[edge.from].dot {
                self.loops[edge.from].push(e);
                let lines = self.edge_labels[e].clone();
                self.loop_labels[edge.from].extend(lines);
            }
        }
    }

    fn is_layered(&self, e: usize) -> bool {
        self.g.edges[e].from != self.g.edges[e].to
    }

    /// Marks edges whose direction is flipped to make the graph acyclic.
    fn break_cycles(&self) -> Vec<bool> {
        let n = self.g.nodes.len();
        let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (e, edge) in self.g.edges.iter().enumerate() {
            if self.is_layered(e) {
                out[edge.from].push(e);
            }
        }
        let mut reversed = vec![false; self.g.edges.len()];
        // 0 = unvisited, 1 = on stack, 2 = done.
        let mut state = vec![0u8; n];
        for root in 0..n {
            if state[root] != 0 {
                continue;
            }
            let mut stack = vec![(root, 0usize)];
            state[root] = 1;
            while let Some(&mut (v, ref mut next)) = stack.last_mut() {
                if let Some(&e) = out[v].get(*next) {
                    *next += 1;
                    let w = self.g.edges[e].to;
                    match state[w] {
                        0 => {
                            state[w] = 1;
                            stack.push((w, 0));
                        }
                        1 => reversed[e] = true,
                        _ => {}
                    }
                } else {
                    state[v] = 2;
                    stack.pop();
                }
            }
        }
        reversed
    }

    fn doubled(&self) -> bool {
        self.g
            .edges
            .iter()
            .enumerate()
            .any(|(e, edge)| self.is_layered(e) && !edge.label.is_empty())
    }

    fn ends(&self, e: usize, reversed: &[bool]) -> (usize, usize) {
        let edge = &self.g.edges[e];
        if reversed[e] {
            (edge.to, edge.from)
        } else {
            (edge.from, edge.to)
        }
    }

    fn rank(&self, reversed: &[bool]) -> Vec<usize> {
        let n = self.g.nodes.len();
        let factor = if self.doubled() { 2 } else { 1 };
        let mut indeg = vec![0usize; n];
        let mut out: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
        for (e, edge) in self.g.edges.iter().enumerate() {
            if !self.is_layered(e) {
                continue;
            }
            let (u, v) = self.ends(e, reversed);
            out[u].push((v, edge.minlen.max(1) * factor));
            indeg[v] += 1;
        }
        let has_in: Vec<bool> = indeg.iter().map(|&d| d > 0).collect();
        let mut rank = vec![0usize; n];
        let mut queue: Vec<usize> = (0..n).filter(|&v| indeg[v] == 0).collect();
        let mut topo = Vec::with_capacity(n);
        while let Some(v) = queue.pop() {
            topo.push(v);
            for &(w, len) in &out[v] {
                rank[w] = rank[w].max(rank[v] + len);
                indeg[w] -= 1;
                if indeg[w] == 0 {
                    queue.push(w);
                }
            }
        }
        // Pull sources down next to their nearest successor.
        for v in 0..n {
            if has_in[v] {
                continue;
            }
            if let Some(r) = out[v].iter().map(|&(w, len)| rank[w] - len).min() {
                rank[v] = rank[v].max(r);
            }
        }
        rank
    }

    fn add_lnode(&mut self, kind: Kind, rank: usize) -> Result<usize, Error> {
        if self.nodes.len() > self.g.nodes.len() + MAX_DUMMIES {
            return Err(Error::TooLarge);
        }
        self.nodes.push(LNode {
            kind,
            rank,
            along: 0,
            cross: 1,
            extra: 0,
            pos: 0,
            r0: 0,
        });
        Ok(self.nodes.len() - 1)
    }

    fn build_chains(&mut self, ranks: &[usize], reversed: &[bool]) -> Result<(), Error> {
        for (i, &r) in ranks.iter().enumerate() {
            self.nodes[i].rank = r;
        }
        let edge_count = self.g.edges.len();
        let node_count = self.g.nodes.len();
        for e in 0..edge_count {
            if !self.is_layered(e) {
                self.chains.push(None);
                continue;
            }
            let (u, v) = self.ends(e, reversed);
            let (ru, rv) = (ranks[u], ranks[v]);
            let edge = &self.g.edges[e];
            let label_rank = (!edge.label.is_empty()).then(|| {
                let mut m = (ru + rv) / 2;
                if m % 2 == 0 {
                    m = if m > ru + 1 { m - 1 } else { m + 1 };
                }
                m
            });
            let mut nodes = vec![u];
            for r in ru + 1..rv {
                let kind = if Some(r) == label_rank {
                    Kind::Label(e)
                } else {
                    Kind::Dummy
                };
                nodes.push(self.add_lnode(kind, r)?);
            }
            nodes.push(v);
            self.chains.push(Some(Chain {
                nodes,
                reversed: reversed[e],
            }));
        }
        for e in 0..edge_count {
            let Some(chain) = &self.chains[e] else {
                continue;
            };
            let edge = &self.g.edges[e];
            let (mark_u, mark_l) = if chain.reversed {
                (edge.head, edge.tail)
            } else {
                (edge.tail, edge.head)
            };
            let pairs: Vec<(usize, usize)> = chain.nodes.windows(2).map(|w| (w[0], w[1])).collect();
            let last = pairs.len() - 1;
            for (k, (a, b)) in pairs.into_iter().enumerate() {
                let dot =
                    |n: usize| matches!(self.nodes[n].kind, Kind::Real(i) if self.real[i].dot);
                let net = if dot(a) {
                    edge_count + a
                } else if dot(b) {
                    edge_count + self.nodes.len() + b
                } else {
                    e
                };
                self.segs.push(Seg {
                    edge: e,
                    upper: a,
                    lower: b,
                    pin_u: 0,
                    pin_l: 0,
                    net,
                    mark_u: k == 0 && mark_u.is_some(),
                    mark_l: k == last && mark_l.is_some(),
                    track: None,
                });
            }
        }
        for n in node_count..self.nodes.len() {
            if let Kind::Label(e) = self.nodes[n].kind {
                let lines = &self.edge_labels[e];
                let (w, h) = (text::max_width(lines), lines.len());
                let node = &mut self.nodes[n];
                (node.along, node.cross) = if self.dir.vertical() { (h, w) } else { (w, h) };
            }
        }
        let max_rank = self.nodes.iter().map(|n| n.rank).max().unwrap_or(0);
        self.layers = vec![Vec::new(); max_rank + 1];
        Ok(())
    }

    /// Final box sizes: wide enough for one port per attached edge with a
    /// gap between ports, and room for self-loops.
    fn size_nodes(&mut self) {
        let mut up = vec![0usize; self.nodes.len()];
        let mut down = vec![0usize; self.nodes.len()];
        for s in &self.segs {
            down[s.upper] += 1;
            up[s.lower] += 1;
        }
        let vertical = self.vertical();
        for i in 0..self.g.nodes.len() {
            let ports = up[i].max(down[i]);
            let b = &mut self.real[i];
            if !b.dot {
                let need = 2 * ports + 1;
                if vertical {
                    b.w = b.w.max(need);
                } else {
                    b.h = b.h.max(need);
                }
            }
            let mut extra = 0;
            if !self.loops[i].is_empty() {
                let label = &self.loop_labels[i];
                let (lw, lh) = (text::max_width(label), label.len());
                if vertical {
                    b.h = b.h.max(4).max(lh + 2);
                    extra = 3 + if lw > 0 { lw + 1 } else { 0 };
                } else {
                    b.w = b.w.max(lw + 2).max(5);
                    extra = 2 + lh;
                }
            }
            let (along, cross) = if vertical { (b.h, b.w) } else { (b.w, b.h) };
            let node = &mut self.nodes[i];
            node.along = along;
            node.cross = cross;
            node.extra = extra;
        }
    }

    fn adjacency(&self) -> (Vec<Vec<usize>>, Vec<Vec<usize>>) {
        let mut ups = vec![Vec::new(); self.nodes.len()];
        let mut downs = vec![Vec::new(); self.nodes.len()];
        for s in &self.segs {
            downs[s.upper].push(s.lower);
            ups[s.lower].push(s.upper);
        }
        (ups, downs)
    }

    fn order(&mut self) {
        let (ups, downs) = self.adjacency();
        // Depth-first from nodes in declaration order keeps chains together.
        let mut seen = vec![false; self.nodes.len()];
        for root in 0..self.nodes.len() {
            if !seen[root] {
                self.visit(root, &downs, &mut seen);
            }
        }
        let mut index = vec![0usize; self.nodes.len()];
        let reindex = |layers: &Vec<Vec<usize>>, index: &mut Vec<usize>| {
            for layer in layers {
                for (i, &n) in layer.iter().enumerate() {
                    index[n] = i;
                }
            }
        };
        reindex(&self.layers, &mut index);
        let mut best = self.layers.clone();
        let mut best_cost = self.crossings(&downs, &index);
        for sweep in 0..12 {
            let down_sweep = sweep % 2 == 0;
            let ranks: Vec<usize> = if down_sweep {
                (1..self.layers.len()).collect()
            } else {
                (0..self.layers.len().saturating_sub(1)).rev().collect()
            };
            for r in ranks {
                let nbrs = if down_sweep { &ups } else { &downs };
                let mut keyed: Vec<(f64, usize)> = self.layers[r]
                    .iter()
                    .map(|&n| {
                        let list = &nbrs[n];
                        let key = if list.is_empty() {
                            index[n] as f64
                        } else {
                            list.iter().map(|&m| index[m] as f64).sum::<f64>() / list.len() as f64
                        };
                        (key, n)
                    })
                    .collect();
                keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
                self.layers[r] = keyed.into_iter().map(|(_, n)| n).collect();
                for (i, &n) in self.layers[r].iter().enumerate() {
                    index[n] = i;
                }
            }
            self.transpose(&ups, &downs, &mut index);
            let cost = self.crossings(&downs, &index);
            if cost < best_cost {
                best_cost = cost;
                best = self.layers.clone();
            }
            if best_cost == 0 {
                break;
            }
        }
        self.layers = best;
    }

    fn visit(&mut self, root: usize, downs: &[Vec<usize>], seen: &mut [bool]) {
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            if seen[n] {
                continue;
            }
            seen[n] = true;
            self.layers[self.nodes[n].rank].push(n);
            for &m in downs[n].iter().rev() {
                if !seen[m] {
                    stack.push(m);
                }
            }
        }
    }

    fn crossings(&self, downs: &[Vec<usize>], index: &[usize]) -> usize {
        let mut total = 0;
        for layer in &self.layers {
            let mut pairs: Vec<(usize, usize)> = Vec::new();
            for &n in layer {
                for &m in &downs[n] {
                    pairs.push((index[n], index[m]));
                }
            }
            for i in 0..pairs.len() {
                for j in i + 1..pairs.len() {
                    let (a, b) = (pairs[i], pairs[j]);
                    if (a.0 < b.0 && a.1 > b.1) || (a.0 > b.0 && a.1 < b.1) {
                        total += 1;
                    }
                }
            }
        }
        total
    }

    /// Swaps adjacent nodes while that removes crossings.
    fn transpose(&mut self, ups: &[Vec<usize>], downs: &[Vec<usize>], index: &mut [usize]) {
        let pair_cost = |u: usize, v: usize, index: &[usize]| {
            let mut c = 0;
            for nbrs in [ups, downs] {
                for &a in &nbrs[u] {
                    for &b in &nbrs[v] {
                        if index[a] > index[b] {
                            c += 1;
                        }
                    }
                }
            }
            c
        };
        for _ in 0..4 {
            let mut improved = false;
            for r in 0..self.layers.len() {
                for i in 1..self.layers[r].len() {
                    let (u, v) = (self.layers[r][i - 1], self.layers[r][i]);
                    if pair_cost(v, u, index) < pair_cost(u, v, index) {
                        self.layers[r].swap(i - 1, i);
                        index[u] = i;
                        index[v] = i - 1;
                        improved = true;
                    }
                }
            }
            if !improved {
                break;
            }
        }
    }

    fn gap(&self, a: usize, b: usize) -> usize {
        let real = |n: usize| !matches!(self.nodes[n].kind, Kind::Dummy);
        match (self.vertical(), real(a) && real(b)) {
            (true, true) => 3,
            _ => 1,
        }
    }

    /// Cross coordinates: repeated sweeps pull each node toward the median
    /// of its neighbors while keeping order and spacing.
    fn place(&mut self) {
        let (ups, downs) = self.adjacency();
        let mut x: Vec<f64> = vec![0.0; self.nodes.len()];
        for layer in &self.layers {
            let mut at = 0.0;
            for (i, &n) in layer.iter().enumerate() {
                if i > 0 {
                    at += self.gap(layer[i - 1], n) as f64;
                }
                x[n] = at;
                at += (self.nodes[n].cross + self.nodes[n].extra) as f64;
            }
        }
        let both: Vec<Vec<usize>> = (0..self.nodes.len())
            .map(|n| ups[n].iter().chain(&downs[n]).copied().collect())
            .collect();
        for iter in 0..8 {
            let ranks: Vec<usize> = if iter % 2 == 0 {
                (1..self.layers.len()).collect()
            } else {
                (0..self.layers.len().saturating_sub(1)).rev().collect()
            };
            let nbrs = if iter % 2 == 0 { &ups } else { &downs };
            for r in ranks {
                self.place_layer(r, nbrs, false, &mut x);
            }
        }
        // Dummies snap to one neighbor so long edges bend once, not twice.
        for _ in 0..2 {
            for r in 0..self.layers.len() {
                self.place_layer(r, &both, true, &mut x);
            }
        }
        let min = x.iter().copied().fold(f64::INFINITY, f64::min);
        for (n, node) in self.nodes.iter_mut().enumerate() {
            node.pos = (x[n] - min).round() as usize;
        }
        self.cross_total = self
            .nodes
            .iter()
            .map(|n| n.pos + n.cross + n.extra)
            .max()
            .unwrap_or(0);
    }

    fn place_layer(&self, r: usize, nbrs: &[Vec<usize>], snap: bool, x: &mut [f64]) {
        let layer = &self.layers[r];
        let center = |n: usize, x: &[f64]| x[n] + (self.nodes[n].cross as f64 - 1.0) / 2.0;
        // Isotonic regression on positions relative to a packed layout,
        // solved with pool-adjacent-violators.
        let mut offset = 0.0;
        let mut blocks: Vec<(f64, f64, usize)> = Vec::new();
        let mut offsets = Vec::with_capacity(layer.len());
        for (i, &n) in layer.iter().enumerate() {
            if i > 0 {
                offset += self.gap(layer[i - 1], n) as f64;
            }
            offsets.push(offset);
            let mut cs: Vec<f64> = nbrs[n]
                .iter()
                .map(|&m| self.port_estimate(m, n, x))
                .collect();
            cs.sort_by(f64::total_cmp);
            let here = center(n, x);
            let dummy = !matches!(self.nodes[n].kind, Kind::Real(_));
            let want = match cs.len() {
                0 => here,
                _ if snap && dummy => cs
                    .iter()
                    .copied()
                    .min_by(|a, b| (a - here).abs().total_cmp(&(b - here).abs()))
                    .unwrap_or(here),
                k if k % 2 == 1 => cs[k / 2],
                k => (cs[k / 2 - 1] + cs[k / 2]) / 2.0,
            };
            let weight = match self.nodes[n].kind {
                Kind::Dummy => 4.0,
                Kind::Label(_) => 2.0,
                Kind::Real(_) if cs.is_empty() => 0.5,
                Kind::Real(_) => 1.0,
            };
            let y = want - (self.nodes[n].cross as f64 - 1.0) / 2.0 - offset;
            blocks.push((weight, weight * y, 1));
            while blocks.len() > 1 {
                let (w2, s2, c2) = blocks[blocks.len() - 1];
                let (w1, s1, c1) = blocks[blocks.len() - 2];
                if s1 / w1 <= s2 / w2 {
                    break;
                }
                blocks.pop();
                *blocks.last_mut().unwrap() = (w1 + w2, s1 + s2, c1 + c2);
            }
            offset += (self.nodes[n].cross + self.nodes[n].extra) as f64;
        }
        let mut i = 0;
        for (w, s, count) in blocks {
            let y = (s / w).round();
            for _ in 0..count {
                x[layer[i]] = y + offsets[i];
                i += 1;
            }
        }
    }

    /// Where the port on box `m` facing neighbor `n` will roughly land:
    /// ports spread evenly in the order of their neighbors.
    fn port_estimate(&self, m: usize, n: usize, x: &[f64]) -> f64 {
        let node = &self.nodes[m];
        let center = |k: usize| x[k] + (self.nodes[k].cross as f64 - 1.0) / 2.0;
        if !matches!(node.kind, Kind::Real(i) if !self.real[i].dot) {
            return center(m);
        }
        let side = if node.rank < self.nodes[n].rank {
            &self.downs[m]
        } else {
            &self.ups[m]
        };
        let mut list = side.clone();
        list.sort_by(|&a, &b| center(a).total_cmp(&center(b)).then(a.cmp(&b)));
        list.dedup();
        let k = list.len();
        let Some(idx) = list.iter().position(|&o| o == n).filter(|_| k > 1) else {
            return center(m);
        };
        let cells = node.cross - 2;
        x[m] + 1.0 + ((2 * idx + 1) * cells / (2 * k)) as f64
    }

    /// Moves every run of dummies on one edge to a shared column when the
    /// layers around them leave room, so long edges run straight.
    fn straighten(&mut self) {
        let mut slot = vec![0usize; self.nodes.len()];
        for layer in &self.layers {
            for (i, &n) in layer.iter().enumerate() {
                slot[n] = i;
            }
        }
        for e in 0..self.chains.len() {
            let Some(chain) = &self.chains[e] else {
                continue;
            };
            let inner = &chain.nodes[1..chain.nodes.len() - 1];
            if inner.is_empty() {
                continue;
            }
            // Feasible range for the shared pin column.
            let (mut lo, mut hi) = (0usize, usize::MAX);
            for &d in inner {
                let node = &self.nodes[d];
                let layer = &self.layers[node.rank];
                let i = slot[d];
                let shift = node.pos + node.cross / 2 - node.pos;
                if i > 0 {
                    let left = &self.nodes[layer[i - 1]];
                    let min_pos = left.pos + left.cross + left.extra + self.gap(layer[i - 1], d);
                    lo = lo.max(min_pos + shift);
                }
                if i + 1 < layer.len() {
                    let right = &self.nodes[layer[i + 1]];
                    let room = (right.pos)
                        .saturating_sub(self.gap(d, layer[i + 1]) + node.cross + node.extra);
                    hi = hi.min(room + shift);
                }
            }
            if lo > hi {
                continue;
            }
            let segs: Vec<&Seg> = self.segs.iter().filter(|seg| seg.edge == e).collect();
            let ends = [segs[0].pin_u, segs[segs.len() - 1].pin_l];
            let (top, bottom) = (chain.nodes[0], chain.nodes[chain.nodes.len() - 1]);
            // A box port can slide along its side while it keeps its order
            // and spacing among the other ports there.
            let reach = |end: usize, others: &[usize], pin: usize, c: usize| {
                if pin == c {
                    return true;
                }
                if self.nodes[end].fixed_pin(&self.real).is_some() {
                    return false;
                }
                let (a, b) = self.interior(end);
                let before = |x: usize| others.iter().filter(|&&p| p < x).count();
                (a..=b).contains(&c)
                    && others.iter().all(|&p| p.abs_diff(c) >= 2)
                    && before(c) == before(pin)
            };
            let first = segs[0] as *const Seg;
            let last = segs[segs.len() - 1] as *const Seg;
            let top_others: Vec<usize> = self
                .segs
                .iter()
                .filter(|o| o.upper == top && !std::ptr::eq(*o, first))
                .map(|o| o.pin_u)
                .collect();
            let bottom_others: Vec<usize> = self
                .segs
                .iter()
                .filter(|o| o.lower == bottom && !std::ptr::eq(*o, last))
                .map(|o| o.pin_l)
                .collect();
            let mut candidates: Vec<usize> = ends.to_vec();
            candidates.extend(inner.iter().map(|&d| {
                let node = &self.nodes[d];
                node.pos + node.cross / 2
            }));
            let score = |c: usize| {
                let straight = usize::from(reach(top, &top_others, ends[0], c))
                    + usize::from(reach(bottom, &bottom_others, ends[1], c));
                let moved: usize = inner
                    .iter()
                    .map(|&d| (self.nodes[d].pos + self.nodes[d].cross / 2).abs_diff(c))
                    .sum();
                (usize::MAX - straight, moved)
            };
            let Some(best) = candidates
                .into_iter()
                .filter(|c| (lo..=hi).contains(c))
                .min_by_key(|&c| score(c))
            else {
                continue;
            };
            for &d in inner {
                let node = &mut self.nodes[d];
                node.pos = best - node.cross / 2;
            }
        }
    }

    fn interior(&self, n: usize) -> (usize, usize) {
        let node = &self.nodes[n];
        (node.pos + 1, node.pos + node.cross - 2)
    }

    /// Picks a port for every segment end. Boxes spread their ports two
    /// cells apart and pull each one toward the pin at the other end.
    fn assign_pins(&mut self) {
        let real = &self.real;
        let nodes = &self.nodes;
        let mut pin_u: Vec<usize> = Vec::with_capacity(self.segs.len());
        let mut pin_l: Vec<usize> = Vec::with_capacity(self.segs.len());
        for s in &self.segs {
            let pin = |n: usize| {
                nodes[n]
                    .fixed_pin(real)
                    .unwrap_or_else(|| nodes[n].center().round() as usize)
            };
            pin_u.push(pin(s.upper));
            pin_l.push(pin(s.lower));
        }
        let mut sides: Vec<(Vec<usize>, Vec<usize>)> =
            vec![(Vec::new(), Vec::new()); self.nodes.len()];
        for (i, s) in self.segs.iter().enumerate() {
            sides[s.upper].1.push(i);
            sides[s.lower].0.push(i);
        }
        let mut by_rank: Vec<usize> = (0..self.g.nodes.len())
            .filter(|&i| !self.real[i].dot)
            .collect();
        by_rank.sort_by_key(|&n| self.nodes[n].rank);
        let passes = [false, true, false];
        for reverse in passes {
            let order: Vec<usize> = if reverse {
                by_rank.iter().rev().copied().collect()
            } else {
                by_rank.clone()
            };
            for n in order {
                let (lo, hi) = self.interior(n);
                // Upper side: this node is the lower end of those segments.
                let mut segs = sides[n].0.clone();
                segs.sort_by_key(|&i| (pin_u[i], self.segs[i].edge));
                let targets: Vec<usize> = segs.iter().map(|&i| pin_u[i]).collect();
                for (&i, p) in segs.iter().zip(spread(&targets, lo, hi)) {
                    pin_l[i] = p;
                }
                let mut segs = sides[n].1.clone();
                segs.sort_by_key(|&i| (pin_l[i], self.segs[i].edge));
                let targets: Vec<usize> = segs.iter().map(|&i| pin_l[i]).collect();
                for (&i, p) in segs.iter().zip(spread(&targets, lo, hi)) {
                    pin_u[i] = p;
                }
            }
        }
        for (i, s) in self.segs.iter_mut().enumerate() {
            s.pin_u = pin_u[i];
            s.pin_l = pin_l[i];
        }
    }

    fn real_layer(&self, r: usize) -> bool {
        self.layers[r]
            .iter()
            .any(|&n| matches!(self.nodes[n].kind, Kind::Real(_)))
    }

    /// Assigns tracks in every channel and computes along coordinates.
    fn route(&mut self) {
        let ranks = self.layers.len();
        let mut by_channel: Vec<Vec<usize>> = vec![Vec::new(); ranks];
        for (i, s) in self.segs.iter().enumerate() {
            by_channel[self.nodes[s.upper].rank].push(i);
        }
        let mut at = 0;
        for (r, segs) in by_channel.iter().enumerate() {
            let layer_along = self.layers[r]
                .iter()
                .map(|&n| self.nodes[n].along)
                .max()
                .unwrap_or(0);
            for &n in &self.layers[r] {
                let node = &mut self.nodes[n];
                node.r0 = at + (layer_along - node.along.min(layer_along)) / 2;
                if node.kind == Kind::Dummy {
                    node.r0 = at;
                    node.along = layer_along;
                }
            }
            at += layer_along;
            if r + 1 == ranks {
                break;
            }
            let tracks = self.assign_tracks(segs);
            let top_pad = usize::from(segs.iter().any(|&i| self.segs[i].mark_u));
            let bottom_pad = usize::from(segs.iter().any(|&i| self.segs[i].mark_l));
            let lead = if tracks > 0 && !self.vertical() { 1 } else { 0 };
            let (upper_real, lower_real) = (self.real_layer(r), self.real_layer(r + 1));
            let min_base = match (self.vertical(), upper_real, lower_real) {
                (true, true, false) => 1,
                (true, false, true) => 0,
                (true, _, _) => 1,
                (false, true, true) => 3,
                (false, true, false) => 2,
                (false, _, _) => 1,
            };
            let base = (2 * lead + tracks).max(min_base);
            self.channels.push(Channel {
                start: at,
                top_pad,
                lead,
            });
            at += top_pad + base + bottom_pad;
        }
        self.along_total = at;
    }

    /// Greedy track assignment. A segment whose upper pin shares a column
    /// with another segment's lower pin must turn above it.
    fn assign_tracks(&mut self, segs: &[usize]) -> usize {
        let movers: Vec<usize> = segs
            .iter()
            .copied()
            .filter(|&i| self.segs[i].pin_u != self.segs[i].pin_l)
            .collect();
        let n = movers.len();
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
        for a in 0..n {
            for b in 0..n {
                let (sa, sb) = (&self.segs[movers[a]], &self.segs[movers[b]]);
                if a != b && sa.pin_u == sb.pin_l && sa.net != sb.net {
                    preds[b].push(a);
                }
            }
        }
        let key = |i: usize| {
            let s = &self.segs[movers[i]];
            if s.pin_l > s.pin_u {
                (0, usize::MAX - s.pin_u)
            } else {
                (1, s.pin_u)
            }
        };
        let mut done = vec![false; n];
        let mut track_of = vec![0usize; n];
        let mut tracks: Vec<Vec<usize>> = Vec::new();
        for _ in 0..n {
            let ready = (0..n)
                .filter(|&i| !done[i])
                .min_by_key(|&i| {
                    let waiting = preds[i].iter().filter(|&&p| !done[p]).count();
                    (waiting, key(i))
                })
                .unwrap();
            done[ready] = true;
            let floor = preds[ready]
                .iter()
                .filter(|&&p| done[p] && p != ready)
                .map(|&p| track_of[p] + 1)
                .max()
                .unwrap_or(0);
            let me = &self.segs[movers[ready]];
            let span = (me.pin_u.min(me.pin_l), me.pin_u.max(me.pin_l));
            let mut t = floor;
            while t < tracks.len()
                && !tracks[t].iter().all(|&o| {
                    let other = &self.segs[movers[o]];
                    let os = (other.pin_u.min(other.pin_l), other.pin_u.max(other.pin_l));
                    other.net == me.net || span.1 + 1 < os.0 || os.1 + 1 < span.0
                })
            {
                t += 1;
            }
            if t == tracks.len() {
                tracks.push(Vec::new());
            }
            tracks[t].push(ready);
            track_of[ready] = t;
        }
        for (i, &s) in movers.iter().enumerate() {
            self.segs[s].track = Some(track_of[i]);
        }
        tracks.len()
    }

    fn screen_point(&self, r: usize, c: usize) -> (usize, usize) {
        match self.dir {
            Dir::TD => (c, r),
            Dir::BT => (c, self.along_total - 1 - r),
            Dir::LR => (r, c),
            Dir::RL => (self.along_total - 1 - r, c),
        }
    }

    /// Screen rectangle for an abstract one: x, y, w, h.
    fn screen_rect(
        &self,
        r0: usize,
        c0: usize,
        along: usize,
        cross: usize,
    ) -> (usize, usize, usize, usize) {
        match self.dir {
            Dir::TD => (c0, r0, cross, along),
            Dir::BT => (c0, self.along_total - r0 - along, cross, along),
            Dir::LR => (r0, c0, along, cross),
            Dir::RL => (self.along_total - r0 - along, c0, along, cross),
        }
    }

    fn screen_size(&self) -> (usize, usize) {
        if self.vertical() {
            (self.cross_total, self.along_total)
        } else {
            (self.along_total, self.cross_total)
        }
    }

    fn draw(&self) -> String {
        let (w, h) = self.screen_size();
        let mut canvas = Canvas::new(w.max(1), h.max(1));
        for i in 0..self.g.nodes.len() {
            self.draw_node(&mut canvas, i);
        }
        for (e, chain) in self.chains.iter().enumerate() {
            if let Some(chain) = chain {
                self.draw_chain(&mut canvas, e, chain);
            }
        }
        for i in 0..self.g.nodes.len() {
            self.draw_loops(&mut canvas, i);
        }
        for n in self.g.nodes.len()..self.nodes.len() {
            if let Kind::Label(e) = self.nodes[n].kind {
                let node = &self.nodes[n];
                let rect = self.screen_rect(node.r0, node.pos, node.along, node.cross);
                draw_text_block(&mut canvas, rect, &self.edge_labels[e], Align::Center);
            }
        }
        canvas.render()
    }

    fn draw_node(&self, canvas: &mut Canvas, i: usize) {
        let node = &self.nodes[i];
        let b = &self.real[i];
        let (x, y, w, h) = self.screen_rect(node.r0, node.pos, node.along, node.cross);
        let shape = self.g.nodes[i].shape;
        if let Shape::Dot(c) = shape {
            canvas.put(x, y, c);
            return;
        }
        let frame = shape.frame();
        canvas.frame(x, y, w, h, &frame);
        let content: usize = b
            .sections
            .iter()
            .map(|s| s.lines.len().max(1))
            .sum::<usize>()
            + b.sections.len().saturating_sub(1);
        let spare = (h - 2).saturating_sub(content);
        let mut row = y + 1 + if b.sections.len() == 1 { spare / 2 } else { 0 };
        let pad = shape.padding();
        for (k, section) in b.sections.iter().enumerate() {
            if k > 0 {
                canvas.put(x, row, if frame.left == '║' { '╟' } else { '├' });
                for cx in x + 1..x + w - 1 {
                    canvas.put(cx, row, '─');
                }
                canvas.put(x + w - 1, row, if frame.right == '║' { '╢' } else { '┤' });
                row += 1;
            }
            for line in &section.lines {
                match section.align {
                    Align::Center => canvas.text_centered(x + 1, w - 2, row, line),
                    Align::Left => {
                        canvas.text(x + 1 + pad, row, line);
                    }
                }
                row += 1;
            }
            if section.lines.is_empty() {
                row += 1;
            }
        }
    }

    /// Screen direction of the abstract along axis, toward higher ranks.
    fn along_dir(&self) -> u8 {
        match self.dir {
            Dir::TD => DOWN,
            Dir::BT => UP,
            Dir::LR => RIGHT,
            Dir::RL => LEFT,
        }
    }

    fn cross_dir(&self) -> u8 {
        if self.vertical() {
            RIGHT
        } else {
            DOWN
        }
    }

    fn draw_chain(&self, canvas: &mut Canvas, e: usize, chain: &Chain) {
        let edge = &self.g.edges[e];
        let segs: Vec<&Seg> = self.segs.iter().filter(|s| s.edge == e).collect();
        let first = segs[0];
        let last = segs[segs.len() - 1];
        let upper = &self.nodes[first.upper];
        let lower = &self.nodes[last.lower];
        let mut points = vec![(upper.r0 + upper.along - 1, first.pin_u)];
        for s in &segs {
            if let Some(t) = s.track {
                let ch = &self.channels[self.nodes[s.upper].rank];
                let row = ch.start + ch.top_pad + ch.lead + t;
                points.push((row, s.pin_u));
                points.push((row, s.pin_l));
            }
        }
        points.push((lower.r0, last.pin_l));
        let (mark_u, mark_l) = if chain.reversed {
            (edge.head, edge.tail)
        } else {
            (edge.tail, edge.head)
        };
        let n = points.len();
        if mark_u.is_some() {
            points[0].0 += 1;
        }
        if mark_l.is_some() {
            points[n - 1].0 -= 1;
        }
        let screen: Vec<(usize, usize)> = points
            .iter()
            .map(|&(r, c)| self.screen_point(r, c))
            .collect();
        for pair in screen.windows(2) {
            canvas.run(pair[0], pair[1], edge.stroke);
        }
        let toward_upper = flip(self.along_dir());
        if let Some(m) = mark_u {
            let (x, y) = screen[0];
            canvas.put(x, y, m.glyph(toward_upper));
        }
        if let Some(m) = mark_l {
            let (x, y) = screen[n - 1];
            canvas.put(x, y, m.glyph(self.along_dir()));
        }
    }

    fn draw_loops(&self, canvas: &mut Canvas, i: usize) {
        let Some(&first) = self.loops[i].first() else {
            return;
        };
        let edge = &self.g.edges[first];
        let node = &self.nodes[i];
        let edge_c = node.pos + node.cross - 1;
        let r1 = node.r0 + 1;
        let r2 = r1 + if self.vertical() { 1 } else { 2 };
        let out = [
            (r1, edge_c),
            (r1, edge_c + 2),
            (r2, edge_c + 2),
            (r2, edge_c),
        ];
        let mut pts: Vec<(usize, usize)> =
            out.iter().map(|&(r, c)| self.screen_point(r, c)).collect();
        if edge.tail.is_some() {
            pts[0] = self.screen_point(r1, edge_c + 1);
        }
        if edge.head.is_some() {
            pts[3] = self.screen_point(r2, edge_c + 1);
        }
        for pair in pts.windows(2) {
            canvas.run(pair[0], pair[1], edge.stroke);
        }
        let inward = flip(self.cross_dir());
        if let Some(m) = edge.tail {
            canvas.put(pts[0].0, pts[0].1, m.glyph(inward));
        }
        if let Some(m) = edge.head {
            canvas.put(pts[3].0, pts[3].1, m.glyph(inward));
        }
        let label = &self.loop_labels[i];
        if label.is_empty() {
            return;
        }
        let (lw, lh) = (text::max_width(label), label.len());
        let rect = if self.vertical() {
            self.screen_rect(r1, edge_c + 4, lh, lw)
        } else {
            self.screen_rect(r1, edge_c + 3, lw, lh)
        };
        draw_text_block(canvas, rect, label, Align::Left);
    }
}

fn flip(dir: u8) -> u8 {
    match dir {
        UP => DOWN,
        DOWN => UP,
        LEFT => RIGHT,
        _ => LEFT,
    }
}

fn draw_text_block(
    canvas: &mut Canvas,
    (x, y, w, h): (usize, usize, usize, usize),
    lines: &[String],
    align: Align,
) {
    let top = y + h.saturating_sub(lines.len()) / 2;
    for (k, line) in lines.iter().enumerate() {
        match align {
            Align::Center => canvas.text_centered(x, w, top + k, line),
            Align::Left => {
                canvas.text(x, top + k, line);
            }
        }
    }
}

/// Places `targets.len()` ordered ports in `lo..=hi`, at least two cells
/// apart. A port sits on its target when that is possible and near its
/// evenly spaced slot otherwise.
fn spread(targets: &[usize], lo: usize, hi: usize) -> Vec<usize> {
    let k = targets.len();
    if k == 0 {
        return Vec::new();
    }
    let cells = hi + 1 - lo;
    if cells < 2 * k - 1 {
        return (0..k).map(|i| (lo + i).min(hi)).collect();
    }
    const INF: usize = usize::MAX / 2;
    // Leaving the ideal slot only pays off when it makes the edge straight.
    const JOG: usize = 1000;
    // cost[i][j]: best total with port i at cell lo + j.
    let mut cost = vec![vec![INF; cells]; k];
    let mut from = vec![vec![0usize; cells]; k];
    for i in 0..k {
        let mut best = (INF, 0);
        for j in 0..cells {
            if i > 0 && j >= 2 && cost[i - 1][j - 2] < best.0 {
                best = (cost[i - 1][j - 2], j - 2);
            }
            let reach_left = j >= 2 * i;
            let reach_right = cells - j > 2 * (k - 1 - i);
            if !reach_left || !reach_right {
                continue;
            }
            let prev = if i == 0 { 0 } else { best.0 };
            if prev >= INF {
                continue;
            }
            let p = lo + j;
            let ideal = lo + (2 * i + 1) * cells / (2 * k);
            let miss = if p == targets[i] {
                0
            } else {
                JOG + p.abs_diff(ideal)
            };
            cost[i][j] = prev + miss;
            from[i][j] = best.1;
        }
    }
    let mut j = (0..cells).min_by_key(|&j| cost[k - 1][j]).unwrap();
    let mut out = vec![0; k];
    for i in (0..k).rev() {
        out[i] = lo + j;
        j = from[i][j];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::spread;

    #[test]
    fn spread_keeps_order_and_spacing() {
        assert_eq!(spread(&[5], 1, 9), vec![5]);
        assert_eq!(spread(&[0, 0, 0], 1, 9), vec![2, 5, 8]);
        assert_eq!(spread(&[3, 6], 1, 9), vec![3, 6]);
        assert_eq!(spread(&[3, 3], 1, 9), vec![3, 7]);
        assert_eq!(spread(&[20, 20], 1, 9), vec![3, 7]);
    }
}
