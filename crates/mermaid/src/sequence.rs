//! `sequenceDiagram`.

use std::collections::HashMap;

use crate::canvas::{Canvas, Stroke, ROUNDED, SQUARE};
use crate::text::{self, label_lines, width};
use crate::{keyword, syntax, Document, Error};

const MAX_PARTICIPANTS: usize = 40;
const MAX_EVENTS: usize = 600;

struct Participant {
    label: Vec<String>,
    actor: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Head {
    Arrow,
    None,
    Cross,
    Async,
}

#[derive(Clone, Copy)]
enum Place {
    Left(usize),
    Right(usize),
    Over(usize, usize),
}

enum Event {
    Message {
        from: usize,
        to: usize,
        text: Vec<String>,
        stroke: Stroke,
        head: Head,
        both: bool,
        activate: bool,
        deactivate: bool,
    },
    Note {
        place: Place,
        text: Vec<String>,
    },
    Start {
        kind: String,
        label: String,
    },
    Else {
        kind: String,
        label: String,
    },
    End,
    Activate(usize),
    Deactivate(usize),
}

struct Diagram {
    participants: Vec<Participant>,
    events: Vec<Event>,
    index: HashMap<String, usize>,
    title: Option<String>,
}

impl Diagram {
    fn participant(&mut self, id: &str) -> Result<usize, Error> {
        let id = id.trim();
        if id.is_empty() {
            return Err(Error::Syntax("missing participant".into()));
        }
        if let Some(&i) = self.index.get(id) {
            return Ok(i);
        }
        if self.participants.len() >= MAX_PARTICIPANTS {
            return Err(Error::TooLarge);
        }
        self.participants.push(Participant {
            label: label_lines(id),
            actor: false,
        });
        self.index
            .insert(id.to_string(), self.participants.len() - 1);
        Ok(self.participants.len() - 1)
    }

