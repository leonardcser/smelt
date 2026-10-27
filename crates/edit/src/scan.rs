//! Cursor scanning over a multi-line `&str`, ported from vim's `textobject.c`.
//!
//! The control flow of each routine follows the C source so edge cases (empty
//! lines, counts, extending a Visual area) behave the way vim does. A `Scan`
//! owns a cursor; the routines move it and report the area they selected
//! through `oap` (operator pending) or `vis` (Visual mode).

use super::text::{next_grapheme_boundary, prev_grapheme_boundary};

/// A line and a byte column. A column equal to the line's length sits on the
/// line's end, which vim calls the NUL.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Pos {
    pub line: usize,
    pub col: usize,
}

/// The Visual area, as far as text objects need it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Visual {
    pub start: Pos,
    pub linewise: bool,
}

/// What an operator takes when no Visual area is active.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Oap {
    pub start: Pos,
    pub inclusive: bool,
    pub linewise: bool,
}

pub(crate) struct Scan<'a> {
    buf: &'a str,
    starts: Vec<usize>,
    pub cur: Pos,
    pub vis: Option<Visual>,
    pub oap: Oap,
    /// `W`, `B` and `E` treat every non-blank run as one word.
    big: bool,
}

fn is_white(c: char) -> bool {
    matches!(c, ' ' | '\t')
}

impl<'a> Scan<'a> {
    pub fn new(buf: &'a str, cursor: usize) -> Self {
        let mut starts = vec![0];
        starts.extend(buf.match_indices('\n').map(|(i, _)| i + 1));
        let mut scan = Scan {
            buf,
            starts,
            cur: Pos::default(),
            vis: None,
            oap: Oap::default(),
            big: false,
        };
        scan.cur = scan.pos_at(smelt_buffer::text::snap_grapheme(buf, cursor));
        scan
    }

    pub fn pos_at(&self, offset: usize) -> Pos {
        let offset = offset.min(self.buf.len());
        let line = self.starts.partition_point(|&s| s <= offset) - 1;
        Pos {
            line,
            col: offset - self.starts[line],
        }
    }

    pub fn offset(&self, pos: Pos) -> usize {
        self.starts[pos.line] + pos.col
    }

    pub fn last_line(&self) -> usize {
        self.starts.len() - 1
    }

