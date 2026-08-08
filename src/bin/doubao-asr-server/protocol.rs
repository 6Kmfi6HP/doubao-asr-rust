use serde::Deserialize;

use super::app::ApiError;

#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub stream: bool,
}

#[derive(Debug, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: MessageContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "input_audio")]
    InputAudio { input_audio: InputAudio },
    #[serde(rename = "audio_url")]
    AudioUrl { audio_url: AudioUrl },
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Deserialize)]
pub struct InputAudio {
    pub data: String,
    pub format: String,
}

#[derive(Debug, Deserialize)]
pub struct AudioUrl {
    pub url: String,
}

#[derive(Debug)]
pub enum AudioSource {
    InputAudio { data: String, format: String },
    AudioUrl(String),
}

pub fn extract_audio(
    request: ChatCompletionRequest,
    model: &str,
) -> Result<(AudioSource, bool), ApiError> {
    if request.model != model {
        return Err(ApiError::model_not_found(&request.model));
    }

    let mut audio = None;
    for message in request.messages {
        if message.role != "user" {
            continue;
        }
        let parts = match message.content {
            MessageContent::Text(text) => {
                let _ = text;
                continue;
            }
            MessageContent::Parts(parts) => parts,
        };
        for part in parts {
            let candidate = match part {
                ContentPart::Text { text } => {
                    let _ = text;
                    continue;
                }
                ContentPart::InputAudio { input_audio } => AudioSource::InputAudio {
                    data: input_audio.data,
                    format: input_audio.format,
                },
                ContentPart::AudioUrl { audio_url } => AudioSource::AudioUrl(audio_url.url),
                ContentPart::Unsupported => {
                    return Err(ApiError::invalid_request(
                        "unsupported user content part",
                        Some("messages"),
                    ));
                }
            };
            if audio.replace(candidate).is_some() {
                return Err(ApiError::invalid_request(
                    "exactly one audio input is required",
                    Some("messages"),
                ));
            }
        }
    }
    audio.map(|audio| (audio, request.stream)).ok_or_else(|| {
        ApiError::invalid_request("exactly one audio input is required", Some("messages"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(content: serde_json::Value) -> ChatCompletionRequest {
        serde_json::from_value(serde_json::json!({
            "model": "doubao-asr",
            "messages": [{"role": "user", "content": content}]
        }))
        .unwrap()
    }

    #[test]
    fn extracts_both_audio_schemas() {
        let (source, _) = extract_audio(
            request(serde_json::json!([{
                "type": "input_audio", "input_audio": {"data": "YQ==", "format": "wav"}
            }])),
            "doubao-asr",
        )
        .unwrap();
        assert!(matches!(source, AudioSource::InputAudio { .. }));

        let (source, _) = extract_audio(
            request(serde_json::json!([{
                "type": "audio_url", "audio_url": {"url": "https://example.com/a.wav"}
            }])),
            "doubao-asr",
        )
        .unwrap();
        assert!(matches!(source, AudioSource::AudioUrl(_)));
    }

    #[test]
    fn requires_exactly_one_audio_and_known_model() {
        assert!(extract_audio(request(serde_json::json!([])), "doubao-asr").is_err());
        assert!(extract_audio(
            request(serde_json::json!([
                {"type":"input_audio", "input_audio":{"data":"YQ==", "format":"wav"}},
                {"type":"audio_url", "audio_url":{"url":"https://example.com/a.wav"}}
            ])),
            "doubao-asr"
        )
        .is_err());
        let mut wrong = request(serde_json::json!([{
            "type": "input_audio", "input_audio": {"data": "YQ==", "format": "wav"}
        }]));
        wrong.model = "other".into();
        assert_eq!(
            extract_audio(wrong, "doubao-asr").unwrap_err().status,
            axum::http::StatusCode::NOT_FOUND
        );
    }
}
