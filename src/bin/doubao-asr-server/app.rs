use std::{
    convert::Infallible,
    future::Future,
    path::Path,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{DefaultBodyLimit, Path as AxumPath, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};
use futures_util::stream;
use serde::Serialize;
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::{
    audio_input::{self, MAX_JSON_BYTES},
    protocol::{self, ChatCompletionRequest},
};

#[derive(Clone)]
pub struct AppConfig {
    pub model: Arc<str>,
    pub api_key: Option<Arc<str>>,
    pub max_concurrency: usize,
    pub request_timeout: Duration,
}

pub trait Transcriber: Send + Sync {
    fn transcribe<'a>(
        &'a self,
        path: &'a Path,
    ) -> impl Future<Output = doubao_asr::Result<doubao_asr::Transcript>> + Send + 'a;
}

pub struct DoubaoTranscriber {
    client: doubao_asr::Client,
}

impl DoubaoTranscriber {
    pub fn new(client: doubao_asr::Client) -> Self {
        Self { client }
    }
}

impl Transcriber for DoubaoTranscriber {
    async fn transcribe(&self, path: &Path) -> doubao_asr::Result<doubao_asr::Transcript> {
        self.client.transcribe_file(path).await
    }
}

struct AppState<T> {
    config: AppConfig,
    transcriber: Arc<T>,
    downloader: reqwest::Client,
    concurrency: Arc<Semaphore>,
}

impl<T> Clone for AppState<T> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            transcriber: self.transcriber.clone(),
            downloader: self.downloader.clone(),
            concurrency: self.concurrency.clone(),
        }
    }
}

pub fn router<T>(config: AppConfig, transcriber: T, downloader: reqwest::Client) -> Router
where
    T: Transcriber + 'static,
{
    let concurrency = Arc::new(Semaphore::new(config.max_concurrency));
    let state = AppState {
        config,
        transcriber: Arc::new(transcriber),
        downloader,
        concurrency,
    };
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/models", get(list_models::<T>))
        .route("/v1/models/{model}", get(get_model::<T>))
        .route("/v1/chat/completions", post(chat_completions::<T>))
        .layer(DefaultBodyLimit::max(MAX_JSON_BYTES))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

async fn list_models<T>(State(state): State<AppState<T>>, headers: HeaderMap) -> Response
where
    T: Transcriber + 'static,
{
    let request_id = request_id(&headers);
    match authorize(&state.config, &headers) {
        Ok(()) => with_request_id(
            Json(json!({
                "object": "list",
                "data": [model_object(&state.config.model)]
            }))
            .into_response(),
            &request_id,
        ),
        Err(error) => with_request_id(error.into_response(), &request_id),
    }
}

async fn get_model<T>(
    State(state): State<AppState<T>>,
    headers: HeaderMap,
    AxumPath(model): AxumPath<String>,
) -> Response
where
    T: Transcriber + 'static,
{
    let request_id = request_id(&headers);
    let response = match authorize(&state.config, &headers) {
        Err(error) => error.into_response(),
        Ok(()) if model != state.config.model.as_ref() => {
            ApiError::model_not_found(&model).into_response()
        }
        Ok(()) => Json(model_object(&state.config.model)).into_response(),
    };
    with_request_id(response, &request_id)
}

async fn chat_completions<T>(
    State(state): State<AppState<T>>,
    headers: HeaderMap,
    payload: Result<Json<ChatCompletionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response
where
    T: Transcriber + 'static,
{
    let request_id = request_id(&headers);
    let response = chat_inner(state, &headers, payload, &request_id).await;
    with_request_id(response, &request_id)
}

async fn chat_inner<T>(
    state: AppState<T>,
    headers: &HeaderMap,
    payload: Result<Json<ChatCompletionRequest>, axum::extract::rejection::JsonRejection>,
    request_id: &str,
) -> Response
where
    T: Transcriber + 'static,
{
    if let Err(error) = authorize(&state.config, headers) {
        return error.into_response();
    }
    let Json(request) = match payload {
        Ok(request) => request,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return ApiError::payload_too_large().into_response()
        }
        Err(_) => {
            return ApiError::invalid_request("request body must be valid JSON", None)
                .into_response()
        }
    };
    let (source, streaming) = match protocol::extract_audio(request, &state.config.model) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let permit = match state.concurrency.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return ApiError::rate_limited().into_response(),
    };
    let started = Instant::now();
    let operation = async {
        let audio = audio_input::materialize(source, &state.downloader).await?;
        let bytes = audio.bytes;
        let source_kind = audio.source_kind;
        let transcript = state
            .transcriber
            .transcribe(audio.path())
            .await
            .map_err(ApiError::from_provider)?;
        Ok::<_, ApiError>((transcript, bytes, source_kind))
    };
    let result = tokio::time::timeout(state.config.request_timeout, operation).await;
    drop(permit);
    let (transcript, bytes, source_kind) = match result {
        Err(_) => {
            tracing::warn!(
                request_id,
                elapsed_ms = started.elapsed().as_millis(),
                "request timed out"
            );
            return ApiError::gateway_timeout().into_response();
        }
        Ok(Err(error)) => {
            tracing::warn!(
                request_id,
                status = error.status.as_u16(),
                elapsed_ms = started.elapsed().as_millis(),
                "request failed"
            );
            return error.into_response();
        }
        Ok(Ok(value)) => value,
    };
    tracing::info!(
        request_id,
        model = state.config.model.as_ref(),
        source = source_kind.as_str(),
        bytes,
        duration_ms = transcript.duration_ms,
        elapsed_ms = started.elapsed().as_millis(),
        "transcription completed"
    );

    if streaming {
        streaming_response(&state.config.model, &transcript.text)
    } else {
        completion_response(&state.config.model, &transcript.text)
    }
}

