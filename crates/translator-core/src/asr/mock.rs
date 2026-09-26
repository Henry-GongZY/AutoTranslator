//! Deterministic stand-in recogniser so the whole pipeline can be exercised
//! without any credentials or network access.
//!
//! Every detected utterance produces one `Final` event taken from a rotating
//! sentence list, plus `Partial` events that grow while the speaker is active.

use async_trait::async_trait;

use super::{RecognitionEvent, SpeechRecognizer};
use crate::error::Result;

const DEFAULT_SENTENCES: &[&str] = &[
    "这是第一段模拟识别结果。",
    "第二段：当前阶段还没有接入云端语音识别服务。",
    "第三段，这一段刻意写得比较长一些，用来验证字幕窗口的自动换行、最大行数限制以及历史滚动是否正常。",
    "Fourth line: mock recogniser output for an English sentence.",
];

pub struct MockRecognizer {
    sentences: Vec<String>,
    index: usize,
    speaking: bool,
    segment_start_us: u64,
    last_partial_us: u64,
    partial_interval_us: u64,
    chars_per_second: f64,
}

impl MockRecognizer {
    pub fn new(sentences: Vec<String>) -> Self {
        let sentences = if sentences.is_empty() {
            DEFAULT_SENTENCES.iter().map(|s| s.to_string()).collect()
        } else {
            sentences
        };
        Self {
            sentences,
            index: 0,
            speaking: false,
            segment_start_us: 0,
            last_partial_us: 0,
            partial_interval_us: 120_000,
            chars_per_second: 14.0,
        }
    }

    fn sentence(&self) -> Option<&str> {
        self.sentences.get(self.index % self.sentences.len()).map(|s| s.as_str())
    }

    fn full_text(&self, start_us: u64, end_us: u64) -> String {
        let seconds = end_us.saturating_sub(start_us) as f64 / 1_000_000.0;
        match self.sentence() {
            Some(sentence) => format!("[{}] {}", self.index + 1, sentence),
            None => format!("[{}] 检测到语音片段，时长 {seconds:.2} 秒", self.index + 1),
        }
    }

    fn partial_text(&self, elapsed_seconds: f64) -> String {
        let full = self.full_text(self.segment_start_us, self.segment_start_us);
        let chars: Vec<char> = full.chars().collect();
        let take = (elapsed_seconds * self.chars_per_second).round() as usize;
        if take >= chars.len() {
            full
        } else {
            chars[..take].iter().collect()
        }
    }

    fn finish_segment(&mut self, end_us: u64) -> RecognitionEvent {
        let text = self.full_text(self.segment_start_us, end_us);
        let event = RecognitionEvent::Final {
            text,
            start_us: self.segment_start_us,
            end_us,
            confidence: 1.0,
        };
        self.index += 1;
        event
    }
}

#[async_trait]
impl SpeechRecognizer for MockRecognizer {
    fn name(&self) -> &str {
        "mock"
    }

    async fn push_audio(
        &mut self,
        _samples: &[f32],
        speech: bool,
        end_us: u64,
        _infer_partial: bool,
    ) -> Result<Vec<RecognitionEvent>> {
        let mut events = Vec::new();

        if speech {
            if !self.speaking {
                self.speaking = true;
                self.segment_start_us = end_us;
                self.last_partial_us = end_us;
            }
            if end_us.saturating_sub(self.last_partial_us) >= self.partial_interval_us {
                let elapsed = end_us.saturating_sub(self.segment_start_us) as f64 / 1_000_000.0;
                events.push(RecognitionEvent::Partial {
                    text: self.partial_text(elapsed),
                    start_us: self.segment_start_us,
                    end_us,
                });
                self.last_partial_us = end_us;
            }
        } else if self.speaking {
            self.speaking = false;
            events.push(self.finish_segment(end_us));
        }

        Ok(events)
    }

    async fn flush(&mut self) -> Result<Vec<RecognitionEvent>> {
        if !self.speaking {
            return Ok(Vec::new());
        }
        self.speaking = false;
        Ok(vec![self.finish_segment(self.last_partial_us)])
    }
}