    fn line(&self, line: usize) -> &'a str {
        let start = self.starts[line];
        let end = self
            .starts
            .get(line + 1)
            .map_or(self.buf.len(), |&next| next - 1);
        &self.buf[start..end]
    }

    fn empty_line(&self, line: usize) -> bool {
        self.line(line).is_empty()
    }

    /// The character at `pos`, or `'\0'` on a line's end.
    fn gchar(&self, pos: Pos) -> char {
        self.line(pos.line)
            .get(pos.col..)
            .and_then(|rest| rest.chars().next())
            .unwrap_or('\0')
    }

    /// The character under the cursor, or NUL at a line's end.
    pub fn char_at_cursor(&self) -> char {
        self.gchar(self.cur)
    }

    /// After a forward word motion: the cursor may not rest on a line's end,
    /// so it steps back onto the last character and the motion turns inclusive.
    pub fn adjust_cursor(&mut self) -> bool {
        let stranded = self.cur.col > 0 && self.gchar(self.cur) == '\0';
        if stranded {
            let line = self.line(self.cur.line);
            self.cur.col = smelt_buffer::text::prev_grapheme_boundary(line, self.cur.col);
        }
        stranded
    }

    /// A line holding only blanks.
    fn linewhite(&self, line: usize) -> bool {
        self.line(line).trim_start_matches([' ', '\t']).is_empty()
    }

    /// Whether `line` starts a paragraph or section: an empty line or a form feed.
    fn start_ps(&self, line: usize) -> bool {
        let text = self.line(line);
        text.is_empty() || text.starts_with('\u{c}')
    }

    /// Whether the cursor is inside the line's leading blanks, `extra` columns
    /// to the right.
    fn inindent(&self, extra: usize) -> bool {
        let indent = self
            .line(self.cur.line)
            .bytes()
            .take_while(|b| matches!(b, b' ' | b'\t'))
            .count();
        indent >= self.cur.col + extra
    }

    // ── Stepping ─────────────────────────────────────────────────────

    /// Step forward a character. Returns 0 within a line, 2 when that lands on
    /// the line's end, 1 when it crosses to the next line and -1 at the end of
    /// the buffer.
    fn inc(&self, pos: &mut Pos) -> i32 {
        let line = self.line(pos.line);
        if pos.col < line.len() {
            pos.col = next_grapheme_boundary(line, pos.col);
            return if pos.col < line.len() { 0 } else { 2 };
        }
        if pos.line < self.last_line() {
            *pos = Pos {
                line: pos.line + 1,
                col: 0,
            };
            return 1;
        }
        -1
    }

    /// Step back a character. Returns 0 within a line, 1 when that crosses to
    /// the previous line's end and -1 at the start of the buffer.
    fn dec(&self, pos: &mut Pos) -> i32 {
        if pos.col > 0 {
            pos.col = prev_grapheme_boundary(self.line(pos.line), pos.col);
            return 0;
        }
        if pos.line > 0 {
            pos.line -= 1;
            pos.col = self.line(pos.line).len();
            return 1;
        }
        -1
    }

    /// `inc` that skips over a line's end.
    fn incl(&self, pos: &mut Pos) -> i32 {
        let mut r = self.inc(pos);
        if r >= 1 && pos.col > 0 {
            r = self.inc(pos);
        }
        r
    }

    /// `dec` that skips over a line's end.
    fn decl(&self, pos: &mut Pos) -> i32 {
        let mut r = self.dec(pos);
        if r == 1 && pos.col > 0 {
            r = self.dec(pos);
        }
        r
    }

    fn inc_cur(&mut self) -> i32 {
        let mut pos = self.cur;
        let r = self.inc(&mut pos);
        self.cur = pos;
        r
    }

    fn dec_cur(&mut self) -> i32 {
        let mut pos = self.cur;
        let r = self.dec(&mut pos);
        self.cur = pos;
        r
    }

    fn incl_cur(&mut self) -> i32 {
        let mut pos = self.cur;
        let r = self.incl(&mut pos);
        self.cur = pos;
        r
    }

    fn decl_cur(&mut self) -> i32 {
        let mut pos = self.cur;
        let r = self.decl(&mut pos);
        self.cur = pos;
        r
    }

    /// One character left within the line; false at its start.
    fn oneleft(&mut self) -> bool {
        if self.cur.col == 0 {
            return false;
        }
        self.cur.col = prev_grapheme_boundary(self.line(self.cur.line), self.cur.col);
        true
    }

    // ── Words ────────────────────────────────────────────────────────

    /// 0 for blanks, 1 for punctuation, 2 for word characters. With `big` every
    /// non-blank is 1.
    fn cls(&self, pos: Pos) -> u8 {
        match self.gchar(pos) {
            '\0' | ' ' | '\t' => 0,
            c if c.is_whitespace() => 0,
            _ if self.big => 1,
            c if c.is_alphanumeric() || c == '_' => 2,
            _ => 1,
        }
    }

    fn cls_cur(&self) -> u8 {
        self.cls(self.cur)
    }

    /// Skip a run of characters of one class; true when the buffer's end was hit.
    fn skip_chars(&mut self, class: u8, forward: bool) -> bool {
        while self.cls_cur() == class {
            let r = if forward {
                self.inc_cur()
            } else {
                self.dec_cur()
            };
            if r == -1 {
                return true;
            }
        }
        false
    }

    /// Go back to the start of the word or of the blanks under the cursor.
    fn back_in_line(&mut self) {
        let class = self.cls_cur();
        loop {
            if self.cur.col == 0 {
                break;
            }
            self.dec_cur();
            if self.cls_cur() != class {
                self.inc_cur();
                break;
            }
        }
    }

    /// `w`: false when the cursor was already at the end of the buffer. With
    /// `eol` the last word stops at the end of its line (for operators).
    pub fn fwd_word(&mut self, count: usize, big: bool, eol: bool) -> bool {
        self.big = big;
        for remaining in (0..count).rev() {
            let class = self.cls_cur();
            let last_line = self.cur.line == self.last_line();
            let i = self.inc_cur();
            if i == -1 || (i >= 1 && last_line) {
                return false;
            }
            if i >= 1 && eol && remaining == 0 {
                return true;
            }
            if class != 0 {
                while self.cls_cur() == class {
                    let i = self.inc_cur();
                    if i == -1 || (i >= 1 && eol && remaining == 0) {
                        return true;
                    }
                }
            }
            while self.cls_cur() == 0 {
                if self.cur.col == 0 && self.empty_line(self.cur.line) {
                    break;
                }
                let i = self.inc_cur();
                if i == -1 || (i >= 1 && eol && remaining == 0) {
                    return true;
                }
            }
        }
        true
    }

    /// `b`: false when the cursor started at the top of the buffer. With `stop`
    /// a cursor already on a word's start moves one word less.
    pub fn bck_word(&mut self, count: usize, big: bool, mut stop: bool) -> bool {
        self.big = big;
        for _ in 0..count {
            let class = self.cls_cur();
            if self.dec_cur() == -1 {
                return false;
            }
            let mut finished = false;
            if !stop || class == self.cls_cur() || class == 0 {
                while self.cls_cur() == 0 {
                    if self.cur.col == 0 && self.empty_line(self.cur.line) {
                        finished = true;
                        break;
                    }
                    if self.dec_cur() == -1 {
                        return true;
                    }
                }
                if !finished && self.skip_chars(self.cls_cur(), false) {
                    return true;
                }
            }
            if !finished {
                self.inc_cur();
            }
            stop = false;
        }
        true
    }

    /// `e`: false when the end of the buffer was reached. With `stop` a cursor
    /// already on a word's end moves one word less; with `empty` an empty line
    /// stops the motion.
    pub fn end_word(&mut self, count: usize, big: bool, mut stop: bool, empty: bool) -> bool {
        self.big = big;
        for _ in 0..count {
            let class = self.cls_cur();
            if self.inc_cur() == -1 {
                return false;
            }
            let mut finished = false;
            if self.cls_cur() == class && class != 0 {
                if self.skip_chars(class, true) {
                    return false;
                }
            } else if !stop || class == 0 {
                while self.cls_cur() == 0 {
                    if empty && self.cur.col == 0 && self.empty_line(self.cur.line) {
                        finished = true;
                        break;
                    }
                    if self.inc_cur() == -1 {
                        return false;
                    }
                }
                if !finished && self.skip_chars(self.cls_cur(), true) {
                    return false;
                }
            }
            if !finished {
                self.dec_cur();
            }
            stop = false;
        }
        true
    }

    /// `ge`: false when the cursor started at the top of the buffer. With
    /// `eol` the motion stops at the end of a line.
    pub fn bckend_word(&mut self, count: usize, big: bool, eol: bool) -> bool {
        self.big = big;
        for _ in 0..count {
            let class = self.cls_cur();
            let i = self.dec_cur();
            if i == -1 {
                return false;
            }
            if eol && i == 1 {
                return true;
            }
            if class != 0 {
                while self.cls_cur() == class {
                    let i = self.dec_cur();
                    if i == -1 || (eol && i == 1) {
                        return true;
                    }
                }
            }
            while self.cls_cur() == 0 {
                if self.cur.col == 0 && self.empty_line(self.cur.line) {
                    break;
                }
                let i = self.dec_cur();
                if i == -1 || (eol && i == 1) {
                    return true;
                }
            }
        }
        true
    }

    /// `iw`, `aw`, `iW`, `aW`; false when there is no word to select.
    pub fn current_word(&mut self, mut count: usize, include: bool, big: bool) -> bool {
        self.big = big;
        let mut inclusive = true;
        let mut include_white = false;
        let mut start_pos = Pos::default();

        if self.vis.is_none_or(|v| v.start == self.cur) {
            self.back_in_line();
            start_pos = self.cur;
            if (self.cls_cur() == 0) == include {
                if !self.end_word(1, big, true, true) {
                    return false;
                }
            } else {
                self.fwd_word(1, big, true);
                if self.cur.col == 0 {
                    self.decl_cur();
                } else {
                    self.oneleft();
                }
                include_white = include;
            }
            match &mut self.vis {
                Some(vis) => vis.start = start_pos,
                None => {
                    self.oap.start = start_pos;
                    self.oap.linewise = false;
                }
            }
            count -= 1;
        }

        while count > 0 {
            inclusive = true;
            match self.vis {
                Some(vis) if self.cur < vis.start => {
                    if self.decl_cur() == -1 {
                        return false;
                    }
                    if include != (self.cls_cur() != 0) {
                        if !self.bck_word(1, big, true) {
                            return false;
                        }
                    } else {
                        if !self.bckend_word(1, big, true) {
                            return false;
                        }
                        self.incl_cur();
                    }
                }
                _ => {
                    if self.incl_cur() == -1 {
                        return false;
                    }
                    if include != (self.cls_cur() == 0) {
                        if !self.fwd_word(1, big, true) && count > 1 {
                            return false;
                        }
                        // Just past a line break the first character is not wanted.
                        if !self.oneleft() {
                            inclusive = false;
                        }
                    } else if !self.end_word(1, big, true, true) {
                        return false;
                    }
                }
            }
            count -= 1;
        }

        if include_white && (self.cls_cur() != 0 || (self.cur.col == 0 && !inclusive)) {
            // Without blanks after the word, take the ones before it, except
            // an indent.
            let end = self.cur;
            self.cur = start_pos;
            if self.oneleft() {
                self.back_in_line();
                if self.cls_cur() == 0 && self.cur.col > 0 {
                    match &mut self.vis {
                        Some(vis) => vis.start = self.cur,
                        None => self.oap.start = self.cur,
                    }
                }
            }
            self.cur = end;
        }

        match &mut self.vis {
            Some(vis) => vis.linewise = false,
            None => self.oap.inclusive = inclusive,
        }
        true
    }

    /// `il`, `al`; false on a line of blanks.
    pub fn current_line(&mut self, include: bool) -> bool {
        if include {
            match &mut self.vis {
                Some(vis) => {
                    vis.start = Pos::default();
                    vis.linewise = true;
                }
                None => {
                    self.oap.start = Pos::default();
                    self.oap.linewise = true;
                }
            }
            self.cur = Pos {
                line: self.last_line(),
                col: 0,
            };
            return true;
        }
        let line = self.line(self.cur.line);
        let trimmed = line.trim_matches([' ', '\t']);
        if trimmed.is_empty() {
            return false;
        }
        let first = line.len() - line.trim_start_matches([' ', '\t']).len();
        let last = prev_grapheme_boundary(line, first + trimmed.len());
        let start = Pos {
            line: self.cur.line,
            col: first,
        };
        self.cur.col = last;
        match &mut self.vis {
            Some(vis) => {
                vis.start = start;
                vis.linewise = false;
            }
            None => {
                self.oap = Oap {
                    start,
                    inclusive: true,
                    linewise: false,
                };
            }
        }
        true
    }

    // ── Sentences ────────────────────────────────────────────────────

    fn find_first_blank(&self, pos: &mut Pos) {
        while self.decl(pos) != -1 {
            if !is_white(self.gchar(*pos)) {
                self.incl(pos);
                break;
            }
        }
    }

    /// Move to the start of the next (or previous) sentence; false when there
    /// is none.
    pub fn findsent(&mut self, forward: bool, mut count: usize) -> bool {
        let mut noskip = false;
        let mut pos = self.cur;
        let step = |scan: &Self, pos: &mut Pos| {
            if forward {
                scan.incl(pos)
            } else {
                scan.decl(pos)
            }
        };

        while count > 0 {
            count -= 1;
            let prev_pos = pos;
            let mut found = false;

            if self.gchar(pos) == '\0' {
                // On an empty line, skip to a non-empty one.
                loop {
                    if step(self, &mut pos) == -1 || self.gchar(pos) != '\0' {
                        break;
                    }
                }
                found = forward;
            } else if forward && pos.col == 0 && self.start_ps(pos.line) {
                if pos.line == self.last_line() {
                    return false;
                }
                pos.line += 1;
                found = true;
            } else if !forward {
                self.decl(&mut pos);
            }

            if !found {
                // Go back to the previous non-blank, non-punctuation character.
                let mut found_dot = false;
                loop {
                    let c = self.gchar(pos);
                    if !(is_white(c) || ".!?)]\"'".contains(c)) {
                        break;
                    }
                    let mut tpos = pos;
                    if self.decl(&mut tpos) == -1 || (self.empty_line(tpos.line) && forward) {
                        break;
                    }
                    if found_dot {
                        break;
                    }
                    if ".!?".contains(c) {
                        found_dot = true;
                    }
                    if ")]\"'".contains(c) && !".!?)]\"'".contains(self.gchar(tpos)) {
                        break;
                    }
                    self.decl(&mut pos);
                }

                let start_line = pos.line;
                loop {
                    let c = self.gchar(pos);
                    if c == '\0' || (pos.col == 0 && self.start_ps(pos.line)) {
                        if !forward && pos.line != start_line {
                            pos.line += 1;
                            pos.col = pos.col.min(self.line(pos.line).len());
                        }
                        break;
                    }
                    if matches!(c, '.' | '!' | '?') {
                        let mut tpos = pos;
                        let mut end = false;
                        let mut next;
                        loop {
                            if self.inc(&mut tpos) == -1 {
                                end = true;
                                next = '\0';
                                break;
                            }
                            next = self.gchar(tpos);
                            if !")]\"'".contains(next) {
                                break;
                            }
                        }
                        if end || is_white(next) || next == '\0' {
                            pos = tpos;
                            if self.gchar(pos) == '\0' {
                                self.inc(&mut pos);
                            }
                            break;
                        }
                    }
                    if step(self, &mut pos) == -1 {
                        if count > 0 {
                            return false;
                        }
                        noskip = true;
                        break;
                    }
                }
            }

            // Skip blanks.
            while !noskip && is_white(self.gchar(pos)) {
                if self.incl(&mut pos) == -1 {
                    break;
                }
            }

            if prev_pos == pos {
                // Did not move: advance a character and try again.
                if step(self, &mut pos) == -1 {
                    if count > 0 {
                        return false;
                    }
                    break;
                }
                count += 1;
            }
        }
        self.cur = pos;
        true
    }

    /// Skip `count / 2` sentences and as many runs of blanks.
    fn findsent_forward(&mut self, mut count: usize, mut at_start_sent: bool) {
        while count > 0 {
            count -= 1;
            self.findsent(true, 1);
            if at_start_sent {
                let mut pos = self.cur;
                self.find_first_blank(&mut pos);
                self.cur = pos;
            }
            if count == 0 || at_start_sent {
                self.decl_cur();
            }
            at_start_sent = !at_start_sent;
        }
    }

    /// `is`, `as`; extends the Visual area when it is bigger than a character.
    pub fn current_sent(&mut self, count: usize, include: bool) -> bool {
        let mut start_pos = self.cur;
        let mut pos = start_pos;
        self.findsent(true, 1);

        if self.vis.is_some_and(|v| start_pos != v.start) {
            return self.sent_extend(start_pos, pos, count, include);
        }

        // A cursor on blanks may be just before the next sentence's start.
        while is_white(self.gchar(pos)) {
            self.incl(&mut pos);
        }
        let start_blank = pos == self.cur;
        if start_blank {
            self.find_first_blank(&mut start_pos);
        } else {
            self.findsent(false, 1);
            start_pos = self.cur;
        }
        let ncount = if include {
            count * 2
        } else if start_blank {
            count - 1
        } else {
            count
        };
        if ncount > 0 {
            self.findsent_forward(ncount, true);
        } else {
            self.decl_cur();
        }

        if include {
            if start_blank {
                // Leave out the blanks after the sentence.
                let mut pos = self.cur;
                self.find_first_blank(&mut pos);
                self.cur = pos;
                if is_white(self.gchar(self.cur)) {
                    self.decl_cur();
                }
            } else if !is_white(self.gchar(self.cur)) {
                self.find_first_blank(&mut start_pos);
            }
        }

        match &mut self.vis {
            Some(vis) => {
                if start_pos == self.cur {
                    // Stuck on a single blank before a sentence: extend instead.
                    return self.sent_extend(start_pos, pos, count, include);
                }
                vis.start = start_pos;
                vis.linewise = false;
            }
            None => {
                // A line break after the sentence goes with it.
                self.oap.inclusive = self.incl_cur() == -1;
                self.oap.start = start_pos;
                self.oap.linewise = false;
            }
        }
        true
    }

    fn sent_extend(
        &mut self,
        start_pos: Pos,
        mut pos: Pos,
        mut count: usize,
        include: bool,
    ) -> bool {
        let vis_start = self.vis.map_or(start_pos, |v| v.start);
        if start_pos < vis_start {
            // Cursor at the start of the Visual area: in the blanks before a
            // sentence, in or after one, or at its start.
            let mut at_start_sent = true;
            self.decl(&mut pos);
            while pos < self.cur {
                if !is_white(self.gchar(pos)) {
                    at_start_sent = false;
                    break;
                }
                self.incl(&mut pos);
            }
            if !at_start_sent {
                self.findsent(false, 1);
                if self.cur == start_pos {
                    at_start_sent = true;
                } else {
                    self.findsent(true, 1);
                }
            }
            if include {
                count *= 2;
            }
            while count > 0 {
                count -= 1;
                if at_start_sent {
                    let mut p = self.cur;
                    self.find_first_blank(&mut p);
                    self.cur = p;
                }
                let c = self.gchar(self.cur);
                if !at_start_sent || (!include && !is_white(c)) {
                    self.findsent(false, 1);
                }
                at_start_sent = !at_start_sent;
            }
        } else {
            // Cursor at the end of the Visual area.
            self.incl(&mut pos);
            let mut at_start_sent = true;
            if pos != self.cur {
                at_start_sent = false;
                while pos < self.cur {
                    if !is_white(self.gchar(pos)) {
                        at_start_sent = true;
                        break;
                    }
                    self.incl(&mut pos);
                }
                if at_start_sent {
                    self.findsent(false, 1);
                } else {
                    self.cur = start_pos;
                }
            }
            if include {
                count *= 2;
            }
            self.findsent_forward(count, at_start_sent);
        }
        true
    }

    // ── Paragraphs ───────────────────────────────────────────────────

    /// `}` (or `{` when not `forward`): false when the count runs past the
    /// buffer's edge. A forward motion that ends on a non-empty last line
    /// lands on its last character and makes the motion inclusive.
    pub fn findpar(&mut self, forward: bool, count: usize) -> bool {
        let last = self.last_line();
        let mut curr = self.cur.line;
        for remaining in (0..count).rev() {
            let mut did_skip = false;
            let mut first = true;
            loop {
                if !self.line(curr).is_empty() {
                    did_skip = true;
                }
                if !first && did_skip && self.start_ps(curr) {
                    break;
                }
                first = false;
                let next = if forward {
                    (curr < last).then_some(curr + 1)
                } else {
                    curr.checked_sub(1)
                };
                match next {
                    Some(line) => curr = line,
                    None if remaining > 0 => return false,
                    None => break,
                }
            }
        }
        self.cur = Pos { line: curr, col: 0 };
        let text = self.line(curr);
        if curr == last && forward && !text.is_empty() {
            self.cur.col = smelt_buffer::text::prev_grapheme_boundary(text, text.len());
            self.oap.inclusive = true;
        }
        true
    }

    /// `ip`, `ap`; extends the Visual area when it spans lines.
    pub fn current_par(&mut self, count: usize, include: bool) -> bool {
        let start = self.cur.line;
        if self.vis.is_some_and(|v| start != v.start.line) {
            return self.par_extend(start, count, include);
        }
        let last = self.last_line() as isize;
        let white = |line: isize| self.linewhite(line as usize);
        let section = |line: isize| self.start_ps(line as usize);

        let mut start = start as isize;
        let white_in_front = white(start);
        while start > 0 {
            if white_in_front {
                if !white(start - 1) {
                    break;
                }
            } else if white(start - 1) || section(start) {
                break;
            }
            start -= 1;
        }

        let mut end = start;
        while end <= last && white(end) {
            end += 1;
        }
        end -= 1;
        let mut i = count as isize;
        if !include && white_in_front {
            i -= 1;
        }
        let mut do_white = false;
        while i > 0 {
            i -= 1;
            if end == last {
                return false;
            }
            if !include {
                do_white = white(end + 1);
            }
            if include || !do_white {
                end += 1;
                while end < last && !white(end + 1) && !section(end + 1) {
                    end += 1;
                }
            }
            if i == 0 && white_in_front && include {
                break;
            }
            if include || do_white {
                while end < last && white(end + 1) {
                    end += 1;
                }
            }
        }

        // Without blank lines after it, take the ones before.
        if !white_in_front && !white(end) && include {
            while start > 0 && white(start - 1) {
                start -= 1;
            }
        }

        if let Some(vis) = &mut self.vis {
            if vis.linewise && start as usize == self.cur.line {
                // Nothing new in a single blank line: extend instead.
                return self.par_extend(start as usize, count, include);
            }
            if vis.start.line != start as usize {
                vis.start = Pos {
                    line: start as usize,
                    col: 0,
                };
            }
            vis.linewise = true;
        } else {
            self.oap = Oap {
                start: Pos {
                    line: start as usize,
                    col: 0,
                },
                inclusive: false,
                linewise: true,
            };
        }
        self.cur = Pos {
            line: end as usize,
            col: 0,
        };
        true
    }

    fn par_extend(&mut self, start: usize, count: usize, include: bool) -> bool {
        let vis_line = self.vis.map_or(start, |v| v.start.line);
        let backward = start < vis_line;
        let edge = if backward { 0 } else { self.last_line() };
        let step = |line: usize| if backward { line - 1 } else { line + 1 };
        let mut start = start;
        let mut ok = true;
        for _ in 0..count {
            if start == edge {
                ok = false;
                break;
            }
            let mut prev_white: Option<bool> = None;
            for _ in 0..2 {
                start = step(start);
                let white = self.linewhite(start);
                if prev_white == Some(white) {
                    start = if backward { start + 1 } else { start - 1 };
                    break;
                }
                while start != edge {
                    let next = step(start);
                    let section_line = if backward { start } else { next };
                    if white != self.linewhite(next) || (!white && self.start_ps(section_line)) {
                        break;
                    }
                    start = next;
                }
                if !include || start == edge {
                    break;
                }
                prev_white = Some(white);
            }
        }
        self.cur = Pos {
            line: start,
            col: 0,
        };
        ok
    }

    // ── Blocks ───────────────────────────────────────────────────────

    /// The bracket matching `what` from `from`, ignoring quotes. An opening
    /// bracket searches backward for an unmatched one (or forward for the next
    /// one with `forward`); a closing bracket searches forward for its match.
    fn find_match(&self, from: Pos, what: char, forward: bool) -> Option<Pos> {
        let (open, close) = match what {
            '(' | ')' => ('(', ')'),
            '[' | ']' => ('[', ']'),
            '{' | '}' => ('{', '}'),
            '<' | '>' => ('<', '>'),
            _ => return None,
        };
        let (nesting, wanted, backwards) = if what == open {
            (close, open, !forward)
        } else {
            (open, close, false)
        };
        let mut pos = from;
        let mut depth = 0usize;
        loop {
            let line = self.line(pos.line);
            if backwards {
                if pos.col == 0 {
                    if pos.line == 0 {
                        return None;
                    }
                    pos.line -= 1;
                    pos.col = self.line(pos.line).len();
                } else {
                    pos.col = line[..pos.col]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(i, _)| i);
                }
            } else if pos.col >= line.len() {
                if pos.line == self.last_line() {
                    return None;
                }
                pos = Pos {
                    line: pos.line + 1,
                    col: 0,
                };
            } else {
                pos.col += self.gchar(pos).len_utf8();
            }
            let c = self.gchar(pos);
            if c != nesting && c != wanted {
                continue;
            }
            let escapes = self.line(pos.line).as_bytes()[..pos.col]
                .iter()
                .rev()
                .take_while(|&&b| b == b'\\')
                .count();
            if escapes % 2 != 0 {
                continue;
            }
            if c == nesting {
                depth += 1;
            } else if depth == 0 {
                return Some(pos);
            } else {
                depth -= 1;
            }
        }
    }

    /// `ib`, `ab`, `iB`, `aB`, `i[`, `a[`, `i<`, `a<`.
    pub fn current_block(
        &mut self,
        mut count: usize,
        include: bool,
        what: char,
        other: char,
    ) -> bool {
        let old_pos = self.cur;
        let mut old_end = self.cur;
        let mut old_start = old_end;
        let vis_active = self.vis.is_some();

        match self.vis {
            Some(vis) if vis.start != self.cur => {
                if vis.start < self.cur {
                    old_start = vis.start;
                    self.cur = vis.start;
                } else {
                    old_end = vis.start;
                }
            }
            _ => {
                if what == '{' {
                    while self.inindent(1) {
                        if self.inc_cur() != 0 {
                            break;
                        }
                    }
                }
                if self.gchar(self.cur) == what {
                    self.cur.col += 1;
                }
            }
        }

        let mut start_pos = Pos::default();
        let mut found = true;
        let forward = self.find_match(self.cur, what, false).is_none();
        while count > 0 {
            count -= 1;
            match self.find_match(self.cur, what, forward) {
                Some(pos) => {
                    self.cur = pos;
                    start_pos = pos;
                }
                None => {
                    found = false;
                    break;
                }
            }
        }
        let Some(mut end_pos) = found
            .then(|| self.find_match(self.cur, other, false))
            .flatten()
        else {
            self.cur = old_pos;
            return false;
        };
        self.cur = end_pos;

        // Leave out the brackets, and the indent before a closing bracket that
        // starts its line.
        let mut sol = false;
        if !include {
            loop {
                self.incl(&mut start_pos);
                sol = self.cur.col == 0;
                self.decl_cur();
                while self.inindent(1) {
                    sol = true;
                    if self.decl_cur() != 0 {
                        break;
                    }
                }

                if start_pos == end_pos && vis_active {
                    self.cur = old_pos;
                    return false;
                }

                // A Visual area no bigger than before grows to the next block.
                if start_pos >= old_start
                    && old_end >= self.cur
                    && start_pos != self.cur
                    && vis_active
                {
                    self.cur = old_start;
                    self.decl_cur();
                    let Some(start) = self.find_match(self.cur, what, false) else {
                        self.cur = old_pos;
                        return false;
                    };
                    start_pos = start;
                    self.cur = start;
                    let Some(end) = self.find_match(self.cur, other, false) else {
                        self.cur = old_pos;
                        return false;
                    };
                    end_pos = end;
                    self.cur = end;
                } else {
                    break;
                }
            }
        }

        match self.vis {
            Some(mut vis) => {
                if sol && self.gchar(self.cur) != '\0' {
                    self.inc_cur();
                }
                vis.start = start_pos;
                vis.linewise = false;
                self.vis = Some(vis);
            }
            None => {
                self.oap = Oap {
                    start: start_pos,
                    inclusive: false,
                    linewise: false,
                };
                if sol {
                    self.incl_cur();
                } else if start_pos <= self.cur {
                    self.oap.inclusive = true;
                } else {
                    // Nothing between the brackets: operate on nothing.
                    self.cur = start_pos;
                }
            }
        }
        true
    }

    // ── Quotes ───────────────────────────────────────────────────────

    /// `i"`, `a"` and the other quotes; they only work within one line.
    pub fn current_quote(&mut self, count: usize, include: bool, quote: char) -> bool {
        let line = self.line(self.cur.line);
        let bytes = line.as_bytes();
        let q = quote as u8;
        let at = |col: usize| bytes.get(col).copied().unwrap_or(0);
        let mut col_start = self.cur.col;
        let mut col_end: usize;
        let mut vis_empty = true;
        let mut vis_bef_curs = false;
        let mut inside_quotes = false;
        let mut selected_quote = false;

        if let Some(vis) = self.vis {
            if vis.start.line != self.cur.line {
                return false;
            }
            vis_bef_curs = vis.start < self.cur;
            vis_empty = vis.start == self.cur;
        }
        let vis_start_col = self.vis.map_or(0, |v| v.start.col);

        if !vis_empty {
            // Does the selection span exactly the text inside quotes?
            let (low, high) = if vis_bef_curs {
                (vis_start_col, self.cur.col)
            } else {
                (self.cur.col, vis_start_col)
            };
            inside_quotes = low > 0 && at(low - 1) == q && at(high) != 0 && at(high + 1) == q;
            let mut i = low;
            while i <= high {
                if at(i) == 0 {
                    break;
                }
                if at(i) == q {
                    selected_quote = true;
                    break;
                }
                i += 1;
            }
        }

        if !vis_empty && at(col_start) == q {
            // On a quote with something selected: take the next string.
            if vis_bef_curs {
                let Some(start) = find_next_quote(bytes, col_start + 1, q, false) else {
                    return false;
                };
                col_start = start;
                match find_next_quote(bytes, col_start + 1, q, true) {
                    Some(end) => col_end = end,
                    None => {
                        col_end = col_start;
                        col_start = self.cur.col;
                    }
                }
            } else {
                col_end = find_prev_quote(bytes, col_start, q, false);
                if at(col_end) != q {
                    return false;
                }
                col_start = find_prev_quote(bytes, col_end, q, true);
                if at(col_start) != q {
                    col_start = col_end;
                    col_end = self.cur.col;
                }
            }
        } else if at(col_start) == q || !vis_empty {
            let mut first_col = col_start;
            if !vis_empty {
                first_col = if vis_bef_curs {
                    match find_next_quote(bytes, col_start, q, false) {
                        Some(col) => col,
                        None => return false,
                    }
                } else {
                    find_prev_quote(bytes, col_start, q, false)
                };
            }
            // Whether this quote opens or closes depends on the pairs from the
            // line's start.
            col_start = 0;
            loop {
                let Some(start) = find_next_quote(bytes, col_start, q, false) else {
                    return false;
                };
                if start > first_col {
                    return false;
                }
                col_start = start;
                let Some(end) = find_next_quote(bytes, col_start + 1, q, true) else {
                    return false;
                };
                col_end = end;
                if col_start <= first_col && first_col <= col_end {
                    break;
                }
                col_start = col_end + 1;
            }
        } else {
            col_start = find_prev_quote(bytes, col_start, q, true);
            if at(col_start) != q {
                // No quote before the cursor: look after it.
                let Some(start) = find_next_quote(bytes, col_start, q, false) else {
                    return false;
                };
                col_start = start;
            }
            let Some(end) = find_next_quote(bytes, col_start + 1, q, true) else {
                return false;
            };
            col_end = end;
        }

        if include {
            if is_white(at(col_end + 1) as char) {
                while is_white(at(col_end + 1) as char) {
                    col_end += 1;
                }
            } else {
                while col_start > 0 && is_white(at(col_start - 1) as char) {
                    col_start -= 1;
                }
            }
        }

        // After `vi"` another `i"` must include the quotes; so does `v2i"`.
        if !include && count < 2 && (vis_empty || !inside_quotes) {
            col_start += 1;
        }
        let line_no = self.cur.line;
        self.cur.col = col_start;
        match &mut self.vis {
            Some(vis) => {
                let starts_outside =
                    at(vis_start_col) != q && (vis_start_col == 0 || at(vis_start_col - 1) != q);
                if vis_empty
                    || (vis_bef_curs && !selected_quote && (inside_quotes || starts_outside))
                {
                    vis.start = self.cur;
                }
            }
            None => {
                self.oap.start = self.cur;
                self.oap.linewise = false;
            }
        }

        self.cur.col = col_end;
        let mut inclusive = false;
        if (include || count > 1 || (!vis_empty && inside_quotes)) && self.inc_cur() == 2 {
            inclusive = true;
        }
        match self.vis {
            Some(mut vis) => {
                if vis_empty || vis_bef_curs {
                    self.dec_cur();
                } else {
                    // Cursor at the start of the Visual area.
                    let starts_outside = at(vis_start_col) != q
                        && (at(vis_start_col) == 0 || at(vis_start_col + 1) != q);
                    if inside_quotes || (!selected_quote && starts_outside) {
                        self.dec_cur();
                        vis.start = self.cur;
                    }
                    self.cur = Pos {
                        line: line_no,
                        col: col_start,
                    };
                }
                vis.linewise = false;
                self.vis = Some(vis);
            }
            None => self.oap.inclusive = inclusive,
        }
        true
    }

    // ── Tags ─────────────────────────────────────────────────────────

    /// Whether the cursor is on an end tag `</aaa>` (`end_tag`), or on a start
    /// tag `<aaa>` that does not close itself.
    fn in_html_tag(&self, end_tag: bool) -> bool {
        let line = self.line(self.cur.line);
        let bytes = line.as_bytes();
        let mut p = self.cur.col.min(line.len());
        while p > 0 {
            if bytes.get(p) == Some(&b'<') {
                break;
            }
            p = prev_grapheme_boundary(line, p);
            if bytes[p] == b'>' {
                break;
            }
        }
        if bytes.get(p) != Some(&b'<') {
            return false;
        }
        let slash = bytes.get(p + 1) == Some(&b'/');
        if end_tag || slash {
            return end_tag && slash;
        }
        let mut pos = Pos {
            line: self.cur.line,
            col: p,
        };
        let mut last = '\0';
        loop {
            if self.inc(&mut pos) < 0 {
                return false;
            }
            match self.gchar(pos) {
                '>' => return last != '/',
                c => last = c,
            }
        }
    }

    /// The `<` of the nearest start tag before `before` that no end tag after
    /// it closes, as vim's `searchpair()` finds it.
    fn unclosed_start_tag(&self, before: usize) -> Option<usize> {
        let mut depth = 0usize;
        for (at, _) in self.buf[..before].rmatch_indices('<') {
            if end_tag_at(self.buf, at, None) {
                depth += 1;
            } else if start_tag_at(self.buf, at, None) {
                if depth == 0 {
                    return Some(at);
                }
                depth -= 1;
            }
        }
        None
    }

    /// The `<` of the end tag closing a `name` tag whose name starts at `after`.
    fn matching_end_tag(&self, after: usize, name: &str) -> Option<usize> {
        let mut depth = 0usize;
        for (at, _) in self.buf[after..].match_indices('<') {
            let at = after + at;
            if end_tag_at(self.buf, at, Some(name)) {
                if depth == 0 {
                    return Some(at);
                }
                depth -= 1;
            } else if start_tag_at(self.buf, at, Some(name)) {
                depth += 1;
            }
        }
        None
    }

    /// `it`, `at`.
    pub fn current_tagblock(&mut self, count_arg: usize, include: bool) -> bool {
        let mut count = count_arg;
        let mut do_include = include;
        let mut is_inclusive = true;
        let old_pos = self.cur;
        let mut old_end = self.cur;
        let mut old_start = old_end;
        if self.vis.is_none() {
            self.decl(&mut old_end);
        }

        // Starting on a tag selects that tag's block.
        match self.vis {
            Some(vis) if vis.start != self.cur => {
                if vis.start < self.cur {
                    old_start = vis.start;
                    self.cur = vis.start;
                } else {
                    old_end = vis.start;
                }
            }
            _ => {
                while self.inindent(1) {
                    if self.inc_cur() != 0 {
                        break;
                    }
                }
                if self.in_html_tag(false) {
                    while self.gchar(self.cur) != '>' {
                        if self.inc_cur() < 0 {
                            break;
                        }
                    }
                } else if self.in_html_tag(true) {
                    while self.gchar(self.cur) != '<' {
                        if self.dec_cur() < 0 {
                            break;
                        }
                    }
                    self.dec_cur();
                    old_end = self.cur;
                }
            }
        }

        loop {
            for _ in 0..count {
                match self.unclosed_start_tag(self.offset(self.cur)) {
                    Some(at) => self.cur = self.pos_at(at),
                    None => {
                        self.cur = old_pos;
                        return false;
                    }
                }
            }
            let tag_pos = self.cur;

            self.inc_cur();
            let from = self.offset(self.cur);
            let len = self.buf[from..]
                .find(|c: char| c == '>' || c == '\n' || is_white(c))
                .unwrap_or(self.buf.len() - from);
            if len == 0 {
                self.cur = old_pos;
                return false;
            }
            let end = self.matching_end_tag(from, &self.buf[from..from + len]);
            match end.map(|at| self.pos_at(at)) {
                Some(end) if end >= old_end => self.cur = end,
                // An open tag without its end tag: try the one around it.
                _ => {
                    count = 1;
                    self.cur = tag_pos;
                    continue;
                }
            }

            if do_include {
                while self.gchar(self.cur) != '>' {
                    if self.inc_cur() < 0 {
                        break;
                    }
                }
            } else if self.gchar(self.cur) == '<' {
                // An end tag starting its line keeps the line break, made
                // exclusive instead of stepping back over it.
                if self.vis.is_none() && self.cur.col == 0 {
                    is_inclusive = false;
                } else {
                    self.dec_cur();
                }
            }
            let end_pos = self.cur;

            let mut start_pos = tag_pos;
            if !do_include {
                // Leave out the start tag, past a `>` in quotes.
                let mut in_quotes = false;
                self.cur = tag_pos;
                while self.inc_cur() >= 0 {
                    match self.gchar(self.cur) {
                        '>' if !in_quotes => {
                            self.inc_cur();
                            start_pos = self.cur;
                            break;
                        }
                        '"' | '\'' => in_quotes = !in_quotes,
                        _ => {}
                    }
                }
                self.cur = end_pos;
                // The same text as the Visual area: take the tags too.
                if self.vis.is_some() && start_pos == old_start && end_pos == old_end {
                    do_include = true;
                    self.cur = old_start;
                    count = count_arg;
                    continue;
                }
            }

            // An end before the start leaves nothing between the tags.
            let empty = end_pos < start_pos;
            match &mut self.vis {
                Some(vis) => {
                    if empty {
                        self.cur = start_pos;
                    }
                    vis.start = start_pos;
                    vis.linewise = false;
                }
                None => {
                    self.oap = Oap {
                        start: start_pos,
                        inclusive: is_inclusive && !empty,
                        linewise: false,
                    };
                    if empty {
                        self.cur = start_pos;
                    }
                }
            }
            return true;
        }
    }
}

