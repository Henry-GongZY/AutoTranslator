//! Streaming Apple SpeechAnalyzer ASR (macOS 26+) through `translator-bridge`.
//!
//! True streaming, unlike the whisper window scheme: audio is pushed to the
//! bridge (`asr-feed`) and recognition events flow back asynchronously
//! (`{"event":"asr",...}` lines), so there is no repeated whole-window decode.
//! The bridge reports availability up front; on macOS 15 this provider fails
//! fast and the session falls back to whisper (skill state-reporting rule).
//!
//! The `speech`/`infer_partial` hints are ignored — SpeechAnalyzer does its
//! own volatility handling (per `SpeechRecognizer` docs, true streaming
//! engines are free to ignore them).

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use super::{AsrOptions, RecognitionEvent, SpeechRecognizer};
use crate::error::{CoreError, Result};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Deserialize, Clone)]
struct BridgeReply {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    capabilities: Option<serde_json::Value>,
}

#[derive(Deserialize, Clone)]
struct AsrEvent {
    text: String,
    #[serde(default, rename = "final")]
    is_final: bool,
    #[serde(default)]
    start_us: u64,
    #[serde(default)]
    end_us: u64,
}

enum BridgeMessage {
    Reply(BridgeReply),
    Event(AsrEvent),
}

pub struct AppleSpeechAsr {
    writer: OwnedWriteHalf,
    replies: mpsc::Receiver<BridgeReply>,
    events: mpsc::Receiver<AsrEvent>,
    next_id: u64,
    session_id: String,
    locale: String,
    /// Bridge media clock -> core session clock offset, learned from the
    /// first pushed frame (events carry times measured from asr-start).
    clock_origin_us: Option<u64>,
    /// Latest volatile text, cleared when a final lands.
    current_partial: String,
}

impl std::fmt::Debug for AppleSpeechAsr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppleSpeechAsr")
            .field("locale", &self.locale)
            .field("started", &self.clock_origin_us.is_some())
            .finish()
    }
}

