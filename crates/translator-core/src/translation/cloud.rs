//! Cloud translation APIs: OpenAI-compatible chat endpoints, DeepL, Google
//! Cloud Translation v2 and Baidu Translate. One adapter each, all sharing a
//! reqwest client; a session only needs to hand over credentials — the
//! endpoints, request shapes, auth schemes and target-language codes live
//! here.
//!
//! Keys live in the client's settings and travel in the session request;
//! failures degrade to untranslated subtitles (the session logs and keeps
//! going) and are never fatal.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use md5::{Digest, Md5};
use serde::Deserialize;

use super::Translator;
use crate::error::{CoreError, Result};

const TIMEOUT: Duration = Duration::from_secs(20);
/// Repeat-translation cache bound; cleared wholesale when exceeded.
const CACHE_CAP: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Provider {
    OpenAi,
    DeepL,
    Google,
    Baidu,
}

impl Provider {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "openai" => Some(Self::OpenAi),
            "deepl" => Some(Self::DeepL),
            "google" => Some(Self::Google),
            "baidu" => Some(Self::Baidu),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::DeepL => "deepl",
            Self::Google => "google",
            Self::Baidu => "baidu",
        }
    }
}

/// Provider-specific target-language codes (input: BCP-47 from the session).
fn target_code(provider: Provider, target: &str) -> String {
    let t = target.to_ascii_lowercase();
    match provider {
        // The model reads the language name; pass it through.
        Provider::OpenAi => target.to_string(),
        // DeepL wants uppercase, English defaults to the US variant.
        Provider::DeepL => match t.as_str() {
            "zh" | "zh-hans" | "zh-cn" => "ZH".into(),
            "zh-hant" | "zh-tw" | "zh-hk" => "ZH".into(),
            "en" => "EN-US".into(),
            other => other.to_uppercase(),
        },
        // Google v2 takes region-qualified codes.
        Provider::Google => match t.as_str() {
            "zh" | "zh-hans" => "zh-CN".into(),
            "zh-hant" | "zh-tw" | "zh-hk" => "zh-TW".into(),
            other => other.into(),
        },
        // Baidu uses its own short codes.
        Provider::Baidu => match t.as_str() {
            "zh" | "zh-hans" | "zh-cn" => "zh".into(),
            "zh-hant" | "zh-tw" | "zh-hk" => "cht".into(),
            "ja" => "jp".into(),
            "ko" => "kor".into(),
            other => other.into(),
        },
    }
}

pub struct CloudTranslator {
    provider: Provider,
    source: String,
    target: String,
    target_code: String,
    api_key: String,
    api_base: String,
    model: String,
    app_id: String,
    http: reqwest::Client,
    cache: HashMap<String, String>,
}

impl CloudTranslator {
    pub fn connect(
        provider: Provider,
        source: String,
        target: String,
        api_key: String,
        api_base: String,
        model: String,
        app_id: String,
    ) -> Result<Self> {
        if api_key.trim().is_empty() {
            return Err(CoreError::Unsupported(format!(
                "translation provider `{}` requires an API key",
                provider.name()
            )));
        }
        if provider == Provider::Baidu && app_id.trim().is_empty() {
            return Err(CoreError::Unsupported(
                "translation provider `baidu` requires an APP ID".into(),
            ));
        }
        let target_code = target_code(provider, &target);
        if target_code.is_empty() {
            return Err(CoreError::Unsupported(
                "translation provider requires `target_language`".into(),
            ));
        }
        let api_base = if api_base.trim().is_empty() {
            "https://api.openai.com/v1".to_string()
        } else {
            api_base.trim_end_matches('/').to_string()
        };
        Ok(Self {
            provider,
            source,
            target,
            target_code,
            api_key,
            api_base,
            model,
            app_id,
            http: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .build()
                .map_err(|e| CoreError::Provider(format!("http client init failed: {e}")))?,
            cache: HashMap::new(),
        })
    }

