//! AWS vnd.amazon.eventstream binary frame decoder.
//!
//! Frame layout (all big-endian):
//! ```text
//! +-------------------+-------------------+-------------------+
//! | total_length (4B) | headers_len (4B)  | prelude_crc (4B)  |  ← 12B prelude
//! +-------------------+-------------------+-------------------+
//! | headers (headers_len bytes)                                |
//! +------------------------------------------------------------+
//! | payload (total_length - headers_len - 16 bytes)            |
//! +------------------------------------------------------------+
//! | message_crc (4B) — CRC32 over everything above             |
//! +------------------------------------------------------------+
//! ```
//!
//! Headers are `[name_len: u8][name: utf8][value_type: u8][value]` repeated.
//! This decoder only extracts `:event-type` and `:message-type` (both
//! value_type = 7 = string, encoded as `[u16 length][utf8 bytes]`).

use bytes::{Buf, BytesMut};

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("prelude CRC mismatch")]
    PreludeCrc,
    #[error("message CRC mismatch")]
    MessageCrc,
    #[error("frame too small (total_length < 16)")]
    UndersizedFrame,
    #[error("invalid header: {0}")]
    InvalidHeader(String),
    #[error("invalid UTF-8 in header")]
    Utf8,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Event {
        event_type: String,
        payload: Vec<u8>,
    },
    Exception {
        exception_type: String,
        payload: Vec<u8>,
    },
}

#[derive(Default)]
pub struct EventStreamDecoder {
    buf: BytesMut,
}

const PRELUDE_LEN: usize = 12;
const MESSAGE_CRC_LEN: usize = 4;
const MIN_FRAME_LEN: usize = 16; // prelude (12) + message CRC (4), no headers, no payload

impl EventStreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Returns `Ok(Some(frame))` when a full frame is available,
    /// `Ok(None)` when waiting for more bytes, `Err` on malformed data.
    ///
    /// On malformed data, the offending frame is consumed so subsequent
    /// calls can make forward progress.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, DecodeError> {
        if self.buf.len() < PRELUDE_LEN {
            return Ok(None);
        }

        // Read prelude without consuming. Indices 0..12 are guaranteed by the
        // `buf.len() < PRELUDE_LEN` check above, so the array form cannot panic.
        let total_length =
            u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        let headers_len =
            u32::from_be_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]) as usize;
        let prelude_crc =
            u32::from_be_bytes([self.buf[8], self.buf[9], self.buf[10], self.buf[11]]);

        if total_length < MIN_FRAME_LEN {
            // Consume the junk prelude so we don't loop forever.
            self.buf.advance(PRELUDE_LEN);
            return Err(DecodeError::UndersizedFrame);
        }

        let computed_prelude_crc = crc32fast::hash(&self.buf[0..8]);
        if computed_prelude_crc != prelude_crc {
            // Consume prelude to avoid repeating the same failure.
            self.buf.advance(PRELUDE_LEN);
            return Err(DecodeError::PreludeCrc);
        }

        if self.buf.len() < total_length {
            // Wait for more bytes — buffer stays intact.
            return Ok(None);
        }

        // Validate message CRC over bytes [0 .. total_length - 4].
        let msg_crc_offset = total_length - MESSAGE_CRC_LEN;
        let expected_msg_crc = u32::from_be_bytes([
            self.buf[msg_crc_offset],
            self.buf[msg_crc_offset + 1],
            self.buf[msg_crc_offset + 2],
            self.buf[msg_crc_offset + 3],
        ]);
        let computed_msg_crc = crc32fast::hash(&self.buf[0..msg_crc_offset]);
        if computed_msg_crc != expected_msg_crc {
            self.buf.advance(total_length);
            return Err(DecodeError::MessageCrc);
        }

        // Parse headers + payload.
        let headers_start = PRELUDE_LEN;
        let headers_end = headers_start + headers_len;
        let payload_start = headers_end;
        let payload_end = msg_crc_offset;

        if headers_end > msg_crc_offset {
            self.buf.advance(total_length);
            return Err(DecodeError::InvalidHeader(
                "headers_len exceeds frame payload region".into(),
            ));
        }

        let header_bytes = &self.buf[headers_start..headers_end];
        let (event_type, message_type) = parse_headers(header_bytes)?;

        let payload = self.buf[payload_start..payload_end].to_vec();

        // Consume the frame from the buffer now that we've extracted it.
        self.buf.advance(total_length);

        let event_type = event_type
            .ok_or_else(|| DecodeError::InvalidHeader("missing :event-type header".into()))?;
        let message_type = message_type
            .ok_or_else(|| DecodeError::InvalidHeader("missing :message-type header".into()))?;

        let frame = if message_type == "exception" {
            Frame::Exception {
                exception_type: event_type,
                payload,
            }
        } else {
            Frame::Event {
                event_type,
                payload,
            }
        };
        Ok(Some(frame))
    }
}

