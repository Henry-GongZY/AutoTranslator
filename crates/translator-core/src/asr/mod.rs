//! Speech recognition provider abstraction.

pub mod mock;

#[cfg(feature = "whisper")]
pub mod whisper;

use async_trait::async_trait;

use crate::error::{CoreError, Result};

/// Options handed to an ASR provider when it is constructed.
pub struct AsrOptions {
    pub engine: String,
    pub model_directory: String,
    /// Rotating sentences for the mock provider.
    pub sentences: Vec<String>,
    /// Whisper catalog ID or file name (e.g. `tiny` or `ggml-tiny.bin`).
    pub model: String,
    /// Whisper language hint (e.g. `zh`, `en`); empty means auto-detect.
    pub language: String,
    /// Whisper initial prompt; empty means the provider default, which for
    /// Whisper is a Simplified-Chinese bias prompt injected whenever
    /// `language` is `zh` (whisper's raw zh output skews Traditional).
    pub initial_prompt: String,
}

impl Default for AsrOptions {
    fn default() -> Self {
        Self {
            engine: String::new(),
            model_directory: String::new(),
            sentences: Vec::new(),
            model: "ggml-tiny.bin".to_string(),
            language: String::new(),
            initial_prompt: String::new(),
        }
    }
}

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
///
/// `infer_partial` lets the caller merge backlogged mid-utterance decodes:
/// when more audio is already queued behind this call, the caller passes
/// `false` so the recogniser only buffers, and the newest push of a batch
/// passes `true` to run the partial decode once. Utterance-end (final)
/// decoding is unaffected and always runs when the VAD closes.
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
        infer_partial: bool,
    ) -> Result<Vec<RecognitionEvent>>;

    /// Emit whatever is still buffered (used when a session stops).
    async fn flush(&mut self) -> Result<Vec<RecognitionEvent>>;
}

/// Build the recogniser named by `provider`.
pub async fn create(provider: &str, opts: AsrOptions) -> Result<Box<dyn SpeechRecognizer>> {
    match provider {
        "" | "mock" => Ok(Box::new(mock::MockRecognizer::new(opts.sentences))),

        #[cfg(feature = "whisper")]
        "whisper" => Ok(Box::new(whisper::WhisperAsr::new(opts).await?)),

        #[cfg(not(feature = "whisper"))]
        "whisper" => Err(CoreError::Unsupported(
            "asr provider `whisper` requires the `whisper` feature (and a CUDA/CPU build)".into(),
        )),

        other => Err(CoreError::Unsupported(format!(
            "asr provider `{other}` is not wired up in phase 1 (only `mock` and `whisper` exist)"
        ))),
    }
}
