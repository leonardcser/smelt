use std::collections::VecDeque;

pub const DEFAULT_MAX_LINES: usize = 2_000;
pub const DEFAULT_MAX_BYTES: usize = 100_000;
pub const TRUNCATION_NOTICE: &str = "[process output truncated; showing tail]";

#[derive(Clone, Debug)]
pub struct OutputLimiter {
    lines: VecDeque<String>,
    retained_bytes: usize,
    total_lines: usize,
    total_bytes: usize,
    max_lines: usize,
    max_bytes: usize,
}

impl Default for OutputLimiter {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES)
    }
}

impl OutputLimiter {
    pub fn new(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            retained_bytes: 0,
            total_lines: 0,
            total_bytes: 0,
            max_lines,
            max_bytes,
        }
    }

    pub fn push_line(&mut self, line: String) {
        self.total_lines += 1;
        self.total_bytes = self.total_bytes.saturating_add(line.len());
        let line = suffix_with_byte_budget(&line, self.max_bytes);
        self.retained_bytes = self.retained_bytes.saturating_add(line.len());
        self.lines.push_back(line);
        self.truncate_tail();
    }

    pub fn push_text(&mut self, text: &str) {
        for line in text.lines() {
            self.push_line(line.to_string());
        }
    }

    pub fn drain_text(&mut self) -> String {
        let text = self.format_text();
        self.lines.clear();
        self.retained_bytes = 0;
        self.total_lines = 0;
        self.total_bytes = 0;
        text
    }

    pub fn format_text(&self) -> String {
        let body = self
            .lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        if !self.truncated() {
            return body;
        }

        let mut parts = Vec::new();
        if self.total_lines > self.lines.len() {
            parts.push(format!(
                "last {} of {} lines",
                self.lines.len(),
                self.total_lines
            ));
        }
        if self.total_bytes > self.retained_bytes {
            parts.push(format!(
                "last {} of {} bytes",
                self.retained_bytes, self.total_bytes
            ));
        }
        format!("{TRUNCATION_NOTICE}: {}\n\n{body}", parts.join(", "))
    }

    #[cfg(test)]
    pub fn retained_lines(&self) -> usize {
        self.lines.len()
    }

    fn truncated(&self) -> bool {
        self.total_lines > self.lines.len() || self.total_bytes > self.retained_bytes
    }

    fn truncate_tail(&mut self) {
        while self.lines.len() > self.max_lines || self.retained_bytes > self.max_bytes {
            let Some(line) = self.lines.pop_front() else {
                self.retained_bytes = 0;
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(line.len());
        }
    }
}

pub fn limit_text_tail(text: &str) -> String {
    let mut limiter = OutputLimiter::default();
    limiter.push_text(text);
    limiter.format_text()
}

fn suffix_with_byte_budget(content: &str, max_bytes: usize) -> String {
    if content.len() <= max_bytes {
        return content.to_string();
    }
    let mut start = content.len();
    let mut used = 0;
    for (idx, ch) in content.char_indices().rev() {
        let len = ch.len_utf8();
        if used + len > max_bytes {
            break;
        }
        start = idx;
        used += len;
    }
    content[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_to_max_lines() {
        let mut output = OutputLimiter::default();
        for i in 0..(DEFAULT_MAX_LINES + 5) {
            output.push_line(format!("line{i}"));
        }
        assert_eq!(output.retained_lines(), DEFAULT_MAX_LINES);
        let text = output.format_text();
        assert!(text.contains(TRUNCATION_NOTICE));
        assert!(!text.contains("line0\n"));
        assert!(text.ends_with(&format!("line{}", DEFAULT_MAX_LINES + 4)));
    }

    #[test]
    fn truncates_long_single_line() {
        let mut output = OutputLimiter::new(10, 8);
        output.push_line("abcdeféXYZ".to_string());
        let text = output.format_text();
        assert!(text.contains(TRUNCATION_NOTICE));
        assert!(text.ends_with("eféXYZ"));
    }

    #[test]
    fn drain_resets_buffer_counters() {
        let mut output = OutputLimiter::new(1, 100);
        output.push_line("one".to_string());
        output.push_line("two".to_string());
        assert!(output.drain_text().contains(TRUNCATION_NOTICE));
        assert_eq!(output.format_text(), "");
    }
}
