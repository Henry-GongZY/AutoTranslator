//! Turns raw recognition events into a stable block of display lines.

use std::collections::VecDeque;

use translator_protocol::{SubtitleEvent, SubtitleKind};

/// Cap on rendered visual lines so a runaway transcript cannot blow up the
/// overlay window.
const MAX_RENDERED_LINES: usize = 6;

pub struct SubtitleConfig {
    /// How many committed utterances to keep on screen.
    pub max_lines: usize,
    pub max_chars_per_line: usize,
    /// Minimum spacing between partial updates, in milliseconds.
    pub partial_interval_ms: u64,
}

impl Default for SubtitleConfig {
    fn default() -> Self {
        Self {
            max_lines: 2,
            max_chars_per_line: 42,
            partial_interval_ms: 120,
        }
    }
}

pub struct SubtitleStabilizer {
    cfg: SubtitleConfig,
    seq: u64,
    history: VecDeque<String>,
}

impl SubtitleStabilizer {
    pub fn new(cfg: SubtitleConfig) -> Self {
        Self {
            cfg,
            seq: 0,
            history: VecDeque::new(),
        }
    }

    pub fn on_partial(
        &mut self,
        text: &str,
        start_us: u64,
        end_us: u64,
        session_id: &str,
    ) -> Option<SubtitleEvent> {
        let text = normalize(text);
        if text.is_empty() {
            return None;
        }
        self.seq += 1;
        let mut lines = self.rendered_history();
        lines.extend(wrap(&text, self.cfg.max_chars_per_line));
        Some(self.build(
            SubtitleKind::Partial,
            text,
            lines,
            start_us,
            end_us,
            0.0,
            session_id,
        ))
    }

    pub fn on_final(
        &mut self,
        text: &str,
        start_us: u64,
        end_us: u64,
        confidence: f32,
        session_id: &str,
    ) -> Option<SubtitleEvent> {
        let text = normalize(text);
        if text.is_empty() {
            return None;
        }
        // Drop exact repeats, which streaming engines emit all the time.
        if self.history.back() == Some(&text) {
            return None;
        }

        self.history.push_back(text.clone());
        while self.history.len() > self.cfg.max_lines.max(1) {
            self.history.pop_front();
        }

        self.seq += 1;
        let lines = self.rendered_history();
        Some(self.build(
            SubtitleKind::Committed,
            text,
            lines,
            start_us,
            end_us,
            confidence as f64,
            session_id,
        ))
    }

    pub fn reset(&mut self) {
        self.history.clear();
    }

    fn rendered_history(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for entry in &self.history {
            lines.extend(wrap(entry, self.cfg.max_chars_per_line));
        }
        trim_to_last(lines, MAX_RENDERED_LINES)
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        &self,
        kind: SubtitleKind,
        text: String,
        lines: Vec<String>,
        start_us: u64,
        end_us: u64,
        confidence: f64,
        session_id: &str,
    ) -> SubtitleEvent {
        SubtitleEvent {
            session_id: session_id.to_string(),
            seq: self.seq,
            kind: kind as i32,
            text,
            translated_text: String::new(),
            language: String::new(),
            start_us,
            end_us,
            confidence,
            lines,
        }
    }
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn wrap(text: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 || text.is_empty() {
        return vec![text.to_string()];
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return vec![text.to_string()];
    }
    chars
        .chunks(max_chars)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect()
}

fn trim_to_last(mut lines: Vec<String>, limit: usize) -> Vec<String> {
    if lines.len() > limit {
        let excess = lines.len() - limit;
        lines.drain(..excess);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_the_last_lines() {
        let mut subs = SubtitleStabilizer::new(SubtitleConfig {
            max_lines: 2,
            ..Default::default()
        });
        subs.on_final("first", 0, 100, 1.0, "s").unwrap();
        subs.on_final("second", 100, 200, 1.0, "s").unwrap();
        let third = subs.on_final("third", 200, 300, 1.0, "s").unwrap();
        assert_eq!(third.lines, vec!["second".to_string(), "third".to_string()]);
    }

    #[test]
    fn ignores_repeated_finals() {
        let mut subs = SubtitleStabilizer::new(SubtitleConfig::default());
        assert!(subs.on_final("same", 0, 100, 1.0, "s").is_some());
        assert!(subs.on_final("same", 100, 200, 1.0, "s").is_none());
    }

    #[test]
    fn appends_partial_below_history() {
        let mut subs = SubtitleStabilizer::new(SubtitleConfig::default());
        subs.on_final("committed", 0, 100, 1.0, "s").unwrap();
        let partial = subs.on_partial("typing", 100, 200, "s").unwrap();
        assert_eq!(partial.lines, vec!["committed".to_string(), "typing".to_string()]);
        assert_eq!(partial.kind, SubtitleKind::Partial as i32);
    }

    #[test]
    fn wraps_long_lines() {
        let long = "あ".repeat(10);
        let wrapped = wrap(&long, 4);
        assert_eq!(wrapped, vec!["ああああ", "ああああ", "ああ"]);
    }
}