fn parse_headers(bytes: &[u8]) -> Result<(Option<String>, Option<String>), DecodeError> {
    let mut event_type: Option<String> = None;
    let mut message_type: Option<String> = None;
    let mut i = 0;
    while i < bytes.len() {
        // name_len: u8
        let name_len = *bytes
            .get(i)
            .ok_or_else(|| DecodeError::InvalidHeader("truncated at name_len".into()))?
            as usize;
        i += 1;
        if i + name_len > bytes.len() {
            return Err(DecodeError::InvalidHeader("truncated header name".into()));
        }
        let name = std::str::from_utf8(&bytes[i..i + name_len]).map_err(|_| DecodeError::Utf8)?;
        i += name_len;

        // value_type: u8
        let value_type = *bytes
            .get(i)
            .ok_or_else(|| DecodeError::InvalidHeader("truncated at value_type".into()))?;
        i += 1;

        // Parse (or skip past) the value according to its type. AWS event-stream
        // defines 10 value types; we only care about `:event-type` and
        // `:message-type` (both strings, type 7), but we must still correctly
        // advance past unfamiliar headers so future AWS additions don't abort
        // the whole stream.
        let value = read_header_value(bytes, &mut i, value_type, name)?;

        if let Some(v) = value {
            match name {
                ":event-type" => event_type = Some(v),
                ":message-type" => message_type = Some(v),
                _ => { /* ignore unknown string header */ }
            }
        }
    }
    Ok((event_type, message_type))
}

