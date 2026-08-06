//! Unofficial asynchronous Rust SDK for Doubao IME speech recognition.
//!
//! The client converts the first audio stream with `ffmpeg`, uploads it to
//! Doubao, and manages anonymous device credentials. The service endpoints are
//! undocumented and may change without notice.

mod audio;
mod config;
mod credentials;
mod error;
mod provider;
mod sami;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;

pub use error::{Error, Result};

/// Provider identifier returned in every successful transcript.
pub const PROVIDER: &str = "doubao-ime";

/// A completed transcription.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Transcript {
    pub text: String,
    pub duration_ms: u64,
    pub frames: usize,
    pub provider: String,
}

/// Configures and constructs a [`Client`].
#[derive(Debug, Clone)]
pub struct ClientBuilder {
    credentials_path: Option<PathBuf>,
    punctuation: bool,
    upload_speed: f64,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            credentials_path: None,
            punctuation: true,
            upload_speed: 1.0,
        }
    }
}

impl ClientBuilder {
    /// Overrides the OS-specific credential location.
    pub fn credentials_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.credentials_path = Some(path.into());
        self
    }
    /// Enables or disables provider punctuation (enabled by default).
    pub fn punctuation(mut self, enabled: bool) -> Self {
        self.punctuation = enabled;
        self
    }
    /// Sets upload pacing (`0 < speed <= 4`; default `1`).
    pub fn upload_speed(mut self, speed: f64) -> Self {
        self.upload_speed = speed;
        self
    }
    /// Validates options and builds a reusable, concurrency-safe client. No network I/O occurs.
    pub fn build(self) -> Result<Client> {
        if !(self.upload_speed > 0.0 && self.upload_speed <= 4.0) {
            return Err(Error::msg(
                "upload speed must be greater than 0 and no more than 4",
            ));
        }
        let path = match self.credentials_path {
            Some(path) if path.as_os_str().is_empty() => {
                return Err(Error::msg("credential path must not be empty"))
            }
            Some(path) => absolute_path(&path)?,
            None => credentials::default_path()?,
        };
        Ok(Client {
            inner: Arc::new(Inner {
                options: Options {
                    credentials_path: path,
                    punctuation: self.punctuation,
                    speed: self.upload_speed,
                },
                credential_state: Mutex::new(CredentialState::default()),
                refresh: Mutex::new(()),
            }),
        })
    }
}

#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}
struct Inner {
    options: Options,
    credential_state: Mutex<CredentialState>,
    refresh: Mutex<()>,
}
struct Options {
    credentials_path: PathBuf,
    punctuation: bool,
    speed: f64,
}
#[derive(Default)]
struct CredentialState {
    loaded: bool,
    credentials: Option<Arc<credentials::Credentials>>,
    dirty: bool,
}

