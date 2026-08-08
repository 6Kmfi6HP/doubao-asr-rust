use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use url::Url;

use super::{app::ApiError, protocol::AudioSource};

pub const MAX_AUDIO_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_JSON_BYTES: usize = 36 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub enum SourceKind {
    InputAudio,
    DataUrl,
    RemoteUrl,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InputAudio => "input_audio",
            Self::DataUrl => "data_url",
            Self::RemoteUrl => "remote_url",
        }
    }
}

pub struct CanonicalAudio {
    _directory: tempfile::TempDir,
    path: PathBuf,
    pub bytes: usize,
    pub source_kind: SourceKind,
}

impl CanonicalAudio {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub async fn materialize(
    source: AudioSource,
    downloader: &reqwest::Client,
) -> Result<CanonicalAudio, ApiError> {
    match source {
        AudioSource::InputAudio { data, format } => {
            let format = format.to_ascii_lowercase();
            if format != "wav" && format != "mp3" {
                return Err(ApiError::invalid_request(
                    "input_audio format must be wav or mp3",
                    Some("messages"),
                ));
            }
            let bytes = decode_base64(&data)?;
            write_bytes(bytes, &format, SourceKind::InputAudio).await
        }
        AudioSource::AudioUrl(value) if value.starts_with("data:") => {
            let (metadata, data) = value.split_once(',').ok_or_else(|| {
                ApiError::invalid_request("invalid audio data URL", Some("messages"))
            })?;
            if !metadata.ends_with(";base64") {
                return Err(ApiError::invalid_request(
                    "audio data URL must use base64 encoding",
                    Some("messages"),
                ));
            }
            let mime = metadata
                .strip_prefix("data:")
                .and_then(|value| value.strip_suffix(";base64"))
                .unwrap_or_default();
            if !mime.starts_with("audio/") {
                return Err(ApiError::invalid_request(
                    "audio data URL must use an audio MIME type",
                    Some("messages"),
                ));
            }
            let extension = extension_for_mime(mime);
            let bytes = decode_base64(data)?;
            write_bytes(bytes, extension, SourceKind::DataUrl).await
        }
        AudioSource::AudioUrl(value) => download(value, downloader).await,
    }
}

fn decode_base64(value: &str) -> Result<Vec<u8>, ApiError> {
    let maximum_encoded = MAX_AUDIO_BYTES.saturating_mul(4) / 3 + 4;
    if value.len() > maximum_encoded {
        return Err(ApiError::payload_too_large());
    }
    let decoded = STANDARD.decode(value).map_err(|_| {
        ApiError::invalid_request("audio data is not valid base64", Some("messages"))
    })?;
    if decoded.len() > MAX_AUDIO_BYTES {
        return Err(ApiError::payload_too_large());
    }
    if decoded.is_empty() {
        return Err(ApiError::invalid_request(
            "audio input must not be empty",
            Some("messages"),
        ));
    }
    Ok(decoded)
}

async fn write_bytes(
    bytes: Vec<u8>,
    extension: &str,
    source_kind: SourceKind,
) -> Result<CanonicalAudio, ApiError> {
    let directory = tempfile::tempdir().map_err(ApiError::internal_io)?;
    let path = directory.path().join(format!("input.{extension}"));
    tokio::fs::write(&path, &bytes)
        .await
        .map_err(ApiError::internal_io)?;
    Ok(CanonicalAudio {
        _directory: directory,
        path,
        bytes: bytes.len(),
        source_kind,
    })
}

async fn download(value: String, downloader: &reqwest::Client) -> Result<CanonicalAudio, ApiError> {
    let url = Url::parse(&value).map_err(|_| {
        ApiError::invalid_request("audio_url must be an HTTP(S) or data URL", Some("messages"))
    })?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(ApiError::invalid_request(
            "audio_url must be an HTTP(S) or data URL",
            Some("messages"),
        ));
    }
    let response = downloader
        .get(url)
        .send()
        .await
        .map_err(|_| ApiError::bad_gateway("could not download audio_url"))?;
    if !response.status().is_success() {
        return Err(ApiError::bad_gateway(
            "audio_url returned a non-success status",
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_AUDIO_BYTES as u64)
    {
        return Err(ApiError::payload_too_large());
    }

    let directory = tempfile::tempdir().map_err(ApiError::internal_io)?;
    let path = directory.path().join("input.media");
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(ApiError::internal_io)?;
    let mut bytes = 0usize;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ApiError::bad_gateway("audio_url download failed"))?;
        bytes = bytes
            .checked_add(chunk.len())
            .ok_or_else(ApiError::payload_too_large)?;
        if bytes > MAX_AUDIO_BYTES {
            return Err(ApiError::payload_too_large());
        }
        file.write_all(&chunk)
            .await
            .map_err(ApiError::internal_io)?;
    }
    file.flush().await.map_err(ApiError::internal_io)?;
    if bytes == 0 {
        return Err(ApiError::invalid_request(
            "audio input must not be empty",
            Some("messages"),
        ));
    }
    Ok(CanonicalAudio {
        _directory: directory,
        path,
        bytes,
        source_kind: SourceKind::RemoteUrl,
    })
}

fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/flac" => "flac",
        "audio/ogg" => "ogg",
        "audio/webm" => "webm",
        "audio/mp4" => "m4a",
        _ => "media",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        http::{header, HeaderValue, StatusCode},
        response::{IntoResponse, Redirect},
        routing::get,
        Router,
    };
    use std::time::Duration;

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    #[tokio::test]
    async fn materializes_input_audio_and_data_urls() {
        let audio = materialize(
            AudioSource::InputAudio {
                data: STANDARD.encode(b"RIFFdata"),
                format: "wav".into(),
            },
            &client(),
        )
        .await
        .unwrap();
        assert_eq!(tokio::fs::read(audio.path()).await.unwrap(), b"RIFFdata");
        assert_eq!(audio.source_kind.as_str(), "input_audio");

        let audio = materialize(
            AudioSource::AudioUrl(format!(
                "data:audio/mpeg;base64,{}",
                STANDARD.encode(b"ID3data")
            )),
            &client(),
        )
        .await
        .unwrap();
        assert_eq!(audio.bytes, 7);
        assert_eq!(audio.source_kind.as_str(), "data_url");
    }

    #[tokio::test]
    async fn rejects_invalid_encodings_formats_and_schemes() {
        assert!(materialize(
            AudioSource::InputAudio {
                data: "not base64".into(),
                format: "wav".into()
            },
            &client()
        )
        .await
        .is_err());
        assert!(materialize(
            AudioSource::InputAudio {
                data: STANDARD.encode(b"x"),
                format: "flac".into()
            },
            &client()
        )
        .await
        .is_err());
        assert!(materialize(
            AudioSource::AudioUrl("file:///etc/passwd".into()),
            &client()
        )
        .await
        .is_err());
    }

    async fn fixture_server() -> std::net::SocketAddr {
        async fn audio() -> &'static [u8] {
            b"RIFFremote"
        }
        async fn redirect() -> Redirect {
            Redirect::temporary("/audio")
        }
        async fn large() -> impl IntoResponse {
            let mut response = StatusCode::OK.into_response();
            response.headers_mut().insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&(MAX_AUDIO_BYTES + 1).to_string()).unwrap(),
            );
            response
        }
        async fn slow() -> &'static [u8] {
            tokio::time::sleep(Duration::from_millis(200)).await;
            b"slow"
        }
        let app = Router::new()
            .route("/audio", get(audio))
            .route("/redirect", get(redirect))
            .route("/missing", get(|| async { StatusCode::NOT_FOUND }))
            .route("/large", get(large))
            .route("/slow", get(slow));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        address
    }

    #[tokio::test]
    async fn downloads_redirects_and_rejects_remote_failures() {
        let address = fixture_server().await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .unwrap();
        let audio = materialize(
            AudioSource::AudioUrl(format!("http://{address}/redirect")),
            &client,
        )
        .await
        .unwrap();
        assert_eq!(tokio::fs::read(audio.path()).await.unwrap(), b"RIFFremote");
        assert_eq!(audio.source_kind.as_str(), "remote_url");

        for path in ["missing", "large"] {
            assert!(materialize(
                AudioSource::AudioUrl(format!("http://{address}/{path}")),
                &client
            )
            .await
            .is_err());
        }

        let timeout_client = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        assert!(materialize(
            AudioSource::AudioUrl(format!("http://{address}/slow")),
            &timeout_client
        )
        .await
        .is_err());
    }
}
