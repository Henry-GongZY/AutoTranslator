//! Text translation providers.
//!
//! Recognition and translation are chosen independently: a session pairs any
//! ASR provider with any translation provider (or none). V1 translates only
//! committed (final) subtitles — the skill's rule that stable text gets
//! translated while volatile partials stay untouched; partial translation with
//! debounce/cancel is a later step.
//!
//! The `apple-translate` provider talks to a small Swift helper process
//! (`clients/macos/translator-bridge`) over a Unix socket with a JSON line
//! protocol. The bridge hosts the Apple Translation framework, which on
//! macOS 15 needs a SwiftUI view to obtain a `TranslationSession`; the core
//! only sees a socket. See `scripts/build-bridge.sh`.

pub mod apple;
pub mod mock;

use async_trait::async_trait;

use crate::error::{CoreError, Result};

/// Options handed to a translation provider when it is constructed.
pub struct TranslatorOptions {
    /// BCP-47 source language; the session requires it to be explicit.
    pub source_language: String,
    /// BCP-47 target language (e.g. `zh-Hans`).
    pub target_language: String,
}

#[async_trait]
pub trait Translator: Send + Sync {
    fn name(&self) -> &str;

    /// Translate one stable segment. Returning `Err` must never kill the
    /// subtitle pipeline: the session logs the failure and ships the subtitle
    /// untranslated.
    async fn translate(&mut self, text: &str) -> Result<String>;
}

/// Build the translator named by `provider`. `""`/`"none"` never reach here —
/// the session treats those as "translation disabled".
pub async fn create(provider: &str, opts: TranslatorOptions) -> Result<Box<dyn Translator>> {
    match provider {
        "mock" => Ok(Box::new(mock::MockTranslator)),
        "apple-translate" => Ok(Box::new(apple::AppleTranslator::connect(opts).await?)),
        other => Err(CoreError::Unsupported(format!(
            "translation provider `{other}` is not wired up (known: `mock`, `apple-translate`)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_translator_marks_output() {
        let mut t = mock::MockTranslator;
        let out = t.translate("hello world").await.unwrap();
        assert_eq!(out, "[mock-zh] hello world");
    }

    #[tokio::test]
    async fn unknown_provider_is_rejected() {
        let result = create(
            "bogus",
            TranslatorOptions {
                source_language: "en".into(),
                target_language: "zh-Hans".into(),
            },
        )
        .await;
        assert!(matches!(result, Err(CoreError::Unsupported(_))));
    }
}