impl AppleSpeechAsr {
    pub async fn connect(opts: AsrOptions) -> Result<Self> {
        if opts.language.is_empty() {
            return Err(CoreError::Unsupported(
                "apple-speech requires an explicit language (no auto-detect)".into(),
            ));
        }
        let path =
            std::env::var("TRANSLATOR_BRIDGE_SOCKET").unwrap_or_else(|_| "/tmp/translator-bridge-v1.sock".into());

        let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&path))
            .await
            .map_err(|_| {
                CoreError::Provider(format!(
                    "connecting to the apple-speech bridge at {path} timed out; is translator-bridge running?"
                ))
            })?
            .map_err(|e| CoreError::Provider(format!("cannot reach the apple-speech bridge at {path}: {e}")))?;
        let (reader, writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let (reply_tx, replies) = mpsc::channel(16);
        let (event_tx, events) = mpsc::channel(64);
        tokio::spawn(async move {
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Ok(msg) = serde_json::from_slice::<serde_json::Value>(&line) else {
                    continue;
                };
                if msg.get("event").and_then(|v| v.as_str()) == Some("asr") {
                    if let Ok(event) = serde_json::from_value::<AsrEvent>(msg) {
                        if event_tx.send(event).await.is_err() {
                            break;
                        }
                    }
                } else if let Ok(reply) = serde_json::from_value::<BridgeReply>(msg) {
                    if reply_tx.send(reply).await.is_err() {
                        break;
                    }
                }
            }
        });

        let mut me = Self {
            writer,
            replies,
            events,
            next_id: 0,
            session_id: String::new(),
            locale: opts.language.clone(),
            clock_origin_us: None,
            current_partial: String::new(),
        };

        // Availability probe: fail fast per the skill's state-reporting rule.
        let caps = me.request("asr-capabilities", "").await?;
        let speech_available = caps
            .capabilities
            .as_ref()
            .and_then(|c| c.get("speech_available"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !speech_available {
            return Err(CoreError::Unsupported(
                "apple-speech requires macOS 26 or newer (SpeechAnalyzer unavailable); use the whisper engine"
                    .into(),
            ));
        }

        let started = me.request("asr-start", "").await?;
        if !started.ok {
            return Err(CoreError::Unsupported(format!(
                "apple-speech start failed: {}",
                started.error.unwrap_or_else(|| "unknown".into())
            )));
        }
        tracing::info!("apple-speech session started (locale={}, rate={})", me.locale, started.status.unwrap_or_default());
        Ok(me)
    }

    async fn request(&mut self, op: &str, payload: &str) -> Result<BridgeReply> {
        self.next_id += 1;
        let line = serde_json::json!({
            "id": self.next_id, "op": op, "text": payload,
            "source": self.locale, "target": "",
        });
        let request = async {
            self.writer
                .write_all(format!("{line}\n").as_bytes())
                .await
                .map_err(|e| CoreError::Provider(format!("apple-speech bridge write failed: {e}")))?;
            self.writer
                .flush()
                .await
                .map_err(|e| CoreError::Provider(format!("apple-speech bridge flush failed: {e}")))?;
            match self.replies.recv().await {
                Some(reply) => Ok(reply),
                None => Err(CoreError::Provider("apple-speech bridge closed the connection".into())),
            }
        };
        match tokio::time::timeout(REQUEST_TIMEOUT, request).await {
            Ok(result) => result,
            Err(_) => Err(CoreError::Provider(format!(
                "apple-speech bridge did not answer `{op}` within {REQUEST_TIMEOUT:?}"
            ))),
        }
    }

    async fn feed(&mut self, samples: &[f32]) -> Result<()> {
        let mut bytes = Vec::with_capacity(samples.len() * 4);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        self.request("asr-feed", &encoded).await.map(|_| ())
    }

    /// Drain pending recognition events into core events, mapping the bridge
    /// media clock onto the core session clock.
    async fn drain_events(&mut self, events: &mut Vec<RecognitionEvent>) -> Result<()> {
        while let Ok(event) = self.events.try_recv() {
            let origin = self.clock_origin_us.unwrap_or(0);
            let (start_us, end_us) = (origin + event.start_us, origin + event.end_us);
            if event.is_final {
                if !event.text.trim().is_empty() {
                    events.push(RecognitionEvent::Final {
                        text: event.text,
                        start_us,
                        end_us,
                        confidence: 1.0,
                    });
                }
                self.current_partial.clear();
            } else if event.text != self.current_partial && !event.text.trim().is_empty() {
                self.current_partial = event.text.clone();
                events.push(RecognitionEvent::Partial {
                    text: event.text,
                    start_us,
                    end_us,
                });
            }
        }
        Ok(())
    }
}

#[async_trait]
impl SpeechRecognizer for AppleSpeechAsr {
    fn name(&self) -> &str {
        "apple-speech"
    }

    async fn push_audio(
        &mut self,
        samples: &[f32],
        _speech: bool,
        end_us: u64,
        _infer_partial: bool,
    ) -> Result<Vec<RecognitionEvent>> {
        if self.clock_origin_us.is_none() && !samples.is_empty() {
            // The frame just pushed ends at `end_us`; the bridge clock started
            // at the same sample.
            self.clock_origin_us = Some(end_us.saturating_sub(
                samples.len() as u64 * 1_000_000 / 16_000,
            ));
        }
        if !samples.is_empty() {
            self.feed(samples).await?;
        }
        let mut events = Vec::new();
        self.drain_events(&mut events).await?;
        Ok(events)
    }

    async fn flush(&mut self) -> Result<Vec<RecognitionEvent>> {
        let mut events = Vec::new();
        if self.clock_origin_us.is_none() {
            return Ok(events); // never fed: nothing to finalize
        }
        let stopped = self.request("asr-stop", "").await?;
        if !stopped.ok {
            return Err(CoreError::Provider(format!(
                "apple-speech stop failed: {}",
                stopped.error.unwrap_or_else(|| "unknown".into())
            )));
        }
        self.drain_events(&mut events).await?;
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_language_is_rejected_upfront() {
        let err = AppleSpeechAsr::connect(AsrOptions {
            language: String::new(),
            ..Default::default()
        })
        .await
        .expect_err("must reject empty language");
        assert!(matches!(err, CoreError::Unsupported(_)));
    }
}
