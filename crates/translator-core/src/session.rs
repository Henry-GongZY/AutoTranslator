//! One capture session: decode -> down-mix -> resample -> VAD -> ASR -> subtitles.

use translator_protocol::{
    AudioFormat, AudioFrame, MetricsEvent, SampleFormat, StartSessionRequest, SubtitleEvent,
};

use crate::asr::{self, RecognitionEvent, SpeechRecognizer};
use crate::audio::{downmix_to_mono, pcm, resampler::SincResampler};
use crate::error::{CoreError, Result};
use crate::subtitle::{SubtitleConfig, SubtitleStabilizer};
use crate::vad::{Vad, VadConfig};

const DEFAULT_TARGET_RATE: u32 = 16_000;
const METRICS_EVERY_FRAMES: u64 = 64;

#[derive(Default)]
pub struct SessionOutput {
    pub subtitles: Vec<SubtitleEvent>,
    pub metrics: Option<MetricsEvent>,
}

#[derive(Default)]
struct Metrics {
    audio_frames: u64,
    audio_bytes: u64,
    dropped_frames: u64,
    speech_frames: u64,
    total_frames: u64,
    subtitle_events: u64,
}

pub struct Session {
    id: String,
    input_rate: u32,
    input_channels: usize,
    input_format: SampleFormat,
    target_rate: u32,
    resampler: Option<SincResampler>,
    vad: Vad,
    recognizer: Box<dyn SpeechRecognizer>,
    subtitles: SubtitleStabilizer,
    paused: bool,
    /// Accumulated audio time derived from the sample count, in microseconds.
    audio_us: f64,
    frames_since_metrics: u64,
    metrics: Metrics,
}

impl Session {
    pub async fn start(request: &StartSessionRequest, mock_sentences: &[String]) -> Result<Self> {
        let declared = request.input_format.clone().unwrap_or(AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            format: SampleFormat::F32 as i32,
        });

        if !(8_000..=192_000).contains(&declared.sample_rate) {
            return Err(CoreError::AudioFormat(format!(
                "input sample rate {} is out of range",
                declared.sample_rate
            )));
        }
        if declared.channels == 0 || declared.channels > 8 {
            return Err(CoreError::AudioFormat(format!(
                "unsupported channel count {}",
                declared.channels
            )));
        }
        let input_format = SampleFormat::try_from(declared.format).map_err(|_| {
            CoreError::AudioFormat(format!("unknown sample format {}", declared.format))
        })?;
        if input_format == SampleFormat::Unspecified {
            return Err(CoreError::AudioFormat(
                "input sample format was not specified".to_string(),
            ));
        }

        let target_rate = if request.target_sample_rate == 0 {
            DEFAULT_TARGET_RATE
        } else {
            request.target_sample_rate
        };
        if !(8_000..=48_000).contains(&target_rate) {
            return Err(CoreError::AudioFormat(format!(
                "target sample rate {target_rate} is out of range"
            )));
        }

        // Phase 1 ships without translation.
        if let Some(translation) = &request.translation {
            if !translation.provider.is_empty() && translation.provider != "none" {
                return Err(CoreError::Unsupported(format!(
                    "translation provider `{}` is not available in phase 1",
                    translation.provider
                )));
            }
        }

        let provider = request
            .asr
            .as_ref()
            .map(|a| a.provider.as_str())
            .unwrap_or("mock");
        let asr_cfg = request.asr.clone().unwrap_or_default();
        let asr_opts = asr::AsrOptions {
            sentences: mock_sentences.to_vec(),
            model: asr_cfg.model.clone(),
            engine: asr_cfg.engine.clone(),
            model_directory: asr_cfg.model_directory.clone(),
            language: asr_cfg.language.clone(),
            // The protocol does not carry a prompt yet; leave empty so the
            // whisper provider applies its zh auto-prompt rule.
            initial_prompt: String::new(),
        };
        let recognizer = asr::create(provider, asr_opts).await?;

        let subtitle_cfg = match &request.subtitle {
            Some(cfg) => SubtitleConfig {
                max_lines: if cfg.max_lines == 0 {
                    SubtitleConfig::default().max_lines
                } else {
                    cfg.max_lines as usize
                },
                max_chars_per_line: if cfg.max_chars_per_line == 0 {
                    SubtitleConfig::default().max_chars_per_line
                } else {
                    cfg.max_chars_per_line as usize
                },
                partial_interval_ms: cfg.partial_interval_ms as u64,
            },
            None => SubtitleConfig::default(),
        };

        let resampler = if declared.sample_rate == target_rate {
            None
        } else {
            Some(SincResampler::new(declared.sample_rate, target_rate)?)
        };

        let id = if request.session_id.is_empty() {
            format!("session-{}", std::process::id())
        } else {
            request.session_id.clone()
        };

        tracing::info!(
            session = %id,
            "session started: {} Hz x {} ch {:?} -> {} Hz, asr=`{}`",
            declared.sample_rate,
            declared.channels,
            input_format,
            target_rate,
            recognizer.name()
        );

