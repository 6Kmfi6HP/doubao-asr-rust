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
    time::Duration,
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
    credential_refresh_interval: Option<Duration>,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            credentials_path: None,
            punctuation: true,
            upload_speed: 1.0,
            credential_refresh_interval: None,
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
    /// Enables time-based credential refresh. Provider rejection recovery is always enabled.
    pub fn credential_refresh_interval(mut self, interval: Duration) -> Self {
        self.credential_refresh_interval = (!interval.is_zero()).then_some(interval);
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
        let credential_provider = credentials::HttpCredentialProvider::build()?;
        Ok(Client {
            inner: Arc::new(Inner {
                options: Options {
                    credentials_path: path,
                    punctuation: self.punctuation,
                    speed: self.upload_speed,
                    credential_refresh_interval: self.credential_refresh_interval,
                },
                credential_state: Mutex::new(CredentialState::default()),
                refresh: Mutex::new(()),
                credential_provider,
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
    credential_provider: Arc<dyn credentials::CredentialProvider>,
}
struct Options {
    credentials_path: PathBuf,
    punctuation: bool,
    speed: f64,
    credential_refresh_interval: Option<Duration>,
}
#[derive(Default)]
struct CredentialState {
    loaded: bool,
    credentials: Option<Arc<credentials::Credentials>>,
    dirty: bool,
    refresh_failures: usize,
    retry_after: u64,
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
        // Proactive maintenance must never make a request fail while the old
        // credential may still be accepted by the provider.
        let _ = self.refresh_credentials_if_due().await;
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
            Err(error) if error.should_refresh_credentials() => {
                self.refresh_and_transcribe(&path, credentials).await
            }
            Err(error) => Err(error),
        }
    }

    /// Refreshes the current token when its configured deadline has elapsed.
    ///
    /// Returns `true` when a fresh token was installed. A disabled interval,
    /// a future deadline, or an active failure backoff returns `false`.
    pub async fn refresh_credentials_if_due(&self) -> Result<bool> {
        let Some(interval) = self.inner.options.credential_refresh_interval else {
            return Ok(false);
        };
        let _refresh = self.inner.refresh.lock().await;
        let current = self.credentials_for_use().await?;
        let now = credentials::unix_seconds();
        {
            let state = self.inner.credential_state.lock().await;
            if state.retry_after > now || current.refresh_due_at(interval) > now {
                return Ok(false);
            }
        }

        match self.inner.credential_provider.refresh(&current).await {
            Ok(candidate) => {
                let candidate = Arc::new(candidate);
                let mut state = self.inner.credential_state.lock().await;
                if !state
                    .credentials
                    .as_ref()
                    .is_some_and(|value| Arc::ptr_eq(value, &current))
                {
                    return Ok(false);
                }
                state.credentials = Some(candidate.clone());
                state.dirty = true;
                state.refresh_failures = 0;
                state.retry_after = 0;
                drop(state);
                self.persist_credentials(&candidate).await?;
                Ok(true)
            }
            Err(error) => {
                let mut state = self.inner.credential_state.lock().await;
                state.refresh_failures = state.refresh_failures.saturating_add(1);
                state.retry_after = now.saturating_add(refresh_backoff(state.refresh_failures));
                Err(error)
            }
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
            state.credentials = Some(Arc::new(self.inner.credential_provider.register().await?));
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
                Err(error) if error.should_refresh_credentials() => {}
                Err(error) => return Err(error),
            }
        }
        if let Ok(candidate) = self.inner.credential_provider.refresh(&current).await {
            let candidate = Arc::new(candidate);
            match provider::transcribe_file(
                path,
                &candidate,
                self.inner.options.punctuation,
                self.inner.options.speed,
            )
            .await
            {
                Ok(result) => {
                    self.activate_credentials(candidate.clone()).await?;
                    return public_transcript(result);
                }
                Err(error) if error.should_refresh_credentials() => {}
                Err(error) => return Err(error),
            }
        }

        let candidate = Arc::new(self.inner.credential_provider.register().await?);
        match provider::transcribe_file(
            path,
            &candidate,
            self.inner.options.punctuation,
            self.inner.options.speed,
        )
        .await
        {
            Ok(result) => {
                self.activate_credentials(candidate.clone()).await?;
                public_transcript(result)
            }
            Err(error) => Err(error),
        }
    }

    async fn activate_credentials(&self, candidate: Arc<credentials::Credentials>) -> Result<()> {
        let mut state = self.inner.credential_state.lock().await;
        state.credentials = Some(candidate.clone());
        state.loaded = true;
        state.dirty = true;
        state.refresh_failures = 0;
        state.retry_after = 0;
        drop(state);
        self.persist_credentials(&candidate).await
    }
}

fn refresh_backoff(failures: usize) -> u64 {
    const BACKOFFS: [u64; 4] = [60, 5 * 60, 15 * 60, 60 * 60];
    BACKOFFS[failures.saturating_sub(1).min(BACKOFFS.len() - 1)]
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct FakeCredentialProvider {
        refreshes: AtomicUsize,
        registrations: AtomicUsize,
        fail_refresh: AtomicBool,
    }

    impl FakeCredentialProvider {
        fn new(fail_refresh: bool) -> Self {
            Self {
                refreshes: AtomicUsize::new(0),
                registrations: AtomicUsize::new(0),
                fail_refresh: AtomicBool::new(fail_refresh),
            }
        }
    }

    impl credentials::CredentialProvider for FakeCredentialProvider {
        fn register(&self) -> credentials::CredentialFuture<'_> {
            self.registrations.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(Error::msg("unexpected registration")) })
        }

        fn refresh<'a>(
            &'a self,
            current: &'a credentials::Credentials,
        ) -> credentials::CredentialFuture<'a> {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if self.fail_refresh.load(Ordering::SeqCst) {
                    return Err(Error::msg("refresh failed"));
                }
                let mut candidate = current.clone();
                candidate.token = "refreshed-token".into();
                candidate.refreshed_at = credentials::unix_seconds();
                Ok(candidate)
            })
        }
    }

    fn client_with_provider(
        path: PathBuf,
        interval: Option<Duration>,
        provider: Arc<dyn credentials::CredentialProvider>,
    ) -> Client {
        Client {
            inner: Arc::new(Inner {
                options: Options {
                    credentials_path: path,
                    punctuation: true,
                    speed: 1.0,
                    credential_refresh_interval: interval,
                },
                credential_state: Mutex::new(CredentialState::default()),
                refresh: Mutex::new(()),
                credential_provider: provider,
            }),
        }
    }

    async fn write_legacy_credentials(path: &Path) {
        tokio::fs::write(
            path,
            br#"{"device_id":"1","install_id":"2","cdid":"c","openudid":"o","clientudid":"u","token":"old-token"}"#,
        )
        .await
        .unwrap();
    }
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

    #[tokio::test]
    async fn legacy_credentials_refresh_once_across_concurrent_callers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.json");
        write_legacy_credentials(&path).await;
        let provider = Arc::new(FakeCredentialProvider::new(false));
        let client = client_with_provider(
            path.clone(),
            Some(Duration::from_secs(6 * 60 * 60)),
            provider.clone(),
        );

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let client = client.clone();
            tasks.push(tokio::spawn(async move {
                client.refresh_credentials_if_due().await.unwrap()
            }));
        }
        let mut refreshed = 0;
        for task in tasks {
            refreshed += usize::from(task.await.unwrap());
        }

        assert_eq!(refreshed, 1);
        assert_eq!(provider.refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(provider.registrations.load(Ordering::SeqCst), 0);
        let stored = credentials::load(&path).await.unwrap().unwrap();
        assert_eq!(stored.token, "refreshed-token");
        assert!(stored.refreshed_at > 0);
    }

    #[tokio::test]
    async fn refresh_failure_retains_credentials_and_starts_backoff() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.json");
        write_legacy_credentials(&path).await;
        let provider = Arc::new(FakeCredentialProvider::new(true));
        let client = client_with_provider(
            path.clone(),
            Some(Duration::from_secs(6 * 60 * 60)),
            provider.clone(),
        );

        assert!(client.refresh_credentials_if_due().await.is_err());
        assert!(!client.refresh_credentials_if_due().await.unwrap());
        assert_eq!(provider.refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(provider.registrations.load(Ordering::SeqCst), 0);
        assert_eq!(
            credentials::load(&path).await.unwrap().unwrap().token,
            "old-token"
        );
    }

    #[tokio::test]
    async fn disabled_proactive_refresh_does_not_load_or_refresh_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let provider = Arc::new(FakeCredentialProvider::new(false));
        let client = client_with_provider(
            directory.path().join("missing.json"),
            None,
            provider.clone(),
        );

        assert!(!client.refresh_credentials_if_due().await.unwrap());
        assert_eq!(provider.refreshes.load(Ordering::SeqCst), 0);
        assert_eq!(provider.registrations.load(Ordering::SeqCst), 0);
    }
}
