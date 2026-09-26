//! Client for the macOS Swift translation bridge (`translator-bridge`).
//!
//! The bridge is a separate helper process that hosts the Apple Translation
//! framework and serves one JSON line per request over a Unix socket:
//!
//! ```text
//! -> {"id":1,"op":"translate","text":"...","source":"en","target":"zh-Hans"}
//! <- {"id":1,"ok":true,"text":"你好"}
//! ```
//!
//! `capabilities` carries the system version and the `LanguageAvailability`
//! status for the requested pair, so a misconfigured session fails fast with a
//! clear error instead of producing untranslated subtitles.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;

use super::TranslatorOptions;
use crate::error::{CoreError, Result};

/// Socket the Swift bridge listens on; overridable for tests.
const DEFAULT_SOCKET: &str = "/tmp/translator-bridge-v1.sock";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Long enough to cover a first-use language asset download.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Deserialize)]
struct BridgeResponse {
    #[allow(dead_code)]
    id: u64,
    ok: bool,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    system_version: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

pub struct AppleTranslator {
    /// `(reader, writer)` of a live bridge connection; `None` after an I/O
    /// failure so the next translate call reconnects once.
    conn: Option<(BufReader<tokio::net::unix::OwnedReadHalf>, OwnedWriteHalf)>,
    path: String,
    source: String,
    target: String,
    next_id: u64,
}

impl std::fmt::Debug for AppleTranslator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppleTranslator")
            .field("path", &self.path)
            .field("source", &self.source)
            .field("target", &self.target)
            .field("connected", &self.conn.is_some())
            .finish()
    }
}

impl AppleTranslator {
    /// Connect to the bridge and probe the requested language pair. Fails
    /// fast — with the reason — when the bridge is missing, the system cannot
    /// translate, or the language assets are not installed.
    pub async fn connect(opts: TranslatorOptions) -> Result<Self> {
        let path = std::env::var("TRANSLATOR_BRIDGE_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.into());
        let mut me = Self {
            conn: None,
            path,
            source: opts.source_language,
            target: opts.target_language,
            next_id: 0,
        };
        me.connect_locked().await?;
        // Probe the pair so a bad session fails at start, not mid-subtitle.
        let probe = me.request("capabilities", "").await?;
        if !probe.ok {
            return Err(CoreError::Unsupported(format!(
                "apple-translate bridge rejected language pair {}→{}: {}",
                me.source,
                me.target,
                probe.error.unwrap_or_else(|| "unknown error".into())
            )));
        }
        if let Some(version) = &probe.system_version {
            tracing::info!("apple-translate bridge ready (macOS {version})");
        }
        if let Some(status) = &probe.status {
            tracing::info!("apple-translate {}→{} availability: {status}", me.source, me.target);
            if status != "installed" {
                return Err(CoreError::Unsupported(format!(
                    "language assets for {} → {} are not installed (status: {status}); \
                     install them once via any app that offers system translation \
                     (e.g. Safari's translate button), then restart the session",
                    me.source, me.target
                )));
            }
        }
        Ok(me)
    }

    async fn connect_locked(&mut self) -> Result<()> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.path))
            .await
            .map_err(|_| {
                CoreError::Provider(format!(
                    "connecting to the apple-translate bridge at {} timed out; is translator-bridge running?",
                    self.path
                ))
            })?
            .map_err(|e| {
                CoreError::Provider(format!(
                    "cannot reach the apple-translate bridge at {}: {e}",
                    self.path
                ))
            })?;
        let (reader, writer) = stream.into_split();
        self.conn = Some((BufReader::new(reader), writer));
        Ok(())
    }

    async fn request(&mut self, op: &str, text: &str) -> Result<BridgeResponse> {
        if self.conn.is_none() {
            self.connect_locked().await?;
        }
        self.next_id += 1;
        let id = self.next_id;
        // serde_json to_string escapes the payload; the protocol is line framed.
        let line = serde_json::json!({
            "id": id,
            "op": op,
            "text": text,
            "source": self.source,
            "target": self.target,
        });
        let payload = format!("{line}\n");

        let request = async {
            let (_, writer) = self.conn.as_mut().expect("connection just opened");
            writer.write_all(payload.as_bytes()).await?;
            writer.flush().await?;
            let (reader, _) = self.conn.as_mut().expect("connection just opened");
            let mut buf = Vec::new();
            reader.read_until(b'\n', &mut buf).await?;
            if buf.is_empty() {
                return Err(CoreError::Provider("apple-translate bridge closed the connection".into()));
            }
            serde_json::from_slice::<BridgeResponse>(&buf)
                .map_err(|e| CoreError::Provider(format!("bad bridge response: {e}")))
        };

        match tokio::time::timeout(REQUEST_TIMEOUT, request).await {
            Ok(result) => {
                if result.is_err() {
                    // Force a reconnect on the next call; the socket state is
                    // no longer trustworthy after an I/O or framing failure.
                    self.conn = None;
                }
                result
            }
            Err(_) => {
                self.conn = None;
                Err(CoreError::Provider(format!(
                    "apple-translate bridge did not answer `{op}` within {REQUEST_TIMEOUT:?}"
                )))
            }
        }
    }
}

#[async_trait]
impl super::Translator for AppleTranslator {
    fn name(&self) -> &str {
        "apple-translate"
    }

    async fn translate(&mut self, text: &str) -> Result<String> {
        if text.trim().is_empty() {
            return Ok(String::new());
        }
        let response = self.request("translate", text).await?;
        if response.ok {
            return Ok(response.text.unwrap_or_default());
        }
        // A refused translation (missing assets, pair revoked) can persist, so
        // surface the reason; the session keeps the subtitle untranslated.
        Err(CoreError::Provider(format!(
            "apple-translate failed: {}",
            response.error.unwrap_or_else(|| "unknown error".into())
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_bridge_fails_fast_with_hint() {
        // A path nothing listens on.
        std::env::set_var("TRANSLATOR_BRIDGE_SOCKET", "/tmp/no-such-bridge-socket.sock");
        let err = AppleTranslator::connect(TranslatorOptions {
            source_language: "en".into(),
            target_language: "zh-Hans".into(),
        })
        .await
        .expect_err("connect must fail");
        assert!(err.to_string().contains("bridge"), "{err}");
        std::env::remove_var("TRANSLATOR_BRIDGE_SOCKET");
    }
}
