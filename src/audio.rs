//! Conversion to Opus and reconstruction of packets from the Ogg container.

use std::ffi::OsStr;
use std::io;
use std::process::Stdio;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use crate::error::{Error, Result};

/// Reconstructs packets from the lacing values in Ogg pages.
pub struct OggPacketReader<R> {
    reader: R,
    partial: Vec<u8>,
    queued: std::collections::VecDeque<Vec<u8>>,
}

impl<R: AsyncRead + Unpin> OggPacketReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            partial: Vec::new(),
            queued: std::collections::VecDeque::new(),
        }
    }

    pub async fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        while self.queued.is_empty() {
            match self.read_page().await {
                Err(error)
                    if error.kind() == io::ErrorKind::UnexpectedEof
                        && error.to_string() == "end of Ogg stream" =>
                {
                    if self.partial.is_empty() {
                        return Ok(None);
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated Ogg packet",
                    ));
                }
                Err(error) => return Err(error),
                Ok(()) => {}
            }
        }
        Ok(Some(
            self.queued.pop_front().expect("queue was checked above"),
        ))
    }

    async fn read_page(&mut self) -> io::Result<()> {
        let mut header = [0u8; 27];
        read_header_or_eof(&mut self.reader, &mut header).await?;
        if &header[..4] != b"OggS" || header[4] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Ogg page header",
            ));
        }

        let mut lacing = vec![0u8; usize::from(header[26])];
        self.reader.read_exact(&mut lacing).await?;
        let payload_len = lacing.iter().map(|&size| usize::from(size)).sum();
        let mut payload = vec![0u8; payload_len];
        self.reader.read_exact(&mut payload).await?;

        let mut offset = 0;
        for size in lacing {
            let end = offset + usize::from(size);
            self.partial.extend_from_slice(&payload[offset..end]);
            offset = end;
            if size < 255 {
                self.queued.push_back(std::mem::take(&mut self.partial));
            }
        }
        Ok(())
    }
}

async fn read_header_or_eof<R: AsyncRead + Unpin>(
    reader: &mut R,
    header: &mut [u8; 27],
) -> io::Result<()> {
    let mut filled = 0;
    while filled < header.len() {
        let count = reader.read(&mut header[filled..]).await?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                if filled == 0 {
                    "end of Ogg stream"
                } else {
                    "truncated Ogg page header"
                },
            ));
        }
        filled += count;
    }
    Ok(())
}

pub fn is_opus_header(packet: &[u8]) -> bool {
    packet.starts_with(b"OpusHead") || packet.starts_with(b"OpusTags")
}

/// Builds an Ogg page for tests and protocol fixtures. CRC is intentionally zero.
#[cfg(test)]
pub fn make_ogg_page(
    header_type: u8,
    sequence: u32,
    segments: &[u8],
    payload: &[u8],
) -> io::Result<Vec<u8>> {
    if segments.len() > u8::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many Ogg segments",
        ));
    }
    let length: usize = segments.iter().map(|&size| usize::from(size)).sum();
    if length != payload.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "lacing length {length} does not match payload {}",
                payload.len()
            ),
        ));
    }
    let mut page = vec![0u8; 27];
    page[..4].copy_from_slice(b"OggS");
    page[5] = header_type;
    page[18..22].copy_from_slice(&sequence.to_le_bytes());
    page[26] = segments.len() as u8;
    page.extend_from_slice(segments);
    page.extend_from_slice(payload);
    Ok(page)
}

pub struct OpusStream {
    child: Child,
    packets: OggPacketReader<tokio::process::ChildStdout>,
    stderr_task: Option<JoinHandle<io::Result<Vec<u8>>>>,
    waited: bool,
}

pub async fn start_opus_stream(input_path: impl AsRef<OsStr>) -> Result<OpusStream> {
    let mut command = Command::new("ffmpeg");
    command
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(input_path)
        .args([
            "-map",
            "0:a:0",
            "-vn",
            "-af",
            "apad=pad_dur=0.02",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-c:a",
            "libopus",
            "-application",
            "audio",
            "-frame_duration",
            "20",
            "-f",
            "opus",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            Error::msg("ffmpeg is required but was not found in PATH")
        } else {
            Error::msg(format!("start ffmpeg: {error}"))
        }
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::msg("open ffmpeg output"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::msg("open ffmpeg error output"))?;
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await?;
        Ok(bytes)
    });
    Ok(OpusStream {
        child,
        packets: OggPacketReader::new(stdout),
        stderr_task: Some(stderr_task),
        waited: false,
    })
}