fn completion_response(model: &str, text: &str) -> Response {
    Json(json!({
        "id": format!("chatcmpl-{}", Uuid::new_v4()),
        "object": "chat.completion",
        "created": unix_seconds(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
        }]
    }))
    .into_response()
}

fn streaming_response(model: &str, text: &str) -> Response {
    let id = format!("chatcmpl-{}", Uuid::new_v4());
    let created = unix_seconds();
    let chunks = [
        json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]
        })
        .to_string(),
        json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
        })
        .to_string(),
        json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
        })
        .to_string(),
        "[DONE]".to_owned(),
    ];
    let events = chunks
        .into_iter()
        .map(|chunk| Ok::<_, Infallible>(Event::default().data(chunk)));
    Sse::new(stream::iter(events)).into_response()
}

fn model_object(model: &str) -> serde_json::Value {
    json!({"id": model, "object": "model", "created": 0, "owned_by": "doubao"})
}

fn authorize(config: &AppConfig, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = config.api_key.as_deref() else {
        return Ok(());
    };
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let valid = provided
        .map(|value| bool::from(value.as_bytes().ct_eq(expected.as_bytes())))
        .unwrap_or(false);
    if valid {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
    }
}

fn request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 128 && value.is_ascii())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("req_{}", Uuid::new_v4()))
}

fn with_request_id(mut response: Response, request_id: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    message: String,
    error_type: &'static str,
    param: Option<&'static str>,
    code: &'static str,
}

