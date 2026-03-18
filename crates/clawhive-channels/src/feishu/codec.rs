use prost::Message;

pub const FRAME_METHOD_CONTROL: i32 = 0;
pub const FRAME_METHOD_DATA: i32 = 1;

#[derive(Clone, PartialEq, Message)]
pub struct Frame {
    #[prost(uint64, tag = "1")]
    pub seq_id: u64,
    #[prost(uint64, tag = "2")]
    pub log_id: u64,
    #[prost(int32, tag = "3")]
    pub service: i32,
    #[prost(int32, tag = "4")]
    pub method: i32,
    #[prost(message, repeated, tag = "5")]
    pub headers: Vec<FrameHeader>,
    #[prost(string, tag = "6")]
    pub payload_encoding: String,
    #[prost(string, tag = "7")]
    pub payload_type: String,
    #[prost(bytes = "vec", tag = "8")]
    pub payload: Vec<u8>,
    #[prost(string, tag = "9")]
    pub log_id_new: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct FrameHeader {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

impl Frame {
    pub fn get_header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.key == key)
            .map(|h| h.value.as_str())
    }

    pub fn set_header(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        if let Some(h) = self.headers.iter_mut().find(|h| h.key == key) {
            h.value = value.into();
        } else {
            self.headers.push(FrameHeader {
                key,
                value: value.into(),
            });
        }
    }
}

pub fn new_ping_frame(service_id: i32) -> Frame {
    Frame {
        seq_id: 0,
        log_id: 0,
        service: service_id,
        method: FRAME_METHOD_CONTROL,
        headers: vec![FrameHeader {
            key: "type".to_string(),
            value: "ping".to_string(),
        }],
        payload_encoding: String::new(),
        payload_type: String::new(),
        payload: vec![],
        log_id_new: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let frame = Frame {
            seq_id: 1,
            log_id: 0,
            service: 100,
            method: FRAME_METHOD_DATA,
            headers: vec![
                FrameHeader {
                    key: "type".to_string(),
                    value: "event".to_string(),
                },
                FrameHeader {
                    key: "message_id".to_string(),
                    value: "msg_001".to_string(),
                },
            ],
            payload_encoding: String::new(),
            payload_type: String::new(),
            payload: br#"{"test": true}"#.to_vec(),
            log_id_new: String::new(),
        };

        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        assert!(!buf.is_empty());

        let decoded = Frame::decode(&buf[..]).unwrap();
        assert_eq!(decoded.seq_id, 1);
        assert_eq!(decoded.method, FRAME_METHOD_DATA);
        assert_eq!(decoded.headers.len(), 2);
        assert_eq!(decoded.payload, br#"{"test": true}"#);
    }

    #[test]
    fn ping_frame_has_correct_structure() {
        let frame = new_ping_frame(42);
        assert_eq!(frame.method, FRAME_METHOD_CONTROL);
        assert_eq!(frame.service, 42);
        let type_header = frame.headers.iter().find(|h| h.key == "type").unwrap();
        assert_eq!(type_header.value, "ping");
    }

    #[test]
    fn frame_get_header() {
        let frame = Frame {
            seq_id: 0,
            log_id: 0,
            service: 0,
            method: FRAME_METHOD_DATA,
            headers: vec![FrameHeader {
                key: "type".to_string(),
                value: "event".to_string(),
            }],
            payload_encoding: String::new(),
            payload_type: String::new(),
            payload: vec![],
            log_id_new: String::new(),
        };
        assert_eq!(frame.get_header("type"), Some("event"));
        assert_eq!(frame.get_header("missing"), None);
    }
}
