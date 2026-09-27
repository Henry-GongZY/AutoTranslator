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

// Apple bridge client speaks Unix domain sockets and only exists on macOS.
#[cfg(unix)]
pub mod apple;
pub mod cloud;
pub mod mock;

use async_trait::async_trait;

use crate::error::{CoreError, Result};

/// Options handed to a translation provider when it is constructed.
pub struct TranslatorOptions {
    /// BCP-47 source language; the session requires it to be explicit.
    pub source_language: String,
    /// BCP-47 target language (e.g. `zh-Hans`).
    pub target_language: String,
    /// Cloud providers: API key (Baidu: the secret key).
    pub api_key: String,
    /// Cloud providers: endpoint override, empty = provider default
    /// (OpenAI-compatible: chat-completions base).
    pub api_base: String,
    /// OpenAI-compatible: model name, empty = provider default.
    pub model: String,
    /// Baidu: APP ID.
    pub app_id: String,
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
        #[cfg(unix)]
        "apple-translate" => Ok(Box::new(apple::AppleTranslator::connect(opts).await?)),
        #[cfg(not(unix))]
        "apple-translate" => Err(CoreError::Unsupported(
            "translation provider `apple-translate` requires macOS (Swift bridge)".into(),
        )),
        "openai" | "deepl" | "google" | "baidu" => {
            let p = cloud::Provider::from_name(provider).ok_or_else(|| {
                CoreError::Unsupported(format!("unknown cloud provider `{provider}`"))
            })?;
            Ok(Box::new(cloud::CloudTranslator::connect(
                p,
                opts.source_language,
                opts.target_language,
                opts.api_key,
                opts.api_base,
                opts.model,
                opts.app_id,
            )?))
        }
        other => Err(CoreError::Unsupported(format!(
            "translation provider `{other}` is not wired up (known: `mock`, `apple-translate`,              `openai`, `deepl`, `google`, `baidu`)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_opts() -> TranslatorOptions {
        TranslatorOptions {
            source_language: "en".into(),
            target_language: "zh-Hans".into(),
            api_key: String::new(),
            api_base: String::new(),
            model: String::new(),
            app_id: String::new(),
        }
    }

    #[tokio::test]
    async fn mock_translator_marks_output() {
        let mut t = mock::MockTranslator;
        let out = t.translate("hello world").await.unwrap();
        assert_eq!(out, "[mock-zh] hello world");
        let _ = test_opts();
    }

    #[tokio::test]
    async fn unknown_provider_is_rejected() {
        let result = create("bogus", test_opts()).await;
        assert!(matches!(result, Err(CoreError::Unsupported(_))));
    }
}