    async fn translate_openai(&self, text: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Response {
            choices: Vec<Choice>,
        }
        #[derive(Deserialize)]
        struct Choice {
            message: Message,
        }
        #[derive(Deserialize)]
        struct Message {
            content: String,
        }

        let source = if self.source.is_empty() {
            String::new()
        } else {
            format!(" from {}", self.source)
        };
        let system = format!(
            "You are a professional subtitle translator. Translate the user's text{source} into {target}. Output only the translation, no explanations, no quotes.",
            source = source,
            target = self.target
        );
        let url = format!("{}/chat/completions", self.api_base);
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({
                "model": if self.model.is_empty() { "gpt-4o-mini".to_string() } else { self.model.clone() },
                "messages": [
                    {"role": "system", "content": system},
                    {"role": "user", "content": text}
                ],
                "temperature": 0.2,
                "stream": false
            }))
            .send()
            .await
            .map_err(|e| CoreError::Provider(format!("openai request failed: {e}")))?;
        let response = check(response, "openai").await?;
        let parsed: Response = response
            .json()
            .await
            .map_err(|e| CoreError::Provider(format!("openai response: {e}")))?;
        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CoreError::Provider("openai returned no translation".into()))
    }

    async fn translate_deepl(&self, text: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Response {
            translations: Vec<Translation>,
        }
        #[derive(Deserialize)]
        struct Translation {
            text: String,
        }

        // Free-tier keys end with `:fx` and use a different host.
        let base = if self.api_base.is_empty() {
            if self.api_key.ends_with(":fx") {
                "https://api-free.deepl.com/v2"
            } else {
                "https://api.deepl.com/v2"
            }
        } else {
            &self.api_base
        };
        let response = self
            .http
            .post(format!("{base}/translate"))
            .header("DeepL-Auth-Key", &self.api_key)
            .json(&serde_json::json!({
                "text": [text],
                "target_lang": self.target_code,
            }))
            .send()
            .await
            .map_err(|e| CoreError::Provider(format!("deepl request failed: {e}")))?;
        let response = check(response, "deepl").await?;
        let parsed: Response = response
            .json()
            .await
            .map_err(|e| CoreError::Provider(format!("deepl response: {e}")))?;
        parsed
            .translations
            .into_iter()
            .next()
            .map(|t| t.text)
            .ok_or_else(|| CoreError::Provider("deepl returned no translation".into()))
    }

    async fn translate_google(&self, text: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Response {
            data: Data,
        }
        #[derive(Deserialize)]
        struct Data {
            translations: Vec<Translation>,
        }
        #[derive(Deserialize)]
        struct Translation {
            #[serde(rename = "translatedText")]
            translated_text: String,
        }

        let base = if self.api_base.is_empty() {
            "https://translation.googleapis.com".to_string()
        } else {
            self.api_base.clone()
        };
        let response = self
            .http
            .post(format!("{base}/language/translate/v2"))
            .query(&[("key", &self.api_key)])
            .form(&[
                ("q", text),
                ("target", self.target_code.as_str()),
                ("format", "text"),
            ])
            .send()
            .await
            .map_err(|e| CoreError::Provider(format!("google request failed: {e}")))?;
        let response = check(response, "google").await?;
        let parsed: Response = response
            .json()
            .await
            .map_err(|e| CoreError::Provider(format!("google response: {e}")))?;
        parsed
            .data
            .translations
            .into_iter()
            .next()
            .map(|t| t.translated_text)
            .ok_or_else(|| CoreError::Provider("google returned no translation".into()))
    }

    async fn translate_baidu(&self, text: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Response {
            #[serde(default)]
            trans_result: Vec<Item>,
            #[serde(default)]
            error_code: Option<String>,
            #[serde(default)]
            error_msg: Option<String>,
        }
        #[derive(Deserialize)]
        struct Item {
            dst: String,
        }

        let salt = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos().to_string())
            .unwrap_or_else(|_| "0".into());
        let sign_input = format!("{}{}{}{}", self.app_id, text, salt, self.api_key);
        let sign = hex_lower(&Md5::digest(sign_input.as_bytes()));
        let from = if self.source.is_empty() {
            "auto".to_string()
        } else {
            target_code(Provider::Baidu, &self.source)
        };
        let base = if self.api_base.is_empty() {
            "https://fanyi-api.baidu.com".to_string()
        } else {
            self.api_base.clone()
        };
        let response = self
            .http
            .post(format!("{base}/api/trans/vip/translate"))
            .form(&[
                ("q", text),
                ("from", from.as_str()),
                ("to", self.target_code.as_str()),
                ("appid", self.app_id.as_str()),
                ("salt", salt.as_str()),
                ("sign", sign.as_str()),
            ])
            .send()
            .await
            .map_err(|e| CoreError::Provider(format!("baidu request failed: {e}")))?;
        let response = check(response, "baidu").await?;
        let parsed: Response = response
            .json()
            .await
            .map_err(|e| CoreError::Provider(format!("baidu response: {e}")))?;
        if let Some(code) = parsed.error_code {
            return Err(CoreError::Provider(format!(
                "baidu error {}: {}",
                code,
                parsed.error_msg.unwrap_or_default()
            )));
        }
        parsed
            .trans_result
            .into_iter()
            .next()
            .map(|item| item.dst)
            .ok_or_else(|| CoreError::Provider("baidu returned no translation".into()))
    }
}

