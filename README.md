# doubao-asr-rust

Unofficial asynchronous Rust SDK, CLI, and OpenAI-compatible HTTP server for
Doubao IME speech recognition.

The provider endpoints are undocumented and may change without notice. Use the
official Volcengine APIs for production workloads that require a supported
service contract.

## Requirements

- Rust 1.86 or newer
- `ffmpeg` in `PATH`, built with the `libopus` encoder
- Network access to the Doubao device registration and ASR endpoints

## Build and test

```bash
cargo build --release
cargo test --all-targets
```

## CLI

Transcribe the first audio stream in a local audio or video file:

```bash
./target/release/doubao-asr recording.wav
./target/release/doubao-asr --json --output transcript.json recording.mp3
```

The first successful request anonymously registers a Doubao IME device and
stores its credentials in the OS user configuration directory. Credentials are
written atomically with owner-only permissions on Unix. Provider session
rejections first trigger a same-device token refresh and retry. Only a token
that is still rejected causes one device re-registration, so a stale token can
recover without deleting the credential file manually.

Run `doubao-asr --help` for all CLI options.

## OpenAI-compatible server

Start the server:

```bash
./target/release/doubao-asr-server
```

It listens on `127.0.0.1:8000` by default and exposes:

- `POST /v1/chat/completions`
- `GET /v1/models`
- `GET /v1/models/doubao-asr`
- `GET /healthz`

Only speech-to-text requests are supported. A request must contain exactly one
audio part. Text parts are accepted for client compatibility but are not sent
to the Doubao provider.

### OpenAI `input_audio`

`input_audio` accepts base64-encoded WAV or MP3 data:

```python
import base64
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8000/v1", api_key="unused")

with open("recording.wav", "rb") as audio_file:
    audio = base64.b64encode(audio_file.read()).decode("ascii")

response = client.chat.completions.create(
    model="doubao-asr",
    messages=[
        {
            "role": "user",
            "content": [
                {
                    "type": "input_audio",
                    "input_audio": {"data": audio, "format": "wav"},
                }
            ],
        }
    ],
)
print(response.choices[0].message.content)
```

### vLLM/Qwen `audio_url`

`audio_url` accepts public or private HTTP(S) URLs and base64 data URLs:

```bash
curl http://127.0.0.1:8000/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "doubao-asr",
    "messages": [{
      "role": "user",
      "content": [{
        "type": "audio_url",
        "audio_url": {"url": "https://example.com/recording.wav"}
      }]
    }]
  }'
```

Local file URLs such as `file:///tmp/recording.wav` are rejected. Encode local
files with `input_audio` or serve them over HTTP(S).

### Delayed streaming

Requests with `"stream": true` receive valid Chat Completions SSE chunks ending
in `data: [DONE]`. Doubao recognition still completes before the first chunk is
sent; this is protocol compatibility, not real-time transcription.

## Server configuration

| Environment variable | Default | Description |
| --- | --- | --- |
| `DOUBAO_ASR_LISTEN` | `127.0.0.1:8000` | Listen address |
| `DOUBAO_ASR_API_KEY` | unset | Optional Bearer API key |
| `DOUBAO_ASR_CREDENTIALS` | OS config path | Credential file |
| `DOUBAO_ASR_MODEL` | `doubao-asr` | Advertised and accepted model ID |
| `DOUBAO_ASR_UPLOAD_SPEED` | `1` | Upload pacing, greater than 0 and at most 4 |
| `DOUBAO_ASR_MAX_CONCURRENCY` | `2` | Simultaneous requests |
| `DOUBAO_ASR_REQUEST_TIMEOUT_SECS` | `1800` | End-to-end request timeout |
| `DOUBAO_ASR_CREDENTIAL_REFRESH_SECS` | `21600` | Proactive same-device token refresh interval; `0` disables it |

Equivalent command-line flags are available from:

```bash
doubao-asr-server --help
```

When `DOUBAO_ASR_API_KEY` is set, all `/v1/*` requests require:

```text
Authorization: Bearer <key>
```

`/healthz` remains unauthenticated.

The observed anonymous settings response does not include a token expiry or
TTL. The server therefore refreshes the token every six hours by default while
reusing the existing device identity. It checks once per minute, retains the
current credential after refresh failures, and retries maintenance after 1, 5,
15, then 60 minutes. Provider session rejection remains an immediate fallback
even when proactive refresh is disabled.

## Limits and security

- Decoded audio is limited to 25 MiB; JSON requests are limited to 36 MiB.
- Remote downloads time out after 30 seconds and follow at most five redirects.
- When concurrency is exhausted, the server returns an OpenAI-style HTTP 429.
- Temporary audio files are deleted when each request completes or is cancelled.
- Logs exclude API keys, provider tokens, audio URLs, request bodies, transcripts,
  and audio data.

Remote `audio_url` requests are intentionally allowed to reach localhost,
private networks, link-local addresses, and other HTTP(S) destinations. This is
an SSRF capability. Do not bind the server to a public interface without a
strong `DOUBAO_ASR_API_KEY` and appropriate reverse-proxy or firewall controls.

## Rust SDK

```rust,no_run
use doubao_asr::Client;

#[tokio::main]
async fn main() -> doubao_asr::Result<()> {
    let client = Client::new()?;
    let transcript = client.transcribe_file("recording.wav").await?;
    println!("{}", transcript.text);
    Ok(())
}
```

## License

MIT