    fn push(&mut self, event: Event) -> Result<(), Error> {
        if self.events.len() >= MAX_EVENTS {
            return Err(Error::TooLarge);
        }
        self.events.push(event);
        Ok(())
    }
}

const ARROWS: &[(&str, Stroke, Head, bool)] = &[
    ("<<-->>", Stroke::Dotted, Head::Arrow, true),
    ("<<->>", Stroke::Solid, Head::Arrow, true),
    ("-->>", Stroke::Dotted, Head::Arrow, false),
    ("->>", Stroke::Solid, Head::Arrow, false),
    ("--x", Stroke::Dotted, Head::Cross, false),
    ("-x", Stroke::Solid, Head::Cross, false),
    ("--)", Stroke::Dotted, Head::Async, false),
    ("-)", Stroke::Solid, Head::Async, false),
    ("-->", Stroke::Dotted, Head::None, false),
    ("->", Stroke::Solid, Head::None, false),
];

const BLOCKS: &[&str] = &["loop", "alt", "opt", "par", "critical", "break", "rect"];
const BRANCHES: &[&str] = &["else", "and", "option"];
const IGNORED: &[&str] = &[
    "destroy",
    "links",
    "link",
    "properties",
    "details",
    "accTitle",
    "accDescr",
];

fn parse(doc: &Document) -> Result<Diagram, Error> {
    let mut d = Diagram {
        participants: Vec::new(),
        events: Vec::new(),
        index: HashMap::new(),
        title: doc.title.clone(),
    };
    let mut autonumber = None;
    // `true` for blocks that draw a frame, `false` for `box` groups.
    let mut open: Vec<bool> = Vec::new();
    for line in doc.lines.iter().map(|l| l.trim_end_matches(';')) {
        if line.is_empty() {
            continue;
        }
        let line = line.strip_prefix("create ").map_or(line, str::trim_start);
        let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        match word {
            "participant" | "actor" => {
                let rest = rest.split('@').next().unwrap_or(rest);
                let (id, alias) = match rest.split_once(" as ") {
                    Some((id, alias)) => (id.trim(), Some(alias)),
                    None => (rest.trim(), None),
                };
                let i = d.participant(id)?;
                if let Some(alias) = alias {
                    d.participants[i].label = label_lines(alias);
                }
                d.participants[i].actor = word == "actor";
            }
            "autonumber" => autonumber = Some(rest.parse::<usize>().unwrap_or(1)),
            "title" => d.title = Some(text::label_text(rest)),
            "activate" => {
                let i = d.participant(rest)?;
                d.push(Event::Activate(i))?;
            }
            "deactivate" => {
                let i = d.participant(rest)?;
                d.push(Event::Deactivate(i))?;
            }
            "box" => open.push(false),
            "end" => match open.pop() {
                Some(true) => d.push(Event::End)?,
                Some(false) => {}
                None => return Err(syntax(line)),
            },
            w if w.eq_ignore_ascii_case("note") => {
                let note = parse_note(&mut d, rest).ok_or_else(|| syntax(line))??;
                d.push(note)?;
            }
            w if BLOCKS.contains(&w) => {
                open.push(true);
                let label = if w == "rect" {
                    String::new()
                } else {
                    text::label_text(rest)
                };
                d.push(Event::Start {
                    kind: w.to_string(),
                    label,
                })?;
            }
            w if BRANCHES.contains(&w) => {
                if open.last() != Some(&true) {
                    return Err(syntax(line));
                }
                d.push(Event::Else {
                    kind: w.to_string(),
                    label: text::label_text(rest),
                })?;
            }
            w if IGNORED.contains(&w) => {}
            _ if line.starts_with("title:") => d.title = Some(text::label_text(&line[6..])),
            _ => {
                let mut event = parse_message(&mut d, line).ok_or_else(|| syntax(line))??;
                if let (Some(n), Event::Message { text, .. }) = (autonumber.as_mut(), &mut event) {
                    match text.first_mut() {
                        Some(first) => *first = format!("{n}. {first}"),
                        None => text.push(format!("{n}.")),
                    }
                    *n += 1;
                }
                d.push(event)?;
            }
        }
    }
    if !open.is_empty() {
        return Err(Error::Syntax("unclosed block".into()));
    }
    if d.participants.is_empty() {
        return Err(Error::Syntax("no participants".into()));
    }
    Ok(d)
}

fn parse_note(d: &mut Diagram, rest: &str) -> Option<Result<Event, Error>> {
    let (target, body) = rest.split_once(':')?;
    let target = target.trim();
    let place = if let Some(p) = keyword(target, "left of") {
        d.participant(p).map(Place::Left)
    } else if let Some(p) = keyword(target, "right of") {
        d.participant(p).map(Place::Right)
    } else if let Some(p) = keyword(target, "over") {
        match p.split_once(',') {
            Some((a, b)) => d.participant(a).and_then(|a| {
                let b = d.participant(b)?;
                Ok(Place::Over(a.min(b), a.max(b)))
            }),
            None => d.participant(p).map(|a| Place::Over(a, a)),
        }
    } else {
        return None;
    };
    Some(place.map(|place| Event::Note {
        place,
        text: label_lines(body),
    }))
}

fn parse_message(d: &mut Diagram, line: &str) -> Option<Result<Event, Error>> {
    let (head_part, body) = match line.split_once(':') {
        Some((h, b)) => (h, b),
        None => (line, ""),
    };
    let (at, token, stroke, head, both) = ARROWS
        .iter()
        .filter_map(|&(tok, stroke, head, both)| {
            head_part.find(tok).map(|at| (at, tok, stroke, head, both))
        })
        .min_by_key(|&(at, tok, ..)| (at, usize::MAX - tok.len()))?;
    let from = head_part[..at].trim();
    let mut to = head_part[at + token.len()..].trim();
    let (mut activate, mut deactivate) = (false, false);
    if let Some(t) = to.strip_prefix('+') {
        activate = true;
        to = t.trim_start();
    } else if let Some(t) = to.strip_prefix('-') {
        deactivate = true;
        to = t.trim_start();
    }
    if from.is_empty() || to.is_empty() {
        return None;
    }
    let from = match d.participant(from) {
        Ok(i) => i,
        Err(e) => return Some(Err(e)),
    };
    let to = match d.participant(to) {
        Ok(i) => i,
        Err(e) => return Some(Err(e)),
    };
    Some(Ok(Event::Message {
        from,
        to,
        text: label_lines(body),
        stroke,
        head,
        both,
        activate,
        deactivate,
    }))
}

pub(crate) fn render(doc: &Document, max_width: usize) -> Result<String, Error> {
    let d = parse(doc)?;
    for (wrap, name_wrap) in [(48, 24), (28, 16), (16, 10), (10, 6)] {
        let body = Layout::new(&d, wrap, name_wrap).draw();
        if body.lines().all(|l| width(l) <= max_width) {
            return Ok(match &d.title {
                Some(t) => format!("{}\n\n{body}", text::wrap(t, max_width).join("\n")),
                None => body,
            });
        }
    }
    Err(Error::TooWide)
}

struct Layout<'d> {
    d: &'d Diagram,
    labels: Vec<Vec<String>>,
    texts: Vec<Vec<String>>,
    x: Vec<i64>,
    box_w: Vec<usize>,
    header_h: usize,
}

