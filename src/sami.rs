//! SAMI's small protobuf-compatible wire format.
//!
//! This intentionally implements only the wire types used by the service.  It
//! does not depend on a protobuf runtime, which also makes it possible to keep
//! accepting the slightly irregular response messages returned by the IME
//! endpoint.

use crate::error::{Error, Result};

pub const FRAME_STATE_FIRST: u64 = 1;
pub const FRAME_STATE_MIDDLE: u64 = 3;
pub const FRAME_STATE_LAST: u64 = 9;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Frame {
    pub request_id: String,
    pub event: String,
    pub status_code: u64,
    pub status_text: String,
    pub payload_json: String,
}

pub fn encode_start_task(app_key: &str, request_id: &str) -> Vec<u8> {
    let mut out = Vec::new();
    append_bytes(&mut out, 2, app_key.as_bytes());
    append_bytes(&mut out, 3, b"ASR");
    append_bytes(&mut out, 5, b"StartTask");
    append_bytes(&mut out, 8, request_id.as_bytes());
    out
}

pub fn encode_start_session(app_key: &str, request_id: &str, session_json: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    append_bytes(&mut out, 2, app_key.as_bytes());
    append_bytes(&mut out, 3, b"ASR");
    append_bytes(&mut out, 5, b"StartSession");
    append_bytes(&mut out, 6, session_json);
    append_bytes(&mut out, 8, request_id.as_bytes());
    out
}