/// Whether a start tag that does not close itself begins at `at`. Without a
/// `name` any tag counts, as `<[^ \t>/!]\+\%(\_s\_[^>]\{-}[^/]>\|$\|\_s\=>\)`
/// matches; with one only that tag, ignoring case, as
/// `<name\>\%(\_s\_[^>]\{-}\_[^/]>\|\_s\?>\)` does.
fn start_tag_at(buf: &str, at: usize, name: Option<&str>) -> bool {
    let bytes = buf.as_bytes();
    let from = at + 1;
    let after = match name {
        Some(name) => {
            let end = from + name.len();
            match buf.get(from..end) {
                Some(found) if found.eq_ignore_ascii_case(name) => end,
                _ => return false,
            }
        }
        None => {
            from + buf[from..]
                .find([' ', '\t', '>', '/', '!', '\n'])
                .unwrap_or(buf.len() - from)
        }
    };
    if after == from {
        return false;
    }
    let space = |i: usize| matches!(bytes.get(i), Some(b' ' | b'\t' | b'\n'));
    match bytes.get(after) {
        Some(b'>') => return true,
        None | Some(b'\n') if name.is_none() => return true,
        _ if !space(after) => return false,
        _ => {}
    }
    if bytes.get(after + 1) == Some(&b'>') {
        return true;
    }
    // White space, then up to the first `>` with no `/` right before it.
    let mut i = after + 1;
    while let Some(&b) = bytes.get(i) {
        if bytes.get(i + 1) == Some(&b'>') && b != b'/' && (b != b'\n' || name.is_some()) {
            return true;
        }
        if b == b'>' {
            return false;
        }
        i += 1;
    }
    false
}