/// Parse or skip an event-stream header value. Returns `Some(string)` for
/// string types (value_type 7) that we actually consume, or `None` for
/// types that are skipped (0/1 = no value, fixed-size numerics, bytebuffer,
/// timestamp, uuid). Unknown value types fail loudly.
fn read_header_value(
    bytes: &[u8],
    i: &mut usize,
    value_type: u8,
    name: &str,
) -> Result<Option<String>, DecodeError> {
    let skip_fixed = |i: &mut usize, n: usize, bytes: &[u8]| -> Result<(), DecodeError> {
        if *i + n > bytes.len() {
            return Err(DecodeError::InvalidHeader(format!(
                "truncated {n}-byte value for header {name}"
            )));
        }
        *i += n;
        Ok(())
    };
    let skip_variable = |i: &mut usize, bytes: &[u8]| -> Result<(), DecodeError> {
        if *i + 2 > bytes.len() {
            return Err(DecodeError::InvalidHeader(
                "truncated at value length".into(),
            ));
        }
        let len = u16::from_be_bytes([bytes[*i], bytes[*i + 1]]) as usize;
        *i += 2;
        if *i + len > bytes.len() {
            return Err(DecodeError::InvalidHeader("truncated header value".into()));
        }
        *i += len;
        Ok(())
    };

    match value_type {
        0 | 1 => Ok(None),                           // true / false, no value bytes
        2 => skip_fixed(i, 1, bytes).map(|()| None), // byte
        3 => skip_fixed(i, 2, bytes).map(|()| None), // short
        4 => skip_fixed(i, 4, bytes).map(|()| None), // integer
        5 => skip_fixed(i, 8, bytes).map(|()| None), // long
        6 => skip_variable(i, bytes).map(|()| None), // bytebuffer
        7 => {
            // string — [u16 len][utf8]
            if *i + 2 > bytes.len() {
                return Err(DecodeError::InvalidHeader(
                    "truncated at value length".into(),
                ));
            }
            let value_len = u16::from_be_bytes([bytes[*i], bytes[*i + 1]]) as usize;
            *i += 2;
            if *i + value_len > bytes.len() {
                return Err(DecodeError::InvalidHeader("truncated header value".into()));
            }
            let value =
                std::str::from_utf8(&bytes[*i..*i + value_len]).map_err(|_| DecodeError::Utf8)?;
            *i += value_len;
            Ok(Some(value.to_string()))
        }
        8 => skip_fixed(i, 8, bytes).map(|()| None), // timestamp (epoch ms, i64)
        9 => skip_fixed(i, 16, bytes).map(|()| None), // uuid
        other => Err(DecodeError::InvalidHeader(format!(
            "unknown value_type {other} for header {name}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a valid frame with :event-type and :message-type headers.
    fn make_frame(event_type: &str, message_type: &str, payload: &[u8]) -> Vec<u8> {
        let mut headers = Vec::new();
        for (name, value) in [(":event-type", event_type), (":message-type", message_type)] {
            headers.push(name.len() as u8);
            headers.extend_from_slice(name.as_bytes());
            headers.push(7);
            headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }
        let total_length = 12 + headers.len() + payload.len() + 4;
        let mut frame = Vec::with_capacity(total_length);
        frame.extend_from_slice(&(total_length as u32).to_be_bytes());
        frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        let prelude_crc = crc32fast::hash(&frame[0..8]);
        frame.extend_from_slice(&prelude_crc.to_be_bytes());
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(payload);
        let msg_crc = crc32fast::hash(&frame);
        frame.extend_from_slice(&msg_crc.to_be_bytes());
        frame
    }

    #[test]
    fn decode_single_event_frame() {
        let frame = make_frame("contentBlockDelta", "event", br#"{"delta":{"text":"hi"}}"#);
        let mut d = EventStreamDecoder::new();
        d.feed(&frame);
        let f = d.next_frame().unwrap().unwrap();
        match f {
            Frame::Event {
                event_type,
                payload,
            } => {
                assert_eq!(event_type, "contentBlockDelta");
                assert_eq!(payload, br#"{"delta":{"text":"hi"}}"#);
            }
            _ => panic!("expected Event"),
        }
        assert!(
            d.next_frame().unwrap().is_none(),
            "buffer should be drained"
        );
    }

    #[test]
    fn decode_exception_frame() {
        let frame = make_frame(
            "ModelStreamErrorException",
            "exception",
            br#"{"message":"boom"}"#,
        );
        let mut d = EventStreamDecoder::new();
        d.feed(&frame);
        let f = d.next_frame().unwrap().unwrap();
        match f {
            Frame::Exception { exception_type, .. } => {
                assert_eq!(exception_type, "ModelStreamErrorException");
            }
            _ => panic!("expected Exception"),
        }
    }

    #[test]
    fn split_frame_across_two_feeds() {
        let frame = make_frame("messageStart", "event", br#"{"role":"assistant"}"#);
        let (a, b) = frame.split_at(10);
        let mut d = EventStreamDecoder::new();
        d.feed(a);
        assert!(d.next_frame().unwrap().is_none());
        d.feed(b);
        let f = d.next_frame().unwrap().unwrap();
        match f {
            Frame::Event { event_type, .. } => assert_eq!(event_type, "messageStart"),
            _ => panic!("expected Event"),
        }
    }

    #[test]
    fn two_frames_back_to_back() {
        let mut full = make_frame("messageStart", "event", b"{}");
        full.extend_from_slice(&make_frame(
            "messageStop",
            "event",
            br#"{"stopReason":"end_turn"}"#,
        ));
        let mut d = EventStreamDecoder::new();
        d.feed(&full);
        let f1 = d.next_frame().unwrap().unwrap();
        let f2 = d.next_frame().unwrap().unwrap();
        match (f1, f2) {
            (Frame::Event { event_type: e1, .. }, Frame::Event { event_type: e2, .. }) => {
                assert_eq!(e1, "messageStart");
                assert_eq!(e2, "messageStop");
            }
            _ => panic!("expected two Events"),
        }
        assert!(d.next_frame().unwrap().is_none());
    }

    #[test]
    fn decode_frame_with_extra_non_string_header_still_parses() {
        // Build a frame where, in addition to :event-type and :message-type
        // (type 7 strings), we include a `:timestamp` header with value_type 8
        // (8-byte int64). Our decoder should skip it without failing.
        let mut headers = Vec::new();
        for (name, value) in [
            (":event-type", "contentBlockDelta"),
            (":message-type", "event"),
        ] {
            headers.push(name.len() as u8);
            headers.extend_from_slice(name.as_bytes());
            headers.push(7);
            headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }
        // :timestamp, value_type 8, 8-byte epoch ms
        let ts_name = ":timestamp";
        headers.push(ts_name.len() as u8);
        headers.extend_from_slice(ts_name.as_bytes());
        headers.push(8);
        headers.extend_from_slice(&1_705_320_000_000_i64.to_be_bytes());

        let payload = br#"{"delta":{"text":"ok"}}"#;
        let total_length = 12 + headers.len() + payload.len() + 4;
        let mut frame = Vec::with_capacity(total_length);
        frame.extend_from_slice(&(total_length as u32).to_be_bytes());
        frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        let prelude_crc = crc32fast::hash(&frame[0..8]);
        frame.extend_from_slice(&prelude_crc.to_be_bytes());
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(payload);
        let msg_crc = crc32fast::hash(&frame);
        frame.extend_from_slice(&msg_crc.to_be_bytes());

        let mut d = EventStreamDecoder::new();
        d.feed(&frame);
        let f = d.next_frame().unwrap().unwrap();
        match f {
            Frame::Event {
                event_type,
                payload,
            } => {
                assert_eq!(event_type, "contentBlockDelta");
                assert_eq!(payload, br#"{"delta":{"text":"ok"}}"#);
            }
            _ => panic!("expected Event"),
        }
    }

    #[test]
    fn corrupted_prelude_crc_errors() {
        let mut frame = make_frame("x", "event", b"{}");
        frame[8] ^= 0xFF;
        let mut d = EventStreamDecoder::new();
        d.feed(&frame);
        let err = d.next_frame().unwrap_err();
        assert!(matches!(err, DecodeError::PreludeCrc));
    }

    #[test]
    fn corrupted_message_crc_errors() {
        let mut frame = make_frame("x", "event", b"{}");
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;
        let mut d = EventStreamDecoder::new();
        d.feed(&frame);
        let err = d.next_frame().unwrap_err();
        assert!(matches!(err, DecodeError::MessageCrc));
    }
}