impl Client {
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }
    pub fn new() -> Result<Self> {
        Self::builder().build()
    }

    /// Transcribes the first audio stream of a local media file.
    pub async fn transcribe_file(&self, input: impl AsRef<Path>) -> Result<Transcript> {
        let path = validate_input(input.as_ref()).await?;
        let credentials = self.credentials_for_use().await?;
        match provider::transcribe_file(
            &path,
            &credentials,
            self.inner.options.punctuation,
            self.inner.options.speed,
        )
        .await
        {
            Ok(result) => {
                self.persist_credentials(&credentials).await?;
                public_transcript(result)
            }
            Err(error) if error.is_unroutable() => {
                self.refresh_and_transcribe(&path, credentials).await
            }
            Err(error) => Err(error),
        }
    }

    async fn credentials_for_use(&self) -> Result<Arc<credentials::Credentials>> {
        let mut state = self.inner.credential_state.lock().await;
        if !state.loaded {
            state.credentials = credentials::load(&self.inner.options.credentials_path)
                .await?
                .map(Arc::new);
            state.loaded = true;
        }
        if state.credentials.is_none() {
            state.credentials = Some(Arc::new(credentials::register().await?));
            state.dirty = true;
        }
        Ok(state.credentials.as_ref().unwrap().clone())
    }

    async fn persist_credentials(&self, candidate: &Arc<credentials::Credentials>) -> Result<()> {
        let mut state = self.inner.credential_state.lock().await;
        let is_current = state
            .credentials
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(c, candidate));
        if is_current && state.dirty {
            credentials::save(&self.inner.options.credentials_path, candidate).await?;
            state.dirty = false;
        }
        Ok(())
    }

    async fn refresh_and_transcribe(
        &self,
        path: &Path,
        failed: Arc<credentials::Credentials>,
    ) -> Result<Transcript> {
        let _refresh = self.inner.refresh.lock().await;
        let current = self.credentials_for_use().await?;
        if !Arc::ptr_eq(&current, &failed) {
            match provider::transcribe_file(
                path,
                &current,
                self.inner.options.punctuation,
                self.inner.options.speed,
            )
            .await
            {
                Ok(result) => {
                    self.persist_credentials(&current).await?;
                    return public_transcript(result);
                }
                Err(error) if error.is_unroutable() => {}
                Err(error) => return Err(error),
            }
        }
        let mut last = Error::Unroutable;
        for _ in 0..2 {
            let candidate = Arc::new(credentials::register().await?);
            match provider::transcribe_file(
                path,
                &candidate,
                self.inner.options.punctuation,
                self.inner.options.speed,
            )
            .await
            {
                Ok(result) => {
                    let mut state = self.inner.credential_state.lock().await;
                    state.credentials = Some(candidate.clone());
                    state.loaded = true;
                    state.dirty = true;
                    drop(state);
                    self.persist_credentials(&candidate).await?;
                    return public_transcript(result);
                }
                Err(error) => {
                    let stop = !error.is_unroutable();
                    last = error;
                    if stop {
                        break;
                    }
                }
            }
        }
        Err(last)
    }
}

fn public_transcript(result: provider::ProviderTranscript) -> Result<Transcript> {
    if result.text.is_empty() {
        return Err(Error::NoSpeech);
    }
    Ok(Transcript {
        text: result.text,
        duration_ms: result.duration_ms,
        frames: result.frames,
        provider: PROVIDER.into(),
    })
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|p| p.join(path))
            .map_err(|e| Error::msg(format!("resolve path: {e}")))
    }
}
async fn validate_input(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(Error::msg("input path must not be empty"));
    }
    let absolute = absolute_path(path)?;
    let metadata = tokio::fs::metadata(&absolute)
        .await
        .map_err(|e| Error::msg(format!("read input file: {e}")))?;
    if !metadata.is_file() {
        return Err(Error::msg("input must be a regular local file"));
    }
    Ok(absolute)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn builder_defaults_and_validation() {
        let c = Client::builder()
            .credentials_path("test.json")
            .build()
            .unwrap();
        assert!(c.inner.options.punctuation);
        assert_eq!(c.inner.options.speed, 1.0);
        assert!(Client::builder().upload_speed(0.0).build().is_err());
        assert!(Client::builder().upload_speed(4.1).build().is_err());
        assert!(Client::builder().credentials_path("").build().is_err());
    }
    #[tokio::test]
    async fn rejects_directory_input_before_network() {
        let dir = tempfile::tempdir().unwrap();
        let client = Client::builder()
            .credentials_path(dir.path().join("c.json"))
            .build()
            .unwrap();
        assert!(client
            .transcribe_file(dir.path())
            .await
            .unwrap_err()
            .to_string()
            .contains("regular local file"));
    }
    #[test]
    fn empty_text_is_no_speech() {
        let err = public_transcript(provider::ProviderTranscript {
            text: String::new(),
            duration_ms: 0,
            frames: 0,
        })
        .unwrap_err();
        assert!(matches!(err, Error::NoSpeech));
    }
}