/// Whether an end tag begins at `at`: any, as `</[^>]*>` matches, or only
/// `</name>`, ignoring case.
fn end_tag_at(buf: &str, at: usize, name: Option<&str>) -> bool {
    let Some(rest) = buf.get(at..).and_then(|rest| rest.strip_prefix("</")) else {
        return false;
    };
    match name {
        Some(name) => rest
            .get(..name.len() + 1)
            .is_some_and(|tag| tag.ends_with('>') && tag[..name.len()].eq_ignore_ascii_case(name)),
        None => rest
            .find(['>', '\n'])
            .is_some_and(|end| rest.as_bytes()[end] == b'>'),
    }
}

/// The next unescaped `quote` at or after `col`. With `escape` a backslash
/// hides the character after it.
fn find_next_quote(line: &[u8], mut col: usize, quote: u8, escape: bool) -> Option<usize> {
    loop {
        let c = *line.get(col)?;
        if escape && c == b'\\' {
            col += 1;
            line.get(col)?;
        } else if c == quote {
            return Some(col);
        }
        col += 1;
    }
}

/// The last `quote` before `col`, or 0 when there is none. With `escape` a
/// quote after an odd run of backslashes does not count.
fn find_prev_quote(line: &[u8], mut col: usize, quote: u8, escape: bool) -> usize {
    while col > 0 {
        col -= 1;
        let mut n = 0;
        if escape {
            while col > n && line[col - n - 1] == b'\\' {
                n += 1;
            }
        }
        if n % 2 == 1 {
            col -= n;
        } else if line[col] == quote {
            break;
        }
    }
    col
}
