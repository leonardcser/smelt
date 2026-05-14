use serde::{Deserialize, Serialize};
use smelt_core::config;
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

/// Format a USD cost for display.
pub(crate) fn format_cost(usd: f64) -> String {
    if usd < 0.01 {
        format!("${:.4}", usd)
    } else if usd < 1.0 {
        format!("${:.3}", usd)
    } else {
        format!("${:.2}", usd)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MetricsEntry {
    pub(crate) timestamp_ms: u64,
    pub(crate) prompt_tokens: u32,
    pub(crate) completion_tokens: u32,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) cost_usd: Option<f64>,
    #[serde(default)]
    pub(crate) cache_read_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_write_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) reasoning_tokens: Option<u32>,
}

fn metrics_path() -> PathBuf {
    config::state_dir().join("metrics.jsonl")
}

/// Append one entry to the metrics JSONL file.
pub(crate) fn append(entry: &MetricsEntry) {
    let path = metrics_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    if let Ok(line) = serde_json::to_string(entry) {
        let _ = writeln!(f, "{line}");
    }
}

/// Load all entries from the metrics file.
pub(crate) fn load() -> Vec<MetricsEntry> {
    let path = metrics_path();
    let Ok(f) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    std::io::BufReader::new(f)
        .lines()
        .filter_map(|line| {
            let line = line.ok()?;
            serde_json::from_str(&line).ok()
        })
        .collect()
}

// ── Aggregation ─────────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    smelt_core::session::now_ms()
}

fn day_key(ms: u64) -> u64 {
    ms / (24 * 3600 * 1000)
}

fn hour_key(ms: u64) -> u64 {
    ms / (3600 * 1000)
}

struct ModelStats {
    prompt: u64,
    completion: u64,
    calls: usize,
    cost_usd: f64,
}

impl ModelStats {
    fn total(&self) -> u64 {
        self.prompt + self.completion
    }
}

struct Stats {
    total_calls: usize,
    total_prompt: u64,
    total_completion: u64,
    total_cost_usd: f64,
    by_model: BTreeMap<String, ModelStats>,
    by_day: BTreeMap<u64, u64>,
    by_hour: BTreeMap<u64, u64>,
}

fn aggregate(entries: &[MetricsEntry]) -> Stats {
    let mut stats = Stats {
        total_calls: entries.len(),
        total_prompt: 0,
        total_completion: 0,
        total_cost_usd: 0.0,
        by_model: BTreeMap::new(),
        by_day: BTreeMap::new(),
        by_hour: BTreeMap::new(),
    };

    let h24_ago = now_ms().saturating_sub(24 * 3600 * 1000);

    for e in entries {
        let prompt = e.prompt_tokens as u64;
        let completion = e.completion_tokens as u64;
        let total = prompt + completion;
        let cost = e.cost_usd.unwrap_or(0.0);

        stats.total_prompt += prompt;
        stats.total_completion += completion;
        stats.total_cost_usd += cost;

        let m = stats.by_model.entry(e.model.clone()).or_insert(ModelStats {
            prompt: 0,
            completion: 0,
            calls: 0,
            cost_usd: 0.0,
        });
        m.prompt += prompt;
        m.completion += completion;
        m.calls += 1;
        m.cost_usd += cost;

        *stats.by_day.entry(day_key(e.timestamp_ms)).or_insert(0) += total;

        if e.timestamp_ms >= h24_ago {
            *stats.by_hour.entry(hour_key(e.timestamp_ms)).or_insert(0) += total;
        }
    }

    stats
}

// ── Structured output for the renderer ──────────────────────────────────────

pub(crate) enum StatsLine {
    /// Dim label + normal value.
    Kv { label: String, value: String },
    /// Section heading (dim).
    Heading(String),
    /// Sparkline bar characters (rendered in accent).
    SparklineBars(String),
    /// Sparkline legend (rendered dim).
    SparklineLegend(String),
    /// One row of the daily heatmap.
    HeatRow { label: String, cells: Vec<HeatCell> },
    /// Empty separator line.
    Blank,
}

#[derive(Clone, Copy)]
pub(crate) enum HeatCell {
    Empty,
    /// Intensity 0..=3 (maps to increasing brightness).
    Level(u8),
}

const SPARKLINE: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn sparkline(values: &[u64]) -> String {
    let max = values.iter().copied().max().unwrap_or(1).max(1);
    values
        .iter()
        .map(|&v| {
            let idx = ((v as f64 / max as f64) * (SPARKLINE.len() - 1) as f64).round() as usize;
            SPARKLINE[idx.min(SPARKLINE.len() - 1)]
        })
        .collect()
}

