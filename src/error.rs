use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Message(String),
    #[error("Doubao ASR completed but returned no text")]
    NoSpeech,
    #[error("Doubao did not provide a routable ASR device")]
    Unroutable,
    #[error("Doubao ASR rejected the session (code {0})")]
    ProviderRejected(u64),
    #[error("operation was cancelled or timed out")]
    Timeout,
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl Error {
    pub(crate) fn msg(value: impl Into<String>) -> Self {
        Self::Message(value.into())
    }
    pub(crate) fn should_refresh_credentials(&self) -> bool {
        matches!(self, Self::Unroutable | Self::ProviderRejected(_))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
