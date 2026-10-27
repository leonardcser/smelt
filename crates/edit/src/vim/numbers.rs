use super::*;

/// What Ctrl-A (`a`) or Ctrl-X (`x`) adds with `count`.
pub(super) fn add_amount(key: char, count: usize) -> i64 {
    let count = i64::try_from(count).unwrap_or(i64::MAX);
    if key == 'x' {
        -count
    } else {
        count
    }
}

/// A number Ctrl-A and Ctrl-X change, as byte offsets in its line: decimal
/// with an optional `-`, hex after `0x`, or binary after `0b`.
pub(super) struct NumberToken {
    start: usize,
    digits: std::ops::Range<usize>,
    radix: u32,
}

/// The number under `col` in `line`, or the first one after it.
pub(super) fn number_at(line: &str, col: usize) -> Option<NumberToken> {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let prefixed = |marks: &[u8], radix: u32| {
            b[i] == b'0'
                && b.get(i + 1).is_some_and(|c| marks.contains(c))
                && b.get(i + 2).is_some_and(|c| char::from(*c).is_digit(radix))
        };
        let (radix, first) = if prefixed(b"xX", 16) {
            (16, i + 2)
        } else if prefixed(b"bB", 2) {
            (2, i + 2)
        } else {
            (10, i)
        };
        let mut end = first;
        while end < b.len() && char::from(b[end]).is_digit(radix) {
            end += 1;
        }
        if end > col {
            let negative = radix == 10 && i > 0 && b[i - 1] == b'-';
            return Some(NumberToken {
                start: if negative { i - 1 } else { i },
                digits: first..end,
                radix,
            });
        }
        i = end;
    }
    None
}

/// `number` in `line` with `amount` added. Decimal keeps a zero-padded width
/// and crosses zero; hex and binary keep their digit count and wrap as vim's
/// unsigned 64-bit numbers do, and hex keeps its letter case.
pub(super) fn added_number(line: &str, number: &NumberToken, amount: i64) -> Option<String> {
    let digits = &line[number.digits.clone()];
    let width = digits.len();
    if number.radix == 10 {
        let magnitude = digits.parse::<i128>().ok()?;
        let value = if number.start < number.digits.start {
            -magnitude
        } else {
            magnitude
        };
        let new = value + i128::from(amount);
        let width = if digits.starts_with('0') { width } else { 0 };
        let sign = if new < 0 { "-" } else { "" };
        return Some(format!("{sign}{:0width$}", new.unsigned_abs()));
    }
    let value = u64::from_str_radix(digits, number.radix).ok()?;
    let new = value.wrapping_add_signed(amount);
    let prefix = &line[number.start..number.digits.start];
    Some(if number.radix == 2 {
        format!("{prefix}{new:0width$b}")
    } else if digits.bytes().any(|c| c.is_ascii_uppercase()) {
        format!("{prefix}{new:0width$X}")
    } else {
        format!("{prefix}{new:0width$x}")
    })
}

/// Add `amount` to the number in the line `line_start` begins, looking from
/// `from` and taking only numbers that start before `limit`. Returns where
/// the new number ends.
pub(super) fn add_in_line(
    ctx: &mut VimContext<'_>,
    line_start: usize,
    from: usize,
    limit: usize,
    amount: i64,
) -> Option<usize> {
    let buf = ctx.buf.as_str();
    let line = &buf[line_start..line_end(buf, line_start)];
    let number = number_at(line, from - line_start)?;
    if line_start + number.start >= limit {
        return None;
    }
    let new = added_number(line, &number, amount)?;
    let (start, end) = (line_start + number.start, line_start + number.digits.end);
    ctx.replace_range(start, end, &new);
    Some(start + new.len())
}

/// Ctrl-A and Ctrl-X in Normal mode: add `amount` to the number under or
/// after the cursor on its line, and land on its last digit.
pub(super) fn add_to_number(ctx: &mut VimContext<'_>, amount: i64) {
    let buf = ctx.buf.as_str();
    let sol = line_start(buf, *ctx.cpos);
    let line = &buf[sol..line_end(buf, sol)];
    if number_at(line, *ctx.cpos - sol).is_none() {
        return;
    }
    ctx.save_undo();
    if let Some(end) = add_in_line(ctx, sol, *ctx.cpos, usize::MAX, amount) {
        *ctx.cpos = prev_grapheme_boundary(ctx.buf.as_str(), end);
    }
}

/// Visual Ctrl-A and Ctrl-X: add `amount` to the first number each selected
/// line holds within the selection, as one undo step, and return to its start.
pub(super) fn visual_add(ctx: &mut VimContext<'_>, amount: i64) -> Action {
    let range = visual_range(ctx.vim_state, ctx.buf.as_str(), *ctx.cpos, *ctx.mode);
    exit_visual(ctx);
    let Some((start, mut end)) = range else {
        return Action::Consumed;
    };
    ctx.save_undo();
    let mut sol = line_start(ctx.buf.as_str(), start);
    let mut from = start;
    loop {
        let before = ctx.buf.len();
        add_in_line(ctx, sol, from, end, amount);
        end = (end + ctx.buf.len()).saturating_sub(before);
        let eol = line_end(ctx.buf.as_str(), sol);
        if eol >= end || eol >= ctx.buf.len() {
            break;
        }
        sol = eol + 1;
        from = sol;
    }
    *ctx.cpos = start;
    clamp_normal(ctx.buf.as_str(), ctx.cpos);
    Action::Consumed
}