pub fn encode_task_request(
    request_id: &str,
    timestamp_ms: i64,
    frame_state: u64,
    audio: &[u8],
) -> Vec<u8> {
    let payload = format!(r#"{{"extra":{{}},"timestamp_ms":{timestamp_ms}}}"#);
    let mut out = Vec::new();
    append_bytes(&mut out, 3, b"ASR");
    append_bytes(&mut out, 5, b"TaskRequest");
    append_bytes(&mut out, 6, payload.as_bytes());
    append_bytes(&mut out, 7, audio);
    append_bytes(&mut out, 8, request_id.as_bytes());
    append_varint_field(&mut out, 9, frame_state);
    out
}

pub fn encode_finish_session(app_key: &str, request_id: &str) -> Vec<u8> {
    let mut out = Vec::new();
    append_bytes(&mut out, 2, app_key.as_bytes());
    append_bytes(&mut out, 3, b"ASR");
    append_bytes(&mut out, 5, b"FinishSession");
    append_bytes(&mut out, 8, request_id.as_bytes());
    out
}

pub fn decode(data: &[u8]) -> Result<Frame> {
    let mut frame = Frame::default();
    let mut offset = 0;
    while offset < data.len() {
        let (key, used) =
            read_varint(&data[offset..]).ok_or(Error::msg("invalid SAMI field key"))?;
        offset += used;
        let field = key >> 3;
        match key & 7 {
            0 => {
                let (value, used) =
                    read_varint(&data[offset..]).ok_or(Error::msg("invalid SAMI varint"))?;
                offset += used;
                if field == 5 {
                    frame.status_code = value;
                }
            }
            1 => {
                if data.len() - offset < 8 {
                    return Err(Error::msg("truncated SAMI fixed64"));
                }
                offset += 8;
            }
            2 => {
                let (length, used) =
                    read_varint(&data[offset..]).ok_or(Error::msg("invalid SAMI length"))?;
                offset += used;
                let length =
                    usize::try_from(length).map_err(|_| Error::msg("truncated SAMI bytes"))?;
                if length > data.len() - offset {
                    return Err(Error::msg("truncated SAMI bytes"));
                }
                let value = &data[offset..offset + length];
                offset += length;
                let Ok(text) = std::str::from_utf8(value) else {
                    continue;
                };
                match field {
                    1 | 8 => frame.request_id = text.to_owned(),
                    4 | 5 => frame.event = text.to_owned(),
                    6 if looks_json(text) => frame.payload_json = text.to_owned(),
                    6 => frame.status_text = text.to_owned(),
                    7 if looks_json(text) => frame.payload_json = text.to_owned(),
                    _ => {}
                }
            }
            5 => {
                if data.len() - offset < 4 {
                    return Err(Error::msg("truncated SAMI fixed32"));
                }
                offset += 4;
            }
            _ => return Err(Error::msg("unsupported SAMI wire type")),
        }
    }
    Ok(frame)
}

fn looks_json(value: &str) -> bool {
    value
        .chars()
        .find(|c| !matches!(c, ' ' | '\n' | '\r' | '\t'))
        .is_some_and(|c| matches!(c, '{' | '['))
}

fn append_bytes(dst: &mut Vec<u8>, field: u64, value: &[u8]) {
    append_varint(dst, field << 3 | 2);
    append_varint(dst, value.len() as u64);
    dst.extend_from_slice(value);
}
fn append_varint_field(dst: &mut Vec<u8>, field: u64, value: u64) {
    append_varint(dst, field << 3);
    append_varint(dst, value);
}
fn append_varint(dst: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        dst.push(value as u8 | 0x80);
        value >>= 7;
    }
    dst.push(value as u8);
}
fn read_varint(src: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, &byte) in src.iter().take(10).enumerate() {
        if byte < 0x80 {
            return Some((value | u64::from(byte) << (i * 7), i + 1));
        }
        value |= u64::from(byte & 0x7f) << (i * 7);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_request_vectors() {
        assert_eq!(
            encode_start_task("token", "request"),
            b"\x12\x05token\x1a\x03ASR\x2a\x09StartTask\x42\x07request"
        );
        assert_eq!(
            encode_start_session("k", "r", br#"{"audio_info":{}}"#),
            b"\x12\x01k\x1a\x03ASR\x2a\x0cStartSession\x32\x11{\"audio_info\":{}}\x42\x01r"
        );
        assert_eq!(
            encode_finish_session("k", "r"),
            b"\x12\x01k\x1a\x03ASR\x2a\x0dFinishSession\x42\x01r"
        );
        assert_eq!(encode_task_request("r", 20, FRAME_STATE_FIRST, &[1, 2]), b"\x1a\x03ASR\x2a\x0bTaskRequest\x32\x1e{\"extra\":{},\"timestamp_ms\":20}\x3a\x02\x01\x02\x42\x01r\x48\x01");
    }

    #[test]
    fn request_frames_decode() {
        for (bytes, event) in [
            (encode_start_task("token", "request"), "StartTask"),
            (
                encode_start_session("token", "request", br#"{"audio_info":{}}"#),
                "StartSession",
            ),
            (
                encode_task_request("request", 1234, FRAME_STATE_LAST, &[1, 2, 3]),
                "TaskRequest",
            ),
            (encode_finish_session("token", "request"), "FinishSession"),
        ] {
            let frame = decode(&bytes).unwrap();
            assert_eq!(frame.event, event);
            assert_eq!(frame.request_id, "request");
            if event == "TaskRequest" {
                assert!(frame.payload_json.contains(r#""timestamp_ms":1234"#));
            }
        }
    }

    #[test]
    fn decodes_response_fields_and_skips_unknown_wire_fields() {
        // request ID (field 1), event (4), status code (5), status text (6), JSON (7),
        // followed by unknown fixed64 and fixed32 fields.
        let bytes = b"\x0a\x03rid\x22\x0bTaskStarted\x28\xac\x02\x32\x02ok\x3a\x08 {\"x\":1}\x51\0\0\0\0\0\0\0\0\x5d\0\0\0\0";
        let frame = decode(bytes).unwrap();
        assert_eq!(frame.request_id, "rid");
        assert_eq!(frame.event, "TaskStarted");
        assert_eq!(frame.status_code, 300);
        assert_eq!(frame.status_text, "ok");
        assert_eq!(frame.payload_json, " {\"x\":1}");
    }

    #[test]
    fn invalid_and_truncated_inputs_are_rejected() {
        for bytes in [
            &b"\x80"[..],
            &b"\x08\x80"[..],
            &b"\x0a\x03x"[..],
            &b"\x09\0"[..],
            &b"\x0d\0"[..],
            &b"\x0b"[..],
        ] {
            assert!(decode(bytes).is_err(), "accepted {bytes:?}");
        }
    }

    #[test]
    fn non_utf8_strings_are_ignored() {
        assert_eq!(decode(b"\x0a\x01\xff").unwrap(), Frame::default());
    }
}