pub(crate) struct StatsOutput {
    pub(crate) left: Vec<StatsLine>,
    pub(crate) right: Vec<StatsLine>,
}

pub(crate) fn render_stats(entries: &[MetricsEntry]) -> StatsOutput {
    if entries.is_empty() {
        return StatsOutput {
            left: vec![StatsLine::Heading("No metrics recorded yet.".into())],
            right: vec![],
        };
    }

    let stats = aggregate(entries);
    let mut left = Vec::new();
    let mut right = Vec::new();
    let total = stats.total_prompt + stats.total_completion;

    if stats.total_cost_usd > 0.0 {
        left.push(StatsLine::Kv {
            label: "total cost".into(),
            value: format_cost(stats.total_cost_usd),
        });
    }
    left.push(StatsLine::Kv {
        label: "calls".into(),
        value: stats.total_calls.to_string(),
    });
    left.push(StatsLine::Kv {
        label: "tokens".into(),
        value: format!(
            "{} ({} prompt + {} completion)",
            fmt(total),
            fmt(stats.total_prompt),
            fmt(stats.total_completion),
        ),
    });
    if stats.total_calls > 0 {
        left.push(StatsLine::Kv {
            label: "avg/call".into(),
            value: format!("{} tokens", fmt(total / stats.total_calls as u64)),
        });
    }

    if stats.by_model.len() > 1 {
        left.push(StatsLine::Blank);
        left.push(StatsLine::Heading("per model".into()));
        let mut models: Vec<_> = stats.by_model.iter().collect();
        models.sort_by_key(|b| std::cmp::Reverse(b.1.total()));
        let max_model_len = models.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        let max_calls_len = models
            .iter()
            .map(|(_, m)| m.calls.to_string().len())
            .max()
            .unwrap_or(0);
        let max_tokens_len = models
            .iter()
            .map(|(_, m)| fmt(m.total()).len())
            .max()
            .unwrap_or(0);
        let show_cost = models.iter().any(|(_, m)| m.cost_usd > 0.0);
        for (model, m) in &models {
            let model_pad = max_model_len.saturating_sub(model.len()) + 2;
            let calls_str = m.calls.to_string();
            let tokens_str = fmt(m.total());
            let calls_pad = max_calls_len.saturating_sub(calls_str.len());
            let tokens_pad = max_tokens_len.saturating_sub(tokens_str.len());
            let cost_str = if show_cost {
                format!("    {}", format_cost(m.cost_usd))
            } else {
                String::new()
            };
            left.push(StatsLine::Kv {
                label: format!("  {model}{}", " ".repeat(model_pad)),
                value: format!(
                    "{}{calls_str}    {}{tokens_str}{cost_str}",
                    " ".repeat(calls_pad),
                    " ".repeat(tokens_pad),
                ),
            });
        }
    }

    if !stats.by_hour.is_empty() {
        right.push(StatsLine::Heading("last 24 hours".into()));
        let now_hour = hour_key(now_ms());
        let values: Vec<u64> = (0..24)
            .map(|i| {
                let h = now_hour - 23 + i;
                stats.by_hour.get(&h).copied().unwrap_or(0)
            })
            .collect();
        right.push(StatsLine::SparklineBars(sparkline(&values)));
        right.push(StatsLine::SparklineLegend(
            "24h ago ─────────────── now".into(),
        ));
    }

    if !stats.by_day.is_empty() {
        right.push(StatsLine::Blank);
        right.push(StatsLine::Heading("daily activity (12 weeks)".into()));

        let today = day_key(now_ms());
        let days: Vec<u64> = (0..84).map(|i| today - 83 + i).collect();
        let values: Vec<u64> = days
            .iter()
            .map(|d| stats.by_day.get(d).copied().unwrap_or(0))
            .collect();
        let max = values.iter().copied().max().unwrap_or(1).max(1);

        let day_labels = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];
        for (row, label) in day_labels.iter().enumerate() {
            let mut cells = Vec::new();
            for week in 0..12 {
                let idx = week * 7 + row;
                if idx < values.len() {
                    let v = values[idx];
                    if v == 0 {
                        cells.push(HeatCell::Empty);
                    } else {
                        let level = ((v as f64 / max as f64) * 3.0).round() as u8;
                        cells.push(HeatCell::Level(level.min(3)));
                    }
                }
            }
            right.push(StatsLine::HeatRow {
                label: label.to_string(),
                cells,
            });
        }
    }

    StatsOutput { left, right }
}