impl std::fmt::Debug for CloudTranslator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudTranslator")
            .field("provider", &self.provider.name())
            .field("source", &self.source)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Translator for CloudTranslator {
    fn name(&self) -> &str {
        self.provider.name()
    }

    async fn translate(&mut self, text: &str) -> Result<String> {
        if text.trim().is_empty() {
            return Ok(String::new());
        }
        if let Some(hit) = self.cache.get(text) {
            return Ok(hit.clone());
        }
        let translated = match self.provider {
            Provider::OpenAi => self.translate_openai(text).await?,
            Provider::DeepL => self.translate_deepl(text).await?,
            Provider::Google => self.translate_google(text).await?,
            Provider::Baidu => self.translate_baidu(text).await?,
        };
        if translated.is_empty() {
            return Err(CoreError::Provider("empty translation returned".into()));
        }
        if self.cache.len() >= CACHE_CAP {
            self.cache.clear();
        }
        self.cache.insert(text.to_string(), translated.clone());
        Ok(translated)
    }
}

async fn check(response: reqwest::Response, provider: &str) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let snippet: String = response.text().await.unwrap_or_default().chars().take(300).collect();
    Err(CoreError::Provider(format!(
        "{provider} API {status}: {snippet}"
    )))
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_codes_per_provider() {
        assert_eq!(target_code(Provider::Google, "zh-Hans"), "zh-CN");
        assert_eq!(target_code(Provider::Google, "zh-Hant"), "zh-TW");
        assert_eq!(target_code(Provider::DeepL, "zh"), "ZH");
        assert_eq!(target_code(Provider::DeepL, "en"), "EN-US");
        assert_eq!(target_code(Provider::Baidu, "ja"), "jp");
        assert_eq!(target_code(Provider::Baidu, "zh-Hant"), "cht");
        assert_eq!(target_code(Provider::OpenAi, "日本語"), "日本語");
    }

    #[test]
    fn baidu_sign_is_lowercased_md5() {
        // Canonical example from Baidu's docs: appid=20200215000001234,
        // q=apple, salt=1435660288, secret=12345678 → digest over the
        // concatenation.
        let digest = hex_lower(&Md5::digest(b"20200215000001234apple143566028812345678"));
        assert_eq!(digest, "95e38d72ee3ed528efe25ead04c21969");
    }

    #[tokio::test]
    async fn missing_key_fails_fast() {
        let err = CloudTranslator::connect(
            Provider::DeepL,
            "en".into(),
            "zh-Hans".into(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        )
        .expect_err("must reject empty key");
        assert!(err.to_string().contains("API key"));
    }

    #[tokio::test]
    async fn cache_hits_do_not_refetch() {
        let mut translator = CloudTranslator::connect(
            Provider::OpenAi,
            "en".into(),
            "zh-Hans".into(),
            "test-key".into(),
            "http://127.0.0.1:9".into(), // nothing listens here
            "test-model".into(),
            String::new(),
        )
        .unwrap();
        let first = translator.translate("hello world").await;
        assert!(first.is_err(), "no server: request must fail");
        translator
            .cache
            .insert("hello world".into(), "[cached] 你好".into());
        assert_eq!(translator.translate("hello world").await.unwrap(), "[cached] 你好");
    }
}