impl OpusStream {
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            match self.packets.next().await {
                Ok(Some(packet)) if is_opus_header(&packet) => continue,
                Ok(Some(packet)) => return Ok(Some(packet)),
                Ok(None) => {
                    self.wait().await?;
                    return Ok(None);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Waits for ffmpeg and reports a non-zero exit status with its diagnostic.
    pub async fn finish(&mut self) -> Result<()> {
        self.wait().await
    }

    async fn wait(&mut self) -> Result<()> {
        if self.waited {
            return Ok(());
        }
        self.waited = true;
        let status = self.child.wait().await?;
        let stderr = match self.stderr_task.take() {
            Some(task) => task
                .await
                .map_err(|error| Error::msg(format!("read ffmpeg stderr: {error}")))??,
            None => Vec::new(),
        };
        if status.success() {
            return Ok(());
        }
        let mut detail = stderr;
        if detail.len() > 500 {
            detail.truncate(500);
            detail.extend_from_slice(b"...");
        }
        let detail = String::from_utf8_lossy(&detail).trim().to_owned();
        if detail.is_empty() {
            Err(Error::msg(format!("ffmpeg failed: {status}")))
        } else {
            Err(Error::msg(format!("ffmpeg failed: {detail}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    fn reader(bytes: Vec<u8>) -> OggPacketReader<BufReader<&'static [u8]>> {
        let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        OggPacketReader::new(BufReader::new(bytes))
    }

    #[tokio::test]
    async fn reads_literal_page_vector() {
        // One page containing packets "abc" and "de". Sequence number is little endian.
        let literal = b"\x4f\x67\x67\x53\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x04\x03\x02\x01\x00\x00\x00\x00\x02\x03\x02\x61\x62\x63\x64\x65";
        let mut packets = reader(literal.to_vec());
        assert_eq!(packets.next().await.unwrap().unwrap(), b"abc");
        assert_eq!(packets.next().await.unwrap().unwrap(), b"de");
        assert!(packets.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reconstructs_continuation_and_queues_following_packet() {
        let mut first_payload = vec![b'a'; 255];
        first_payload.extend(vec![b'b'; 10]);
        let page1 = make_ogg_page(0, 0, &[255], &first_payload[..255]).unwrap();
        let page2_payload = [&first_payload[255..], b"xyz"].concat();
        let page2 = make_ogg_page(1, 1, &[10, 3], &page2_payload).unwrap();
        let mut packets = reader([page1, page2].concat());
        assert_eq!(packets.next().await.unwrap().unwrap(), first_payload);
        assert_eq!(packets.next().await.unwrap().unwrap(), b"xyz");
    }

    #[tokio::test]
    async fn exact_255_byte_packet_needs_zero_terminator() {
        let payload = vec![7; 255];
        let page1 = make_ogg_page(0, 0, &[255], &payload).unwrap();
        let page2 = make_ogg_page(1, 1, &[0], &[]).unwrap();
        let mut packets = reader([page1, page2].concat());
        assert_eq!(packets.next().await.unwrap().unwrap(), payload);
    }

    #[tokio::test]
    async fn eof_in_partial_packet_is_unexpected() {
        let page = make_ogg_page(0, 0, &[255], &[1; 255]).unwrap();
        assert_eq!(
            reader(page).next().await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[tokio::test]
    async fn rejects_bad_capture_version_and_truncation() {
        for bytes in [vec![0; 27], b"OggS\x01".to_vec(), b"OggS\0".to_vec()] {
            assert!(reader(bytes).next().await.is_err());
        }
        let mut page = make_ogg_page(0, 0, &[3], b"abc").unwrap();
        page.pop();
        assert_eq!(
            reader(page).next().await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn recognizes_only_opus_identification_and_comment_headers() {
        assert!(is_opus_header(b"OpusHead..."));
        assert!(is_opus_header(b"OpusTags..."));
        assert!(!is_opus_header(&[0xf8, 0xff, 0xfe]));
        assert!(!is_opus_header(b"Opus"));
    }

    #[test]
    fn page_builder_validates_lacing_and_sets_fields() {
        let page = make_ogg_page(1, 0x0102_0304, &[3], b"abc").unwrap();
        assert_eq!(&page[..4], b"OggS");
        assert_eq!(page[5], 1);
        assert_eq!(&page[18..22], &[4, 3, 2, 1]);
        assert!(make_ogg_page(0, 0, &[2], b"abc").is_err());
        assert!(make_ogg_page(0, 0, &vec![0; 256], b"").is_err());
    }
}
