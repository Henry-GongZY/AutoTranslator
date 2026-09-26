//! Deterministic stand-in translator so the whole pipeline can be exercised
//! without the Swift bridge, language assets or network access.

use async_trait::async_trait;

use super::Translator;
use crate::error::Result;

pub struct MockTranslator;

#[async_trait]
impl Translator for MockTranslator {
    fn name(&self) -> &str {
        "mock"
    }

    async fn translate(&mut self, text: &str) -> Result<String> {
        Ok(format!("[mock-zh] {text}"))
    }
}
