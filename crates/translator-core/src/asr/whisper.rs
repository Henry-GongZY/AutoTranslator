//! Local Whisper ASR through the `whisper-rs` binding (whisper.cpp).
//!
//! Whisper is an offline, non-streaming model, so we approximate realtime
//! subtitles with a sliding-window scheme driven by the VAD:
//!
//! * While voice is active we accumulate 16 kHz mono audio for the current
//!   utterance and re-decode the buffered window every `STEP_SECONDS`, emitting
//!   the transcript as a `Partial`.
//! * When voice activity ends we decode once more and emit the transcript as a
//!   `Final`, then drop the committed audio so the next utterance starts clean.
//!
//! The subtitle stabilizer de-duplicates and keeps the on-screen block stable.
//! The first decode also compiles any GPU kernels, so it is slower than the rest.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState};

use super::{AsrOptions, RecognitionEvent, SpeechRecognizer};
use crate::error::{CoreError, Result};

const SAMPLE_RATE: u32 = 16_000;
/// Maximum audio kept for one utterance (seconds). Whisper's context window is
/// ~30 s, 12 s is a comfortable, low-latency default.
const WINDOW_SECONDS: f32 = 12.0;
/// Re-decode cadence while speaking (seconds of new audio since last run).
const STEP_SECONDS: f32 = 2.0;
/// Skip decoding utterances shorter than this (seconds).
const MIN_UTTERANCE_SECONDS: f32 = 0.5;

#[allow(dead_code)]
pub struct WhisperAsr {
    /// Owns the model weights; shared, never mutated after load.
    ctx: WhisperContext,
    /// Per-call inference state. Behind a `Mutex` so the struct stays `Sync`.
    state: Mutex<WhisperState>,
    /// Whisper language hint (empty = auto-detect); stored owned so we can build
    /// `FullParams` (which borrows the hint) per decode call.
    language: String,
    /// Monotonic audio of the current utterance (16 kHz mono f32).
    audio: Vec<f32>,
    /// Absolute session time (us) of `audio[0]`.
    audio_start_us: u64,
    /// Absolute session time up to which we last ran inference.
    last_infer_end_us: u64,
    speaking: bool,
    device: String,
}

impl WhisperAsr {
    pub async fn new(opts: AsrOptions) -> Result<Self> {
        let model_path = resolve_model(&opts.model).await?;

        let (ctx, device) = load_context(&model_path)?;
        let state = ctx
            .create_state()
            .map_err(|e| CoreError::Model(format!("whisper state init failed: {e}")))?;

        tracing::info!(
            "whisper asr ready: model={} device={} language={}",
            model_path.display(),
            device,
            if opts.language.is_empty() {
                "auto"
            } else {
                &opts.language
            }
        );
        println!(
            "[translator-core] whisper device: {device} (model={})",
            model_path.display()
        );

        Ok(Self {
            ctx,
            state: Mutex::new(state),
            language: opts.language,
            audio: Vec::new(),
            audio_start_us: 0,
            last_infer_end_us: 0,
            speaking: false,
            device,
        })
    }

    /// Build a fresh `FullParams` for one decode, borrowing `self.language`.
    fn build_params(&self) -> FullParams<'_, '_> {
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(if self.language.is_empty() {
            None
        } else {
            Some(self.language.as_str())
        });
        params.set_translate(false);
        params.set_token_timestamps(true);
        params.set_print_progress(false);
        params.set_suppress_blank(true);
        // Each window is an independent utterance; do not carry context across.
        params.set_no_context(true);
        params.set_max_len(0);
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(8);
        params.set_n_threads(threads as i32);
        params
    }

    /// Decode the current utterance buffer and return `(text, start_us, end_us)`
    /// for each segment in absolute session time.
    fn decode(&mut self) -> Result<Vec<(String, u64, u64)>> {
        let min_samples = (MIN_UTTERANCE_SECONDS * SAMPLE_RATE as f32) as usize;
        if self.audio.len() < min_samples {
            return Ok(Vec::new());
        }

        let params = self.build_params();
        let mut state = self
            .state
            .lock()
            .map_err(|_| CoreError::Internal("whisper state mutex poisoned".into()))?;
        state
            .full(params, &self.audio)
            .map_err(|e| CoreError::Provider(format!("whisper inference failed: {e}")))?;

        let n = state
            .full_n_segments()
            .map_err(|e| CoreError::Provider(format!("whisper segment count: {e}")))?
            as i32;

        let mut segs = Vec::with_capacity(n.max(0) as usize);
        for i in 0..n {
            let text = state
                .full_get_segment_text(i)
                .map_err(|e| CoreError::Provider(format!("whisper segment text: {e}")))?;
            let t0 = state
                .full_get_segment_t0(i)
                .map_err(|e| CoreError::Provider(format!("whisper t0: {e}")))?;
            let t1 = state
                .full_get_segment_t1(i)
                .map_err(|e| CoreError::Provider(format!("whisper t1: {e}")))?;
            let start_us = self.audio_start_us.saturating_add(t0 as u64 * 10_000);
            let end_us = self.audio_start_us.saturating_add(t1 as u64 * 10_000);
            segs.push((text.trim().to_string(), start_us, end_us));
        }
        Ok(segs)
    }
}

#[async_trait]
impl SpeechRecognizer for WhisperAsr {
    fn name(&self) -> &str {
        "whisper"
    }