        Ok(Self {
            id,
            input_rate: declared.sample_rate,
            input_channels: declared.channels as usize,
            input_format,
            target_rate,
            resampler,
            vad: Vad::new(VadConfig::new(target_rate)),
            recognizer,
            subtitles: SubtitleStabilizer::new(subtitle_cfg),
            paused: false,
            audio_us: 0.0,
            frames_since_metrics: 0,
            metrics: Metrics::default(),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Rate the recogniser actually consumes.
    pub fn target_rate(&self) -> u32 {
        self.target_rate
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub async fn push_frame(&mut self, frame: AudioFrame) -> Result<SessionOutput> {
        let mut output = SessionOutput::default();
        self.metrics.audio_frames += 1;
        self.metrics.audio_bytes += frame.pcm.len() as u64;

        if self.paused {
            return Ok(output);
        }

        let declared = frame.format.clone().unwrap_or(AudioFormat {
            sample_rate: self.input_rate,
            channels: self.input_channels as u32,
            format: self.input_format as i32,
        });
        let sample_format = SampleFormat::try_from(declared.format).map_err(|_| {
            self.metrics.dropped_frames += 1;
            CoreError::AudioFormat(format!("unknown sample format {}", declared.format))
        })?;
        let rate = if declared.sample_rate == 0 {
            self.input_rate
        } else {
            declared.sample_rate
        };
        let channels = if declared.channels == 0 {
            self.input_channels
        } else {
            declared.channels as usize
        };

        let bytes_per_sample = pcm::bytes_per_sample(sample_format)?;
        let expected = frame.frames as usize * channels * bytes_per_sample;
        if expected != frame.pcm.len() {
            self.metrics.dropped_frames += 1;
            return Err(CoreError::AudioFormat(format!(
                "frame declares {} frames x {} ch x {} B = {} bytes but carries {} bytes",
                frame.frames,
                channels,
                bytes_per_sample,
                expected,
                frame.pcm.len()
            )));
        }

        let samples = pcm::decode_to_f32(&frame.pcm, sample_format)?;

        self.audio_us += frame.frames as f64 * 1_000_000.0 / rate as f64;
        let end_us = self.audio_us as u64;

        let mono = downmix_to_mono(&samples, channels);
        let work: Vec<f32> = match self.resampler.as_mut() {
            Some(resampler) => {
                let mut out = Vec::with_capacity(mono.len() / 3 + 8);
                resampler.process(&mono, &mut out);
                out
            }
            None => mono,
        };

        let vad = self.vad.push(&work, end_us);
        self.metrics.speech_frames += vad.speech_frames as u64;
        self.metrics.total_frames += vad.completed_frames as u64;

        let events = self.recognizer.push_audio(&work, vad.active, end_us).await?;
        for event in events {
            let subtitle = match event {
                RecognitionEvent::Partial {
                    text,
                    start_us,
                    end_us,
                } => self.subtitles.on_partial(&text, start_us, end_us, &self.id),
                RecognitionEvent::Final {
                    text,
                    start_us,
                    end_us,
                    confidence,
                } => self
                    .subtitles
                    .on_final(&text, start_us, end_us, confidence, &self.id),
            };
            if let Some(subtitle) = subtitle {
                self.metrics.subtitle_events += 1;
                output.subtitles.push(subtitle);
            }
        }

        self.frames_since_metrics += 1;
        if self.frames_since_metrics >= METRICS_EVERY_FRAMES {
            self.frames_since_metrics = 0;
            output.metrics = Some(self.snapshot_metrics());
        }

        Ok(output)
    }

    /// Flush any buffered recognition result and return the final subtitles.
    pub async fn flush(&mut self) -> Result<Vec<SubtitleEvent>> {
        let events = self.recognizer.flush().await?;
        let mut out = Vec::new();
        for event in events {
            let subtitle = match event {
                RecognitionEvent::Partial {
                    text,
                    start_us,
                    end_us,
                } => self.subtitles.on_partial(&text, start_us, end_us, &self.id),
                RecognitionEvent::Final {
                    text,
                    start_us,
                    end_us,
                    confidence,
                } => self
                    .subtitles
                    .on_final(&text, start_us, end_us, confidence, &self.id),
            };
            if let Some(subtitle) = subtitle {
                self.metrics.subtitle_events += 1;
                out.push(subtitle);
            }
        }
        Ok(out)
    }

    pub fn snapshot_metrics(&self) -> MetricsEvent {
        MetricsEvent {
            audio_frames: self.metrics.audio_frames,
            audio_bytes: self.metrics.audio_bytes,
            audio_ms: (self.audio_us / 1000.0) as u64,
            dropped_frames: self.metrics.dropped_frames,
            speech_ratio: if self.metrics.total_frames == 0 {
                0.0
            } else {
                self.metrics.speech_frames as f64 / self.metrics.total_frames as f64
            },
            subtitle_events: self.metrics.subtitle_events,
        }
    }
}
