//! `pie` charts, drawn as horizontal bars.

use crate::text::{self, label_text, width};
use crate::{keyword, syntax, Document, Error};

const MAX_SLICES: usize = 100;
const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];

pub(crate) fn render(doc: &Document, max_width: usize) -> Result<String, Error> {
    let mut show_data = false;
    let mut title = doc.title.clone();
    let mut args = doc.args.as_str();
    if let Some(rest) = keyword(args, "showData") {
        show_data = true;
        args = rest;
    }
    if let Some(t) = keyword(args, "title") {
        title = Some(label_text(t));
    }
    let mut slices: Vec<(String, f64)> = Vec::new();
    for line in &doc.lines {
        if line.is_empty() || keyword(line, "accTitle").is_some() || line.starts_with("accDescr") {
            continue;
        }
        if let Some(t) = keyword(line, "title") {
            title = Some(label_text(t));
            continue;
        }
        let (label, value) = line.rsplit_once(':').ok_or_else(|| syntax(line))?;
        let value: f64 = value.trim().parse().map_err(|_| syntax(line))?;
        if !value.is_finite() || value < 0.0 {
            return Err(syntax(line));
        }
        if slices.len() >= MAX_SLICES {
            return Err(Error::TooLarge);
        }
        slices.push((label_text(label), value));
    }
    let total: f64 = slices.iter().map(|s| s.1).sum();
    let largest = slices.iter().map(|s| s.1).fold(0.0, f64::max);
    if slices.is_empty() || total <= 0.0 {
        return Err(Error::Syntax("pie chart has no data".into()));
    }
    let pcts: Vec<String> = slices
        .iter()
        .map(|(_, v)| format!("{:.1}%", v / total * 100.0))
        .collect();
    let data: Vec<String> = slices
        .iter()
        .map(|(_, v)| {
            if show_data {
                format!(" ({})", number(*v))
            } else {
                String::new()
            }
        })
        .collect();
    let label_w = slices.iter().map(|s| width(&s.0)).max().unwrap_or(0);
    let pct_w = pcts.iter().map(|v| width(v)).max().unwrap_or(0);
    let data_w = data.iter().map(|v| width(v)).max().unwrap_or(0);
    let value_w = pct_w + data_w;
    let bar_w = max_width.saturating_sub(label_w + value_w + 2).min(40);
    if bar_w < 8 {
        return Err(Error::TooWide);
    }
    let mut out = Vec::new();
    if let Some(t) = title.filter(|t| !t.is_empty()) {
        out.extend(text::wrap(&t, max_width));
        out.push(String::new());
    }
    for (((label, v), pct), data) in slices.iter().zip(&pcts).zip(&data) {
        let eighths = (v / largest * (bar_w * 8) as f64).round() as usize;
        let mut bar = "█".repeat(eighths / 8);
        if !eighths.is_multiple_of(8) {
            bar.push(EIGHTHS[eighths % 8]);
        }
        let bar = format!("{bar:<bar_w$}");
        let pad = " ".repeat(label_w - width(label));
        out.push(format!("{label}{pad} {bar} {pct:>pct_w$}{data:>data_w$}"));
    }
    Ok(out.join("\n"))
}

fn number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}