impl<'d> Layout<'d> {
    fn new(d: &'d Diagram, wrap: usize, name_wrap: usize) -> Self {
        let labels: Vec<Vec<String>> = d
            .participants
            .iter()
            .map(|p| text::wrap_lines(&p.label, name_wrap))
            .collect();
        let texts: Vec<Vec<String>> = d
            .events
            .iter()
            .map(|e| match e {
                Event::Message { text, .. } | Event::Note { text, .. } => {
                    text::wrap_lines(text, wrap)
                }
                _ => Vec::new(),
            })
            .collect();
        let box_w: Vec<usize> = labels.iter().map(|l| text::max_width(l) + 4).collect();
        let header_h = labels.iter().map(Vec::len).max().unwrap_or(1) + 2;
        let mut lay = Layout {
            d,
            labels,
            texts,
            x: Vec::new(),
            box_w,
            header_h,
        };
        lay.x = lay.solve();
        lay
    }

    /// Lifeline columns that satisfy every spacing constraint, found by
    /// widening gaps for short spans first.
    fn solve(&self) -> Vec<i64> {
        let n = self.d.participants.len();
        let mut cons: Vec<(usize, usize, i64)> = Vec::new();
        for i in 1..n {
            let (a, b) = (self.box_w[i - 1], self.box_w[i]);
            cons.push((i - 1, i, (a - a / 2 + b / 2 + 2) as i64));
        }
        let mut depth = 0usize;
        for (k, e) in self.d.events.iter().enumerate() {
            let tw = text::max_width(&self.texts[k]) as i64;
            match *e {
                Event::Message { from, to, .. } if from == to => {
                    let room = (tw + 3).max(6) + 2 * depth as i64;
                    cons.extend((from + 1 < n).then_some((from, from + 1, room)));
                }
                Event::Message { from, to, .. } => {
                    cons.push((from.min(to), from.max(to), (tw + 4).max(8)));
                }
                Event::Note { place, .. } => match place {
                    Place::Right(a) if a + 1 < n => cons.push((a, a + 1, tw + 8)),
                    Place::Left(a) if a > 0 => cons.push((a - 1, a, tw + 8)),
                    Place::Over(a, b) if a == b => {
                        let half = (tw + 4) / 2 + 2;
                        if a > 0 {
                            cons.push((a - 1, a, half + 1));
                        }
                        if a + 1 < n {
                            cons.push((a, a + 1, half + 1));
                        }
                    }
                    Place::Over(a, b) => cons.push((a, b, tw - 1)),
                    _ => {}
                },
                Event::Start { .. } => depth += 1,
                Event::End => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        cons.sort_by_key(|&(a, b, _)| b - a);
        let mut gaps = vec![0i64; n.saturating_sub(1)];
        for (a, b, dist) in cons {
            let have: i64 = gaps[a..b].iter().sum();
            if have >= dist {
                continue;
            }
            let span = (b - a) as i64;
            let deficit = dist - have;
            for (k, g) in gaps[a..b].iter_mut().enumerate() {
                *g += deficit / span + i64::from((k as i64) < deficit % span);
            }
        }
        let mut x = vec![(self.box_w[0] / 2) as i64];
        for g in gaps {
            x.push(x[x.len() - 1] + g);
        }
        x
    }

    /// Top row of each event and the total height.
    fn rows(&self) -> (Vec<usize>, usize) {
        let mut y = self.header_h + 1;
        let mut rows = Vec::with_capacity(self.d.events.len());
        for (k, e) in self.d.events.iter().enumerate() {
            rows.push(y);
            let lines = self.texts[k].len();
            y += match *e {
                Event::Message { from, to, .. } if from == to => lines + 3,
                Event::Message { .. } => lines + 2,
                Event::Note { .. } => lines + 3,
                Event::Start { .. } | Event::Else { .. } => 1,
                Event::End => 2,
                Event::Activate(_) | Event::Deactivate(_) => 0,
            };
        }
        (rows, y)
    }

    /// Horizontal extent of an event's drawing.
    fn extent(&self, k: usize) -> Option<(i64, i64)> {
        let tw = text::max_width(&self.texts[k]) as i64;
        match self.d.events[k] {
            Event::Message { from, to, .. } if from == to => {
                Some((self.x[from], self.x[from] + (tw + 2).max(4)))
            }
            Event::Message { from, to, .. } => Some((self.x[from.min(to)], self.x[from.max(to)])),
            Event::Note { place, .. } => Some(self.note_rect(place, tw)),
            _ => None,
        }
    }

    fn note_rect(&self, place: Place, tw: i64) -> (i64, i64) {
        let w = tw + 4;
        match place {
            Place::Right(a) => (self.x[a] + 2, self.x[a] + 1 + w),
            Place::Left(a) => (self.x[a] - 1 - w, self.x[a] - 2),
            Place::Over(a, b) if a == b => (self.x[a] - w / 2, self.x[a] - w / 2 + w - 1),
            Place::Over(a, b) => {
                let (l, r) = (self.x[a] - 2, self.x[b] + 2);
                let extra = (w - (r - l + 1)).max(0);
                (l - extra / 2, r + extra - extra / 2)
            }
        }
    }

    /// Block frames, outermost first.
    fn frames(&self) -> Vec<Frame> {
        let mut out: Vec<Frame> = Vec::new();
        let mut stack: Vec<OpenFrame> = Vec::new();
        for (k, e) in self.d.events.iter().enumerate() {
            match e {
                Event::Start { .. } => {
                    let frame = Frame {
                        start: k,
                        end: k,
                        left: 0,
                        right: 0,
                        seps: Vec::new(),
                    };
                    stack.push(OpenFrame {
                        frame,
                        extent: None,
                        slot: out.len(),
                    });
                }
                Event::Else { .. } => {
                    if let Some(open) = stack.last_mut() {
                        open.frame.seps.push(k);
                    }
                }
                Event::End => {
                    let Some(OpenFrame {
                        mut frame,
                        extent,
                        slot,
                    }) = stack.pop()
                    else {
                        continue;
                    };
                    let (lo, hi) = extent.unwrap_or((self.x[0], self.x[self.x.len() - 1]));
                    let label_w = std::iter::once(frame.start)
                        .chain(frame.seps.iter().copied())
                        .filter_map(|e| self.frame_label(e))
                        .map(|l| width(&l) as i64)
                        .max()
                        .unwrap_or(0);
                    frame.end = k;
                    frame.left = lo - 3;
                    frame.right = (hi + 3).max(frame.left + label_w + 3);
                    if let Some(parent) = stack.last_mut() {
                        parent.extent = Some(merge(parent.extent, (frame.left, frame.right)));
                    }
                    out.insert(slot, frame);
                }
                _ => {
                    if let (Some(ext), Some(open)) = (self.extent(k), stack.last_mut()) {
                        open.extent = Some(merge(open.extent, ext));
                    }
                }
            }
        }
        out
    }

    fn frame_label(&self, k: usize) -> Option<String> {
        match &self.d.events[k] {
            Event::Start { kind, label } | Event::Else { kind, label } => {
                Some(if label.is_empty() {
                    kind.clone()
                } else {
                    format!("{kind} [{label}]")
                })
            }
            _ => None,
        }
        .filter(|l| l != "rect")
    }

    fn draw(&self) -> String {
        let (rows, height) = self.rows();
        let frames = self.frames();
        let n = self.x.len();
        // Horizontal bounds of everything drawn, to shift into view.
        let mut lo = 0i64;
        let mut hi = self.x[n - 1] + (self.box_w[n - 1] - self.box_w[n - 1] / 2) as i64;
        for i in 0..n {
            lo = lo.min(self.x[i] - (self.box_w[i] / 2) as i64);
        }
        for k in 0..self.d.events.len() {
            if let Some((l, r)) = self.extent(k) {
                lo = lo.min(l);
                hi = hi.max(r);
            }
        }
        for frame in &frames {
            lo = lo.min(frame.left);
            hi = hi.max(frame.right);
        }
        let shift = -lo;
        let col = |x: i64| (x + shift) as usize;
        let mut c = Canvas::new((hi - lo + 1) as usize, height);
        for i in 0..n {
            c.run(
                (col(self.x[i]), self.header_h),
                (col(self.x[i]), height - 1),
                Stroke::Solid,
            );
        }
        self.draw_activations(&mut c, &rows, height, &col);
        for frame in &frames {
            let (start, end, seps) = (frame.start, frame.end, &frame.seps);
            let (l, r) = (frame.left, frame.right);
            let (top, bottom) = (rows[start], rows[end]);
            let (l, r) = (col(l), col(r));
            c.run((l, top), (r, top), Stroke::Solid);
            c.run((l, bottom), (r, bottom), Stroke::Solid);
            c.run((l, top), (l, bottom), Stroke::Solid);
            c.run((r, top), (r, bottom), Stroke::Solid);
            if let Some(label) = self.frame_label(start) {
                c.text(l + 2, top, &label);
            }
            for &s in seps {
                let y = rows[s];
                c.run((l, y), (r, y), Stroke::Dotted);
                if let Some(label) = self.frame_label(s) {
                    c.text(l + 2, y, &label);
                }
            }
        }
        for (k, e) in self.d.events.iter().enumerate() {
            let y = rows[k];
            let lines = &self.texts[k];
            match *e {
                Event::Message {
                    from,
                    to,
                    stroke,
                    head,
                    ..
                } if from == to => {
                    let x = col(self.x[from]);
                    for (i, line) in lines.iter().enumerate() {
                        c.text(x + 2, y + i, line);
                    }
                    let a = y + lines.len();
                    c.run((x, a), (x + 3, a), stroke);
                    c.run((x + 3, a), (x + 3, a + 1), stroke);
                    c.run((x + 3, a + 1), (x + 1, a + 1), stroke);
                    if let Some(g) = head_glyph(head, false) {
                        c.put(x + 1, a + 1, g);
                    }
                }
                Event::Message {
                    from,
                    to,
                    stroke,
                    head,
                    both,
                    ..
                } => {
                    let (xf, xt) = (col(self.x[from]), col(self.x[to]));
                    let right = xt > xf;
                    for (i, line) in lines.iter().enumerate() {
                        c.text(xf.min(xt) + 2, y + i, line);
                    }
                    let a = y + lines.len();
                    let (start, end) = if right { (xf, xt - 1) } else { (xf, xt + 1) };
                    let start = if both {
                        if right {
                            start + 1
                        } else {
                            start - 1
                        }
                    } else {
                        start
                    };
                    c.run((start, a), (end, a), stroke);
                    if let Some(g) = head_glyph(head, right) {
                        c.put(end, a, g);
                    }
                    if let Some(g) = head_glyph(head, !right).filter(|_| both) {
                        c.put(start, a, g);
                    }
                }
                Event::Note { place, .. } => {
                    let tw = text::max_width(lines) as i64;
                    let (l, r) = self.note_rect(place, tw);
                    let (l, r) = (col(l), col(r));
                    let h = lines.len() + 2;
                    c.clear_rect(l, y, r - l + 1, h);
                    c.frame(l, y, r - l + 1, h, &SQUARE);
                    for (i, line) in lines.iter().enumerate() {
                        c.text(l + 2, y + 1 + i, line);
                    }
                }
                _ => {}
            }
        }
        for i in 0..n {
            let w = self.box_w[i];
            let x = col(self.x[i]) - w / 2;
            let frame = if self.d.participants[i].actor {
                &ROUNDED
            } else {
                &SQUARE
            };
            c.frame(x, 0, w, self.header_h, frame);
            let label = &self.labels[i];
            let top = 1 + (self.header_h - 2 - label.len()) / 2;
            for (k, line) in label.iter().enumerate() {
                c.text_centered(x + 1, w - 2, top + k, line);
            }
            c.run(
                (col(self.x[i]), self.header_h - 1),
                (col(self.x[i]), self.header_h),
                Stroke::Solid,
            );
        }
        c.render()
    }

    fn draw_activations(
        &self,
        c: &mut Canvas,
        rows: &[usize],
        height: usize,
        col: &dyn Fn(i64) -> usize,
    ) {
        let n = self.x.len();
        let mut open: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut spans: Vec<(usize, usize, usize)> = Vec::new();
        for (k, e) in self.d.events.iter().enumerate() {
            let arrow = rows[k] + self.texts[k].len();
            match *e {
                Event::Activate(p) => open[p].push(arrow),
                Event::Deactivate(p) => {
                    if let Some(s) = open[p].pop() {
                        spans.push((p, s, arrow));
                    }
                }
                Event::Message {
                    from,
                    to,
                    activate,
                    deactivate,
                    ..
                } => {
                    if activate {
                        open[to].push(arrow);
                    }
                    if deactivate {
                        if let Some(s) = open[from].pop() {
                            spans.push((from, s, arrow));
                        }
                    }
                }
                _ => {}
            }
        }
        for (p, starts) in open.into_iter().enumerate() {
            for s in starts {
                spans.push((p, s, height - 1));
            }
        }
        for (p, s, e) in spans {
            let x = col(self.x[p]);
            c.run((x, s), (x, e.max(s)), Stroke::Thick);
        }
    }
}

struct Frame {
    start: usize,
    end: usize,
    left: i64,
    right: i64,
    /// `else`, `and` and `option` events inside the frame.
    seps: Vec<usize>,
}

/// A frame whose `end` has not been reached yet.
struct OpenFrame {
    frame: Frame,
    /// Horizontal span of everything inside so far.
    extent: Option<(i64, i64)>,
    /// Index in the output that keeps outer frames before inner ones.
    slot: usize,
}

fn merge(a: Option<(i64, i64)>, b: (i64, i64)) -> (i64, i64) {
    match a {
        Some((l, r)) => (l.min(b.0), r.max(b.1)),
        None => b,
    }
}

fn head_glyph(head: Head, right: bool) -> Option<char> {
    match (head, right) {
        (Head::Arrow, true) => Some('▶'),
        (Head::Arrow, false) => Some('◀'),
        (Head::Async, true) => Some('▷'),
        (Head::Async, false) => Some('◁'),
        (Head::Cross, _) => Some('×'),
        (Head::None, _) => None,
    }
}