impl ApiError {
    pub fn invalid_request(message: impl Into<String>, param: Option<&'static str>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            error_type: "invalid_request_error",
            param,
            code: "invalid_request",
        }
    }

    pub fn model_not_found(model: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: format!("The model `{model}` does not exist"),
            error_type: "invalid_request_error",
            param: Some("model"),
            code: "model_not_found",
        }
    }

    pub fn payload_too_large() -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: "audio input exceeds the 25 MiB limit".into(),
            error_type: "invalid_request_error",
            param: Some("messages"),
            code: "request_too_large",
        }
    }

    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "Incorrect API key provided".into(),
            error_type: "authentication_error",
            param: None,
            code: "invalid_api_key",
        }
    }

    pub fn rate_limited() -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "the transcription concurrency limit has been reached".into(),
            error_type: "rate_limit_error",
            param: None,
            code: "rate_limit_exceeded",
        }
    }

    pub fn bad_gateway(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
            error_type: "api_error",
            param: None,
            code: "upstream_error",
        }
    }

    pub fn gateway_timeout() -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            message: "transcription timed out".into(),
            error_type: "api_error",
            param: None,
            code: "timeout",
        }
    }

    pub fn internal_io(_error: std::io::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "could not prepare the audio input".into(),
            error_type: "api_error",
            param: None,
            code: "internal_error",
        }
    }

    fn from_provider(error: doubao_asr::Error) -> Self {
        match error {
            doubao_asr::Error::NoSpeech => Self::invalid_request(
                "the input contains no recognizable speech",
                Some("messages"),
            ),
            doubao_asr::Error::Timeout => Self::gateway_timeout(),
            _ => Self::bad_gateway(error.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorBody {
            error: ErrorDetail,
        }
        #[derive(Serialize)]
        struct ErrorDetail {
            message: String,
            #[serde(rename = "type")]
            error_type: &'static str,
            param: Option<&'static str>,
            code: &'static str,
        }
        (
            self.status,
            Json(ErrorBody {
                error: ErrorDetail {
                    message: self.message,
                    error_type: self.error_type,
                    param: self.param,
                    code: self.code,
                },
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use base64::{engine::general_purpose::STANDARD, Engine};
    use tower::ServiceExt;

    struct FakeTranscriber;

    impl Transcriber for FakeTranscriber {
        async fn transcribe(&self, _path: &Path) -> doubao_asr::Result<doubao_asr::Transcript> {
            Ok(doubao_asr::Transcript {
                text: "测试文本".into(),
                duration_ms: 1000,
                frames: 50,
                provider: doubao_asr::PROVIDER.into(),
            })
        }
    }

    fn app(api_key: Option<&str>) -> Router {
        router(
            AppConfig {
                model: Arc::from("doubao-asr"),
                api_key: api_key.map(Arc::from),
                max_concurrency: 2,
                request_timeout: Duration::from_secs(5),
            },
            FakeTranscriber,
            reqwest::Client::new(),
        )
    }

    fn request(streaming: bool) -> axum::http::Request<Body> {
        let body = json!({
            "model": "doubao-asr",
            "stream": streaming,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "input_audio",
                    "input_audio": {"data": STANDARD.encode(b"RIFFdata"), "format": "wav"}
                }]
            }]
        });
        axum::http::Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn returns_standard_completion_and_delayed_sse() {
        let response = app(None).oneshot(request(false)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(value["object"], "chat.completion");
        assert_eq!(value["choices"][0]["message"]["content"], "测试文本");

        let response = app(None).oneshot(request(true)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("测试文本"));
        assert!(body.contains("data: [DONE]"));
    }

    #[tokio::test]
    async fn enforces_optional_bearer_auth_and_models() {
        let response = app(Some("secret")).oneshot(request(false)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app(None)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    struct BlockingTranscriber {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    impl Transcriber for BlockingTranscriber {
        async fn transcribe(&self, _path: &Path) -> doubao_asr::Result<doubao_asr::Transcript> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(doubao_asr::Transcript {
                text: "done".into(),
                duration_ms: 1,
                frames: 1,
                provider: doubao_asr::PROVIDER.into(),
            })
        }
    }

    #[tokio::test]
    async fn rejects_requests_above_the_concurrency_limit() {
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let app = router(
            AppConfig {
                model: Arc::from("doubao-asr"),
                api_key: None,
                max_concurrency: 1,
                request_timeout: Duration::from_secs(5),
            },
            BlockingTranscriber {
                started: started.clone(),
                release: release.clone(),
            },
            reqwest::Client::new(),
        );
        let first_app = app.clone();
        let first = tokio::spawn(async move { first_app.oneshot(request(false)).await.unwrap() });
        started.notified().await;

        let second = app.oneshot(request(false)).await.unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        release.notify_one();
        assert_eq!(first.await.unwrap().status(), StatusCode::OK);
    }
}