    async fn push_audio(
        &mut self,
        samples: &[f32],
        speech: bool,
        end_us: u64,
    ) -> Result<Vec<RecognitionEvent>> {
        let mut events = Vec::new();

        // Append and keep at most one utterance's worth of audio.
        self.audio.extend_from_slice(samples);
        let cap = (WINDOW_SECONDS * SAMPLE_RATE as f32) as usize;
        if self.audio.len() > cap {
            let drop = self.audio.len() - cap;
            self.audio.drain(0..drop);
            self.audio_start_us += drop as u64 * 1_000_000 / SAMPLE_RATE as u64;
        }

        if speech {
            if !self.speaking {
                self.speaking = true;
            }
            let since = end_us.saturating_sub(self.last_infer_end_us);
            if since >= (STEP_SECONDS * 1_000_000.0) as u64 {
                self.last_infer_end_us = end_us;
                let segs = self.decode()?;
                let text = join_segments(&segs);
                if !text.is_empty() {
                    events.push(RecognitionEvent::Partial {
                        text,
                        start_us: self.audio_start_us,
                        end_us,
                    });
                }
            }
        } else if self.speaking {
            self.speaking = false;
            self.last_infer_end_us = end_us;
            let segs = self.decode()?;
            let text = join_segments(&segs);
            if !text.is_empty() {
                let start = segs.first().map(|s| s.1).unwrap_or(self.audio_start_us);
                events.push(RecognitionEvent::Final {
                    text,
                    start_us: start,
                    end_us,
                    confidence: 1.0,
                });
            }
            // Commit: discard the processed audio so the next utterance is fresh.
            self.audio.clear();
            self.audio_start_us = end_us;
        }

        Ok(events)
    }

    async fn flush(&mut self) -> Result<Vec<RecognitionEvent>> {
        if !self.speaking && self.audio.is_empty() {
            return Ok(Vec::new());
        }
        self.speaking = false;
        self.last_infer_end_us = self.audio_end_us();
        let segs = self.decode()?;
        let text = join_segments(&segs);
        let mut events = Vec::new();
        if !text.is_empty() {
            let start = segs.first().map(|s| s.1).unwrap_or(self.audio_start_us);
            events.push(RecognitionEvent::Final {
                text,
                start_us: start,
                end_us: self.audio_end_us(),
                confidence: 1.0,
            });
        }
        self.audio.clear();
        Ok(events)
    }
}

impl WhisperAsr {
    fn audio_end_us(&self) -> u64 {
        self.audio_start_us + self.audio.len() as u64 * 1_000_000 / SAMPLE_RATE as u64
    }
}

fn join_segments(segs: &[(String, u64, u64)]) -> String {
    segs.iter()
        .map(|(t, _, _)| t.as_str())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Locate (downloading if necessary) the requested Whisper model file.
async fn resolve_model(name: &str) -> Result<PathBuf> {
    if let Ok(explicit) = std::env::var("TRANSLATOR_WHISPER_MODEL") {
        let path = PathBuf::from(&explicit);
        if path.exists() {
            return Ok(path);
        }
        return Err(CoreError::Model(format!(
            "TRANSLATOR_WHISPER_MODEL points to {path:?} which does not exist"
        )));
    }

    let mut dir = models_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| CoreError::Model(format!("cannot create models dir: {e}")))?;
    dir.push(name);
    if dir.exists() {
        return Ok(dir);
    }

    let base = std::env::var("TRANSLATOR_MODEL_BASE_URL").unwrap_or_else(|_| {
        // Mirror that works well from mainland China; override for other regions.
        "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/".to_string()
    });
    let url = format!("{base}{name}");
    download(&url, &dir).await?;
    Ok(dir)
}

fn models_dir() -> PathBuf {
    let mut dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();
    dir.push("models");
    dir
}

async fn download(url: &str, dest: &Path) -> Result<()> {
    tracing::info!("downloading whisper model from {url} ...");
    let resp = reqwest::get(url)
        .await
        .map_err(|e| CoreError::Model(format!("model download request failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(CoreError::Model(format!(
            "model download failed: HTTP {}",
            resp.status()
        )));
    }
    let total = resp.content_length().unwrap_or(0);
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| CoreError::Model(format!("model download stream failed: {e}")))?;

    let tmp = dest.with_extension("part");
    std::fs::write(&tmp, &bytes)
        .map_err(|e| CoreError::Model(format!("cannot write {tmp:?}: {e}")))?;
    std::fs::rename(&tmp, dest)
        .map_err(|e| CoreError::Model(format!("cannot finalize model: {e}")))?;
    tracing::info!(
        "whisper model saved to {dest:?} ({} MB)",
        bytes.len() / 1_000_000
    );
    let _ = total;
    Ok(())
}

fn load_context(path: &Path) -> Result<(WhisperContext, String)> {
    let path_str = path.to_str().unwrap_or_default();
    let try_gpu = WhisperContextParameters {
        use_gpu: true,
        ..Default::default()
    };
    match WhisperContext::new_with_params(path_str, try_gpu) {
        Ok(ctx) => Ok((ctx, "GPU".to_string())),
        Err(gpu_err) => {
            tracing::warn!("whisper GPU init failed ({gpu_err}); falling back to CPU");
            let cpu = WhisperContextParameters {
                use_gpu: false,
                ..Default::default()
            };
            let ctx = WhisperContext::new_with_params(path_str, cpu)
                .map_err(|e| CoreError::Model(format!("whisper context load failed: {e}")))?;
            Ok((ctx, "CPU".to_string()))
        }
    }
}
