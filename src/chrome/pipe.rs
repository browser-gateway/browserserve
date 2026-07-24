//! `\0`-framed CDP transport over the browser's inherited pipe fds.

use bytes::{Buf, Bytes, BytesMut};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;

/// Errors on the CDP pipe transport.
#[derive(Debug, Error)]
pub enum PipeError {
    /// The browser closed its end of the pipe (it exited or is exiting).
    #[error("CDP pipe closed by the browser")]
    Closed,
    /// A single CDP message exceeded the configured cap.
    #[error("CDP frame exceeds the {max_bytes}-byte cap")]
    FrameTooLarge {
        /// The configured cap in bytes.
        max_bytes: usize,
    },
    /// A frame was not valid JSON.
    #[error("CDP frame is not valid JSON: {0}")]
    BadJson(#[from] serde_json::Error),
    /// Underlying pipe I/O failure.
    #[error("CDP pipe I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// The write half: sends one CDP message per call.
pub struct CdpWriter {
    writer: pipe::Sender,
}

impl CdpWriter {
    /// Sends one raw, already-serialized CDP message (no `\0` terminator).
    ///
    /// # Errors
    ///
    /// [`PipeError::Closed`] when the browser has gone away, or
    /// [`PipeError::Io`] on any other pipe failure.
    pub async fn send_raw(&mut self, frame: &[u8]) -> Result<(), PipeError> {
        self.writer.write_all(frame).await.map_err(map_closed)?;
        self.writer.write_all(&[0]).await.map_err(map_closed)?;
        Ok(())
    }

    /// Serializes and sends one CDP message.
    ///
    /// # Errors
    ///
    /// As [`CdpWriter::send_raw`], plus [`PipeError::BadJson`] on
    /// serialization failure.
    pub async fn send(&mut self, message: &serde_json::Value) -> Result<(), PipeError> {
        let frame = serde_json::to_vec(message)?;
        self.send_raw(&frame).await
    }
}

/// The read half: receives one CDP message per call.
pub struct CdpReader {
    reader: pipe::Receiver,
    buf: BytesMut,
    max_frame: usize,
}

impl CdpReader {
    /// Receives the next raw CDP frame (JSON bytes, `\0` stripped), buffering
    /// until complete. Cancellation-safe: partial data stays buffered. This is
    /// the bridge hot path: no JSON parsing happens here.
    ///
    /// # Errors
    ///
    /// [`PipeError::Closed`] on end-of-stream, [`PipeError::FrameTooLarge`]
    /// when a message exceeds the cap, or [`PipeError::Io`] on any other pipe
    /// failure.
    pub async fn recv_raw(&mut self) -> Result<Bytes, PipeError> {
        loop {
            if let Some(frame) = take_frame(&mut self.buf, self.max_frame)? {
                return Ok(frame);
            }
            let read = self
                .reader
                .read_buf(&mut self.buf)
                .await
                .map_err(map_closed)?;
            if read == 0 {
                return Err(PipeError::Closed);
            }
        }
    }

    /// Receives and parses the next CDP message. Probe/control use only.
    ///
    /// # Errors
    ///
    /// As [`CdpReader::recv_raw`], plus [`PipeError::BadJson`] on a corrupt
    /// frame.
    pub async fn recv(&mut self) -> Result<serde_json::Value, PipeError> {
        let frame = self.recv_raw().await?;
        Ok(serde_json::from_slice(&frame)?)
    }
}

/// One browser's CDP command/response pipe pair.
///
/// Messages are JSON terminated by a single `\0` byte in both directions.
pub struct CdpPipe {
    writer: CdpWriter,
    reader: CdpReader,
}

impl CdpPipe {
    /// Wraps the runtime-side ends of the browser's fd 3 / fd 4 pipes.
    #[must_use]
    pub fn new(writer: pipe::Sender, reader: pipe::Receiver, max_frame: usize) -> Self {
        Self {
            writer: CdpWriter { writer },
            reader: CdpReader {
                reader,
                buf: BytesMut::with_capacity(8 * 1024),
                max_frame,
            },
        }
    }

    /// Splits into independently owned halves for concurrent pumping.
    #[must_use]
    pub fn split(self) -> (CdpReader, CdpWriter) {
        (self.reader, self.writer)
    }

    /// Reassembles a pipe from halves taken by [`CdpPipe::split`], so a bridge
    /// can hand the transport back for post-session capture.
    #[must_use]
    pub fn from_halves(reader: CdpReader, writer: CdpWriter) -> Self {
        Self { writer, reader }
    }

    /// Sends one CDP message.
    ///
    /// # Errors
    ///
    /// See [`CdpWriter::send`].
    pub async fn send(&mut self, message: &serde_json::Value) -> Result<(), PipeError> {
        self.writer.send(message).await
    }

    /// Receives the next CDP message.
    ///
    /// # Errors
    ///
    /// See [`CdpReader::recv`].
    pub async fn recv(&mut self) -> Result<serde_json::Value, PipeError> {
        self.reader.recv().await
    }
}

fn map_closed(e: std::io::Error) -> PipeError {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        PipeError::Closed
    } else {
        PipeError::Io(e)
    }
}

fn take_frame(buf: &mut BytesMut, max_frame: usize) -> Result<Option<Bytes>, PipeError> {
    if let Some(pos) = memchr::memchr(0, buf) {
        if pos > max_frame {
            return Err(PipeError::FrameTooLarge {
                max_bytes: max_frame,
            });
        }
        let frame = buf.split_to(pos).freeze();
        buf.advance(1);
        return Ok(Some(frame));
    }
    if buf.len() > max_frame {
        return Err(PipeError::FrameTooLarge {
            max_bytes: max_frame,
        });
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf_from(bytes: &[u8]) -> BytesMut {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(bytes);
        buf
    }

    #[test]
    fn incomplete_frame_returns_none_and_keeps_bytes() {
        let mut buf = buf_from(b"{\"id\":1");
        assert!(take_frame(&mut buf, 1024).unwrap().is_none());
        assert_eq!(&buf[..], b"{\"id\":1");
    }

    #[test]
    fn complete_frame_is_extracted_and_consumed() {
        let mut buf = buf_from(b"{\"id\":1}\0rest");
        let frame = take_frame(&mut buf, 1024).unwrap().unwrap();
        assert_eq!(&frame[..], b"{\"id\":1}");
        assert_eq!(&buf[..], b"rest");
    }

    #[test]
    fn multiple_frames_come_out_in_order() {
        let mut buf = buf_from(b"a\0b\0c\0");
        let mut out = Vec::new();
        while let Some(frame) = take_frame(&mut buf, 1024).unwrap() {
            out.push(frame);
        }
        assert_eq!(out, [&b"a"[..], b"b", b"c"]);
        assert!(buf.is_empty());
    }

    #[test]
    fn frame_split_across_pushes_survives() {
        let mut buf = buf_from(b"{\"a\":");
        assert!(take_frame(&mut buf, 1024).unwrap().is_none());
        buf.extend_from_slice(b"1}\0");
        let frame = take_frame(&mut buf, 1024).unwrap().unwrap();
        assert_eq!(&frame[..], b"{\"a\":1}");
    }

    #[test]
    fn unterminated_overflow_is_rejected() {
        let mut buf = buf_from(&[b'x'; 32]);
        assert!(matches!(
            take_frame(&mut buf, 16),
            Err(PipeError::FrameTooLarge { max_bytes: 16 })
        ));
    }

    #[test]
    fn terminated_oversize_frame_is_rejected() {
        let mut buf = buf_from(&[b'x'; 32]);
        buf.extend_from_slice(b"\0");
        assert!(matches!(
            take_frame(&mut buf, 16),
            Err(PipeError::FrameTooLarge { max_bytes: 16 })
        ));
    }

    #[tokio::test]
    async fn send_and_recv_round_trip_over_real_pipes() {
        let (chrome_side_tx, our_rx) = pipe::pipe().unwrap();
        let (our_tx, mut chrome_side_rx) = pipe::pipe().unwrap();
        let mut cdp = CdpPipe::new(our_tx, our_rx, 1024 * 1024);

        let message = serde_json::json!({ "id": 1, "method": "Browser.getVersion" });
        cdp.send(&message).await.unwrap();
        let mut received = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            chrome_side_rx.read_exact(&mut byte).await.unwrap();
            if byte[0] == 0 {
                break;
            }
            received.push(byte[0]);
        }
        let echoed: serde_json::Value = serde_json::from_slice(&received).unwrap();
        assert_eq!(echoed, message);

        let reply = serde_json::json!({ "id": 1, "result": { "product": "TestBrowser" } });
        let mut frame = serde_json::to_vec(&reply).unwrap();
        frame.push(0);
        let mut writer = chrome_side_tx;
        writer.write_all(&frame).await.unwrap();
        assert_eq!(cdp.recv().await.unwrap(), reply);
    }

    #[tokio::test]
    async fn recv_reports_closed_on_eof() {
        let (chrome_side_tx, our_rx) = pipe::pipe().unwrap();
        let (our_tx, _keep) = pipe::pipe().unwrap();
        let mut cdp = CdpPipe::new(our_tx, our_rx, 1024);
        drop(chrome_side_tx);
        assert!(matches!(cdp.recv().await, Err(PipeError::Closed)));
    }
}
