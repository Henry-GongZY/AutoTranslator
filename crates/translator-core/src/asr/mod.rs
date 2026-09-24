//! Speech recognition provider abstraction.

pub mod mock;

use async_trait::async_trait;

use crate::error::Result;

/// What a recogniser reports back to the pipeline.
#[derive(Debug, Clone, PartialEq)]
pub enum RecognitionEvent {
    /// Unstable text that may still be replaced.
    Partial {
        text: String,
        start_us: u64,
        end_us: u64,
    },
    /// Stable text that will not change any more.
    Final {
        text: String,
        start_us: u64,
        end_us: u64,
        confidence: f32,
    },
}

/// Streaming recogniser fed with 16 kHz mono audio.
///
/// `speech` is the VAD hint: mock/segmented engines use it to delimit
/// utterances, true streaming engines are free to ignore it.
#[async_trait]
pub trait SpeechRecognizer: Send + Sync {
    fn name(&self) -> &str;

    /// `end_us` is the timestamp of the last sample in `samples` on the session
    /// audio clock.
    async fn push_audio(
        &mut self,
        samples: &[f32],
        speech: bool,
        end_us: u64,
    ) -> Result<Vec<RecognitionEvent>>;

    /// Emit whatever is still buffered (used when a session stops).
    async fn flush(&mut self) -> Result<Vec<RecognitionEvent>>;
}

/// Build the recogniser named by `provider`.
pub fn create(provider: &str, sentences: Vec<String>) -> Result<Box<dyn SpeechRecognizer>> {
    match provider {
        "" | "mock" => Ok(Box::new(mock::MockRecognizer::new(sentences))),
        other => Err(crate::error::CoreError::Unsupported(format!(
            "asr provider `{other}` is not wired up in phase 1 (only `mock` is available)"
        ))),
    }
}