const KV_GAP: usize = 2;

fn label_col_width(lines: &[StatsLine]) -> usize {
    lines
        .iter()
        .filter_map(|l| match l {
            StatsLine::Kv { label, .. } => Some(label.len()),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        + KV_GAP
}

fn stats_line_visual_width(line: &StatsLine, label_col: usize) -> usize {
    match line {
        StatsLine::Kv { label, value } => {
            let col = label_col.max(label.len() + KV_GAP);
            col + value.len()
        }
        StatsLine::Heading(text) | StatsLine::SparklineLegend(text) => text.len(),
        StatsLine::SparklineBars(bars) => bars.chars().count(),
        StatsLine::HeatRow { label, cells } => label.len() + 1 + cells.len() * 2,
        StatsLine::Blank => 0,
    }
}

/// Flatten one `StatsLine` to a plain string. Used by the `/stats` and
/// `/cost` Lua plugins which render through `smelt.ui.dialog.open` and
/// need a textual representation rather than the structured variants.
fn stats_line_to_text(line: &StatsLine, label_col: usize) -> String {
    match line {
        StatsLine::Kv { label, value } => {
            let pad = label_col.saturating_sub(label.len());
            format!("{label}{}{value}", " ".repeat(pad))
        }
        StatsLine::Heading(text) => text.clone(),
        StatsLine::SparklineBars(bars) => bars.clone(),
        StatsLine::SparklineLegend(text) => text.clone(),
        StatsLine::HeatRow { label, cells } => {
            let mut out = String::new();
            out.push_str(label);
            out.push(' ');
            for cell in cells {
                out.push_str(match cell {
                    HeatCell::Empty => "·",
                    HeatCell::Level(0) => "░",
                    HeatCell::Level(1) => "▒",
                    HeatCell::Level(2) => "▓",
                    HeatCell::Level(_) => "█",
                });
                out.push(' ');
            }
            out
        }
        StatsLine::Blank => String::new(),
    }
}

/// Render full `/stats` output as a single string. Two-column layout
/// joined row-by-row when both columns are present; falls back to
/// sequential left → blank → right.
pub(crate) fn render_stats_text(out: &StatsOutput) -> String {
    let left_col = label_col_width(&out.left);
    let right_col = label_col_width(&out.right);
    if out.right.is_empty() {
        return out
            .left
            .iter()
            .map(|l| stats_line_to_text(l, left_col))
            .collect::<Vec<_>>()
            .join("\n");
    }

    let left_visual = out
        .left
        .iter()
        .map(|l| stats_line_visual_width(l, left_col))
        .max()
        .unwrap_or(0);
    let term_width = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80);
    let right_visual = out
        .right
        .iter()
        .map(|l| stats_line_visual_width(l, right_col))
        .max()
        .unwrap_or(0);
    let gap = 5;

    if left_visual + gap + right_visual + 2 <= term_width {
        // Side-by-side.
        let rows = out.left.len().max(out.right.len());
        (0..rows)
            .map(|i| {
                let l_text = out
                    .left
                    .get(i)
                    .map(|l| stats_line_to_text(l, left_col))
                    .unwrap_or_default();
                let r_text = out
                    .right
                    .get(i)
                    .map(|l| stats_line_to_text(l, right_col))
                    .unwrap_or_default();
                let pad = (left_visual + gap).saturating_sub(l_text.chars().count());
                format!("{l_text}{}{r_text}", " ".repeat(pad))
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        // Sequential.
        let mut rows: Vec<String> = out
            .left
            .iter()
            .map(|l| stats_line_to_text(l, left_col))
            .collect();
        rows.push(String::new());
        rows.extend(out.right.iter().map(|l| stats_line_to_text(l, right_col)));
        rows.join("\n")
    }
}

/// Render `/cost` output (single column) as a plain string.
pub(crate) fn render_cost_text(lines: &[StatsLine]) -> String {
    let col = label_col_width(lines);
    lines
        .iter()
        .map(|l| stats_line_to_text(l, col))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn render_session_cost(
    cost_usd: f64,
    model: &str,
    turns: usize,
    resolved: &engine::pricing::ResolvedPricing,
) -> Vec<StatsLine> {
    let mut lines = Vec::new();
    let pricing = &resolved.pricing;

    lines.push(StatsLine::Heading("session".into()));
    lines.push(StatsLine::Kv {
        label: "cost".into(),
        value: if cost_usd > 0.0 {
            format_cost(cost_usd)
        } else {
            "$0".into()
        },
    });
    lines.push(StatsLine::Kv {
        label: "model".into(),
        value: model.to_string(),
    });
    lines.push(StatsLine::Kv {
        label: "turns".into(),
        value: turns.to_string(),
    });
    lines.push(StatsLine::Blank);

    let fmt_rate = |rate: f64| -> String {
        if rate == 0.0 {
            return "—".into();
        }
        format_cost(rate)
    };

    lines.push(StatsLine::Heading("pricing (per 1M tokens)".into()));
    lines.push(StatsLine::Kv {
        label: "source".into(),
        value: resolved.source.label().to_string(),
    });
    if !pricing.is_zero() {
        lines.push(StatsLine::Kv {
            label: "input".into(),
            value: fmt_rate(pricing.input),
        });
        lines.push(StatsLine::Kv {
            label: "output".into(),
            value: fmt_rate(pricing.output),
        });
        if pricing.cache_read > 0.0 {
            lines.push(StatsLine::Kv {
                label: "cache read".into(),
                value: fmt_rate(pricing.cache_read),
            });
        }
        if pricing.cache_write > 0.0 {
            lines.push(StatsLine::Kv {
                label: "cache write".into(),
                value: fmt_rate(pricing.cache_write),
            });
        }
    }
    lines
}

fn fmt(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ts_ms: u64, model: &str, prompt: u32, completion: u32, cost: f64) -> MetricsEntry {
        MetricsEntry {
            timestamp_ms: ts_ms,
            prompt_tokens: prompt,
            completion_tokens: completion,
            model: model.to_string(),
            cost_usd: Some(cost),
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
        }
    }

    // ── format_cost ──────────────────────────────────────────────────────

    #[test]
    fn format_cost_uses_four_decimals_below_one_cent() {
        // < $0.01 → 4 decimals to keep tiny costs legible.
        assert_eq!(format_cost(0.0001), "$0.0001");
        assert_eq!(format_cost(0.0099), "$0.0099");
    }

    #[test]
    fn format_cost_uses_three_decimals_below_one_dollar() {
        // $0.01..$1.00 → 3 decimals.
        assert_eq!(format_cost(0.01), "$0.010");
        assert_eq!(format_cost(0.999), "$0.999");
    }

    #[test]
    fn format_cost_uses_two_decimals_at_or_above_one_dollar() {
        // ≥ $1.00 → 2 decimals.
        assert_eq!(format_cost(1.0), "$1.00");
        assert_eq!(format_cost(123.456), "$123.46");
    }

    // ── fmt (token counts) ───────────────────────────────────────────────

    #[test]
    fn fmt_keeps_small_numbers_as_plain_decimal() {
        assert_eq!(fmt(0), "0");
        assert_eq!(fmt(42), "42");
        assert_eq!(fmt(999), "999");
    }

    #[test]
    fn fmt_uses_k_suffix_for_thousands() {
        assert_eq!(fmt(1_000), "1.0k");
        assert_eq!(fmt(15_500), "15.5k");
        assert_eq!(fmt(999_999), "1000.0k");
    }

    #[test]
    fn fmt_uses_capital_m_suffix_for_millions() {
        assert_eq!(fmt(1_000_000), "1.0M");
        assert_eq!(fmt(2_500_000), "2.5M");
    }

    // ── day_key / hour_key ───────────────────────────────────────────────

    #[test]
    fn day_key_is_constant_within_a_calendar_day() {
        let day_start = 24 * 3600 * 1000u64;
        assert_eq!(day_key(day_start), day_key(day_start + 12 * 3600 * 1000));
    }

    #[test]
    fn day_key_increments_across_midnight() {
        let day_start = 24 * 3600 * 1000u64;
        assert_eq!(
            day_key(day_start) + 1,
            day_key(day_start + 24 * 3600 * 1000)
        );
    }

    #[test]
    fn hour_key_is_constant_within_an_hour_and_increments_at_the_boundary() {
        let h = 5 * 3600 * 1000u64;
        assert_eq!(hour_key(h), hour_key(h + 59 * 60 * 1000));
        assert_eq!(hour_key(h) + 1, hour_key(h + 3600 * 1000));
    }

    // ── sparkline ────────────────────────────────────────────────────────

    #[test]
    fn sparkline_produces_one_char_per_input_value() {
        let s = sparkline(&[0, 1, 2, 3]);
        assert_eq!(s.chars().count(), 4);
    }

    #[test]
    fn sparkline_maps_zero_to_blank_space() {
        let s = sparkline(&[0]);
        assert_eq!(s, " ");
    }

    #[test]
    fn sparkline_maps_max_value_to_full_block() {
        let s = sparkline(&[5, 5]);
        assert_eq!(s, "██");
    }

    #[test]
    fn sparkline_scales_intermediate_values_between_blank_and_full() {
        let s = sparkline(&[0, 1, 2, 3, 4]);
        // First char is space, last is full block; middle chars are intermediate ramps.
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(chars[0], ' ');
        assert_eq!(chars[chars.len() - 1], '█');
    }

    #[test]
    fn sparkline_handles_empty_input() {
        assert_eq!(sparkline(&[]), "");
    }

    // ── aggregate (deterministic parts only) ─────────────────────────────

    #[test]
    fn aggregate_sums_totals_across_all_entries() {
        let entries = vec![entry(0, "m1", 10, 20, 0.01), entry(0, "m2", 5, 15, 0.02)];
        let s = aggregate(&entries);
        assert_eq!(s.total_calls, 2);
        assert_eq!(s.total_prompt, 15);
        assert_eq!(s.total_completion, 35);
        assert!((s.total_cost_usd - 0.03).abs() < 1e-9);
    }

    #[test]
    fn aggregate_buckets_by_model() {
        let entries = vec![
            entry(0, "m1", 10, 20, 0.01),
            entry(0, "m1", 5, 5, 0.0),
            entry(0, "m2", 2, 3, 0.0),
        ];
        let s = aggregate(&entries);
        assert_eq!(s.by_model.len(), 2);
        let m1 = s.by_model.get("m1").unwrap();
        assert_eq!(m1.calls, 2);
        assert_eq!(m1.total(), 40);
        let m2 = s.by_model.get("m2").unwrap();
        assert_eq!(m2.calls, 1);
        assert_eq!(m2.total(), 5);
    }

    #[test]
    fn aggregate_buckets_total_tokens_by_calendar_day() {
        // Two entries on day 100, one on day 101.
        let d100 = 100 * 24 * 3600 * 1000u64;
        let d101 = 101 * 24 * 3600 * 1000u64;
        let entries = vec![
            entry(d100, "m", 10, 10, 0.0),
            entry(d100 + 5 * 3600 * 1000, "m", 1, 1, 0.0),
            entry(d101, "m", 100, 0, 0.0),
        ];
        let s = aggregate(&entries);
        assert_eq!(s.by_day.get(&100).copied(), Some(22));
        assert_eq!(s.by_day.get(&101).copied(), Some(100));
    }

    #[test]
    fn aggregate_treats_missing_cost_as_zero() {
        let mut e = entry(0, "m", 1, 1, 0.0);
        e.cost_usd = None;
        let s = aggregate(&[e]);
        assert_eq!(s.total_cost_usd, 0.0);
    }

    #[test]
    fn aggregate_of_empty_slice_is_all_zeros() {
        let s = aggregate(&[]);
        assert_eq!(s.total_calls, 0);
        assert_eq!(s.total_prompt, 0);
        assert_eq!(s.total_completion, 0);
        assert_eq!(s.by_model.len(), 0);
        assert_eq!(s.by_day.len(), 0);
    }

    // ── stats_line_to_text + label_col_width + render_cost_text ─────────

    #[test]
    fn label_col_width_returns_widest_label_plus_two_space_gap() {
        let lines = vec![
            StatsLine::Kv {
                label: "ab".into(),
                value: "x".into(),
            },
            StatsLine::Kv {
                label: "abcd".into(),
                value: "y".into(),
            },
            StatsLine::Heading("ignored".into()),
        ];
        // max("ab", "abcd") = 4, plus KV_GAP (2) = 6.
        assert_eq!(label_col_width(&lines), 6);
    }

    #[test]
    fn label_col_width_is_just_the_gap_when_no_kv_lines_present() {
        assert_eq!(label_col_width(&[StatsLine::Heading("h".into())]), 2);
    }

    #[test]
    fn stats_line_to_text_pads_kv_label_to_the_col_width() {
        let line = StatsLine::Kv {
            label: "k".into(),
            value: "v".into(),
        };
        // label "k" (1 char) padded to col=4 → 3 spaces.
        assert_eq!(stats_line_to_text(&line, 4), "k   v");
    }

    #[test]
    fn stats_line_to_text_renders_each_variant_verbatim_or_via_unicode_glyphs() {
        assert_eq!(stats_line_to_text(&StatsLine::Heading("H".into()), 0), "H");
        assert_eq!(
            stats_line_to_text(&StatsLine::SparklineLegend("L".into()), 0),
            "L"
        );
        assert_eq!(stats_line_to_text(&StatsLine::Blank, 0), "");
        let heat = StatsLine::HeatRow {
            label: "Mo".into(),
            cells: vec![
                HeatCell::Empty,
                HeatCell::Level(0),
                HeatCell::Level(2),
                HeatCell::Level(9),
            ],
        };
        // Glyphs from per-level map, separated by spaces. Level ≥3 collapses to █.
        assert_eq!(stats_line_to_text(&heat, 0), "Mo · ░ ▓ █ ");
    }

    #[test]
    fn render_cost_text_joins_padded_lines_with_newlines() {
        let lines = vec![
            StatsLine::Heading("session".into()),
            StatsLine::Kv {
                label: "cost".into(),
                value: "$1.23".into(),
            },
            StatsLine::Kv {
                label: "turns".into(),
                value: "4".into(),
            },
        ];
        // Both KV labels are 5 chars; col_width = 7. cost → 2 spaces, turns → 2 spaces.
        let out = render_cost_text(&lines);
        let expected = "session\ncost   $1.23\nturns  4";
        assert_eq!(out, expected);
    }

    // ── render_session_cost ──────────────────────────────────────────────

    fn pricing(input: f64, output: f64, cr: f64, cw: f64) -> engine::pricing::ResolvedPricing {
        engine::pricing::ResolvedPricing {
            pricing: engine::pricing::ModelPricing {
                input,
                output,
                cache_read: cr,
                cache_write: cw,
            },
            source: engine::pricing::PricingSource::Config,
        }
    }

    fn labels(lines: &[StatsLine]) -> Vec<&str> {
        lines
            .iter()
            .filter_map(|l| match l {
                StatsLine::Kv { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn render_session_cost_includes_headline_session_block() {
        let lines = render_session_cost(1.23, "gpt-x", 4, &pricing(1.0, 2.0, 0.0, 0.0));
        let l = labels(&lines);
        assert!(l.contains(&"cost"));
        assert!(l.contains(&"model"));
        assert!(l.contains(&"turns"));
    }

    #[test]
    fn render_session_cost_renders_zero_session_cost_as_dollar_zero() {
        let lines = render_session_cost(0.0, "m", 0, &pricing(0.0, 0.0, 0.0, 0.0));
        let cost = lines
            .iter()
            .find_map(|l| match l {
                StatsLine::Kv { label, value } if label == "cost" => Some(value.as_str()),
                _ => None,
            })
            .unwrap();
        assert_eq!(cost, "$0");
    }

    #[test]
    fn render_session_cost_omits_input_output_rows_when_pricing_is_zero() {
        let lines = render_session_cost(0.0, "m", 0, &pricing(0.0, 0.0, 0.0, 0.0));
        let l = labels(&lines);
        assert!(!l.contains(&"input"));
        assert!(!l.contains(&"output"));
    }

    #[test]
    fn render_session_cost_includes_input_output_when_pricing_nonzero() {
        let lines = render_session_cost(0.0, "m", 0, &pricing(3.0, 15.0, 0.0, 0.0));
        let l = labels(&lines);
        assert!(l.contains(&"input"));
        assert!(l.contains(&"output"));
    }

    #[test]
    fn render_session_cost_conditionally_adds_cache_rows_only_when_rate_positive() {
        let with_cr = render_session_cost(0.0, "m", 0, &pricing(1.0, 1.0, 0.5, 0.0));
        let l = labels(&with_cr);
        assert!(l.contains(&"cache read"));
        assert!(!l.contains(&"cache write"));
    }

    #[test]
    fn render_session_cost_formats_zero_rates_as_em_dash() {
        let lines = render_session_cost(0.0, "m", 0, &pricing(0.0, 5.0, 0.0, 0.0));
        let input_value = lines
            .iter()
            .find_map(|l| match l {
                StatsLine::Kv { label, value } if label == "input" => Some(value.as_str()),
                _ => None,
            })
            .unwrap();
        // Zero rate renders as an em-dash placeholder.
        assert_eq!(input_value, "—");
    }
}
