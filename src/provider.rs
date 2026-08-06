use std::{collections::HashSet, time::Duration};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
};

use crate::{
    config::Config,
    error::{Error, Result},
    sami,
};

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

#[derive(Debug)]
struct Recognition {
    text: String,
    final_result: bool,
}

enum Event {
    Kind(String),
    Recognition(Recognition),
    Error(Error),
}

struct ProviderClient {
    config: Config,
    writer: WsSink,
    events: mpsc::Receiver<Event>,
    task_id: String,
    reader_task: tokio::task::JoinHandle<()>,
}

impl Drop for ProviderClient {
    fn drop(&mut self) {
        // The split reader otherwise keeps the socket and task alive after the
        // transcription future completes or is cancelled.
        self.reader_task.abort();
    }
}

impl ProviderClient {
    async fn dial(config: Config) -> Result<Self> {
        let url = config
            .ws_url()
            .map_err(|_| Error::msg("could not prepare Doubao ASR connection"))?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| Error::msg("could not prepare Doubao ASR connection"))?;
        let headers = request.headers_mut();
        headers.insert(
            "user-agent",
            config.user_agent().parse().expect("static header"),
        );
        headers.insert("proto-version", "v2".parse().unwrap());
        headers.insert("x-custom-keepalive", "true".parse().unwrap());
        let (socket, _) = tokio::time::timeout(Duration::from_secs(10), connect_async(request))
            .await
            .map_err(|_| Error::msg("could not connect to Doubao ASR"))?
            .map_err(|_| Error::msg("could not connect to Doubao ASR"))?;
        let (writer, mut reader) = socket.split();
        let (tx, events) = mpsc::channel(1024);
        let reader_task = tokio::spawn(async move {
            while let Some(message) = reader.next().await {
                let raw = match message {
                    Ok(Message::Binary(raw)) => raw,
                    Ok(Message::Close(_)) | Err(_) => {
                        let _ = tx
                            .send(Event::Error(Error::msg(
                                "Doubao ASR connection closed unexpectedly",
                            )))
                            .await;
                        return;
                    }
                    _ => continue,
                };
                let frame = match sami::decode(&raw) {
                    Ok(frame) => frame,
                    Err(_) => {
                        if tx
                            .send(Event::Error(Error::msg(
                                "Doubao ASR returned an invalid protocol frame",
                            )))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        continue;
                    }
                };
                match frame.event.as_str() {
                    "TaskStarted" | "SessionStarted" | "SessionFinished" => {
                        if tx.send(Event::Kind(frame.event)).await.is_err() {
                            return;
                        }
                    }
                    "TaskFailed" | "SessionFailed" => {
                        let error = if frame
                            .status_text
                            .to_ascii_lowercase()
                            .contains("service discovery failure")
                        {
                            Error::Unroutable
                        } else {
                            Error::msg(format!(
                                "Doubao ASR rejected the session (code {})",
                                frame.status_code
                            ))
                        };
                        if tx.send(Event::Error(error)).await.is_err() {
                            return;
                        }
                    }
                    _ => {
                        #[derive(Deserialize)]
                        struct Payload {
                            #[serde(default)]
                            results: Vec<WireResult>,
                        }
                        #[derive(Deserialize)]
                        struct WireResult {
                            text: String,
                            #[serde(default)]
                            is_interim: bool,
                        }
                        if let Ok(payload) = serde_json::from_str::<Payload>(&frame.payload_json) {
                            for result in payload.results {
                                if tx
                                    .send(Event::Recognition(Recognition {
                                        text: result.text,
                                        final_result: !result.is_interim,
                                    }))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        });
        Ok(Self {
            config,
            writer,
            events,
            task_id: String::new(),
            reader_task,
        })
    }

    async fn write(&mut self, bytes: Vec<u8>) -> Result<()> {
        self.writer
            .send(Message::Binary(bytes.into()))
            .await
            .map_err(|_| Error::msg("Doubao ASR connection write failed"))
    }

    async fn start(&mut self) -> Result<()> {
        self.task_id = uuid::Uuid::new_v4().to_string();
        self.write(sami::encode_start_task(&self.config.token, &self.task_id))
            .await?;
        self.wait_for("TaskStarted", None, Duration::from_secs(15))
            .await?;
        self.write(sami::encode_start_session(
            &self.config.token,
            &self.task_id,
            &self.config.session_json(),
        ))
        .await?;
        self.wait_for("SessionStarted", None, Duration::from_secs(15))
            .await
    }

    async fn send_opus(&mut self, packet: &[u8], state: u64, timestamp_ms: i64) -> Result<()> {
        if packet.is_empty() {
            return Err(Error::msg("refusing to send an empty Opus packet"));
        }
        self.write(sami::encode_task_request(
            &self.task_id,
            timestamp_ms,
            state,
            packet,
        ))
        .await
    }

    fn drain(&mut self, accumulator: &mut TextAccumulator) -> Result<()> {
        loop {
            match self.events.try_recv() {
                Ok(Event::Recognition(r)) => accumulator.consume(r),
                Ok(Event::Error(e)) => return Err(e),
                Ok(Event::Kind(_)) => {}
                Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Err(Error::msg("Doubao ASR connection closed unexpectedly"))
                }
            }
        }
    }

    async fn finish(&mut self, accumulator: &mut TextAccumulator) -> Result<()> {
        self.write(sami::encode_finish_session(
            &self.config.token,
            &self.task_id,
        ))
        .await?;
        self.wait_for(
            "SessionFinished",
            Some(accumulator),
            Duration::from_secs(20),
        )
        .await
    }

    async fn wait_for(
        &mut self,
        expected: &str,
        mut accumulator: Option<&mut TextAccumulator>,
        timeout: Duration,
    ) -> Result<()> {
        tokio::time::timeout(timeout, async {
            loop {
                match self.events.recv().await {
                    Some(Event::Kind(kind)) if kind == expected => return Ok(()),
                    Some(Event::Recognition(r)) => {
                        if let Some(a) = accumulator.as_deref_mut() {
                            a.consume(r);
                        }
                    }
                    Some(Event::Error(e)) => return Err(e),
                    Some(Event::Kind(_)) => {}
                    None => return Err(Error::msg("Doubao ASR connection closed unexpectedly")),
                }
            }
        })
        .await
        .map_err(|_| Error::msg(format!("timeout waiting for Doubao ASR {expected}")))?
    }
}

#[derive(Default)]
struct TextAccumulator {
    final_parts: Vec<String>,
    seen: HashSet<String>,
    latest_interim: String,
}
impl TextAccumulator {
    fn consume(&mut self, result: Recognition) {
        let text = result.text.trim();
        if text.is_empty() {
            return;
        }
        if !result.final_result {
            self.latest_interim = text.into();
            return;
        }
        self.latest_interim.clear();
        if self.seen.insert(text.into()) {
            self.final_parts.push(text.into());
        }
    }
    fn text(&self) -> String {
        if self.final_parts.is_empty() {
            self.latest_interim.clone()
        } else {
            self.final_parts.join("\n")
        }
    }
}

pub(crate) struct ProviderTranscript {
    pub text: String,
    pub duration_ms: u64,
    pub frames: usize,
}

pub(crate) async fn transcribe_file(
    path: &std::path::Path,
    credentials: &crate::credentials::Credentials,
    punctuation: bool,
    speed: f64,
) -> Result<ProviderTranscript> {
    if !(speed > 0.0 && speed <= 4.0) {
        return Err(Error::msg(
            "speed must be greater than 0 and no more than 4",
        ));
    }
    let mut client = ProviderClient::dial(Config::new(credentials, punctuation)).await?;
    client.start().await?;
    let mut stream = crate::audio::start_opus_stream(path).await?;
    let mut current = stream
        .next()
        .await?
        .ok_or_else(|| Error::msg("the input contains no decodable audio"))?;
    let started = tokio::time::Instant::now();
    let mut frame_index = 0usize;
    let mut accumulator = TextAccumulator::default();
    loop {
        let next = stream.next().await?;
        let state = if next.is_none() {
            sami::FRAME_STATE_LAST
        } else if frame_index == 0 {
            sami::FRAME_STATE_FIRST
        } else {
            sami::FRAME_STATE_MIDDLE
        };
        let deadline = started + Duration::from_secs_f64((frame_index as f64 * 0.020) / speed);
        tokio::time::sleep_until(deadline).await;
        client
            .send_opus(&current, state, (frame_index * 20) as i64)
            .await?;
        frame_index += 1;
        client.drain(&mut accumulator)?;
        match next {
            Some(packet) => current = packet,
            None => break,
        }
    }
    stream.finish().await?;
    client.finish(&mut accumulator).await?;
    Ok(ProviderTranscript {
        text: accumulator.text(),
        duration_ms: (frame_index * 20) as u64,
        frames: frame_index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accumulator_deduplicates_finals() {
        let mut a = TextAccumulator::default();
        a.consume(Recognition {
            text: " 第一 句 ".into(),
            final_result: false,
        });
        a.consume(Recognition {
            text: "第一句。".into(),
            final_result: true,
        });
        a.consume(Recognition {
            text: "第一句。".into(),
            final_result: true,
        });
        a.consume(Recognition {
            text: "第二句。".into(),
            final_result: true,
        });
        assert_eq!(a.text(), "第一句。\n第二句。");
    }
    #[test]
    fn accumulator_uses_interim_fallback() {
        let mut a = TextAccumulator::default();
        a.consume(Recognition {
            text: "临时结果".into(),
            final_result: false,
        });
        assert_eq!(a.text(), "临时结果");
    }
}
