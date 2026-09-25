//! Error type shared by the whole core.

use translator_protocol::ErrorCode;

pub type Result<T> = std::result::Result<T, CoreError>;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("invalid audio format: {0}")]
    AudioFormat(String),

    #[error("invalid session: {0}")]
    InvalidSession(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("provider failure: {0}")]
    Provider(String),

    #[error("model error: {0}")]
    Model(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl CoreError {
    /// Map to the wire-level error code sent back to the client.
    pub fn code(&self) -> ErrorCode {
        match self {
            CoreError::Io(_) => ErrorCode::Internal,
            CoreError::Unsupported(_) => ErrorCode::Unsupported,
            CoreError::AudioFormat(_) => ErrorCode::AudioFormat,
            CoreError::InvalidSession(_) => ErrorCode::InvalidSession,
            CoreError::BadRequest(_) => ErrorCode::BadRequest,
            CoreError::Provider(_) => ErrorCode::ProviderFailure,
            CoreError::Model(_) => ErrorCode::Internal,
            CoreError::Internal(_) => ErrorCode::Internal,
        }
    }
}
