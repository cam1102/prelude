use std::collections::BTreeMap;

use crate::{Error, Result};

pub type HeaderMap = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtspLimits {
    pub maximum_header_bytes: usize,
    pub maximum_body_bytes: usize,
    pub maximum_buffer_bytes: usize,
}

impl Default for RtspLimits {
    fn default() -> Self {
        Self {
            maximum_header_bytes: 32 * 1024,
            maximum_body_bytes: 4 * 1024 * 1024,
            maximum_buffer_bytes: 5 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspRequest {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspResponse {
    pub status: u16,
    pub reason: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventChannelMessage {
    Request(RtspRequest),
    Response(RtspResponse),
}

impl RtspResponse {
    pub fn new(status: u16, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
            headers: HeaderMap::new(),
            body: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct RtspCodec {
    limits: RtspLimits,
    buffer: Vec<u8>,
}

#[derive(Debug)]
pub struct EventChannelCodec {
    limits: RtspLimits,
    buffer: Vec<u8>,
}

impl EventChannelCodec {
    pub fn new(limits: RtspLimits) -> Result<Self> {
        RtspCodec::new(limits)?;
        Ok(Self {
            limits,
            buffer: Vec::new(),
        })
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<EventChannelMessage>> {
        let new_length =
            self.buffer
                .len()
                .checked_add(bytes.len())
                .ok_or(Error::MessageTooLarge {
                    section: "event buffer",
                    actual: usize::MAX,
                    maximum: self.limits.maximum_buffer_bytes,
                })?;
        if new_length > self.limits.maximum_buffer_bytes {
            return Err(Error::MessageTooLarge {
                section: "event buffer",
                actual: new_length,
                maximum: self.limits.maximum_buffer_bytes,
            });
        }
        self.buffer.extend_from_slice(bytes);
        let mut messages = Vec::new();
        loop {
            let Some(header_end) = find_bytes(&self.buffer, b"\r\n\r\n") else {
                if self.buffer.len() > self.limits.maximum_header_bytes {
                    return Err(Error::MessageTooLarge {
                        section: "event headers",
                        actual: self.buffer.len(),
                        maximum: self.limits.maximum_header_bytes,
                    });
                }
                break;
            };
            if header_end + 4 > self.limits.maximum_header_bytes {
                return Err(Error::MessageTooLarge {
                    section: "event headers",
                    actual: header_end + 4,
                    maximum: self.limits.maximum_header_bytes,
                });
            }
            let (start_line, headers) = parse_head(&self.buffer[..header_end])?;
            let content_length = parse_content_length(&headers)?;
            if content_length > self.limits.maximum_body_bytes {
                return Err(Error::MessageTooLarge {
                    section: "event body",
                    actual: content_length,
                    maximum: self.limits.maximum_body_bytes,
                });
            }
            let body_start = header_end + 4;
            let message_end = body_start
                .checked_add(content_length)
                .ok_or_else(|| Error::InvalidRtsp("event Content-Length overflow".to_owned()))?;
            if self.buffer.len() < message_end {
                break;
            }
            let body = self.buffer[body_start..message_end].to_vec();
            messages.push(parse_event_message(&start_line, headers, body)?);
            self.buffer.drain(..message_end);
        }
        Ok(messages)
    }

    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }
}

impl RtspCodec {
    pub fn new(limits: RtspLimits) -> Result<Self> {
        if limits.maximum_header_bytes < 4
            || limits.maximum_buffer_bytes < limits.maximum_header_bytes
            || limits.maximum_buffer_bytes < limits.maximum_body_bytes
        {
            return Err(Error::InvalidRtsp("invalid parser limits".to_owned()));
        }
        Ok(Self {
            limits,
            buffer: Vec::new(),
        })
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<RtspRequest>> {
        let new_length =
            self.buffer
                .len()
                .checked_add(bytes.len())
                .ok_or(Error::MessageTooLarge {
                    section: "buffer",
                    actual: usize::MAX,
                    maximum: self.limits.maximum_buffer_bytes,
                })?;
        if new_length > self.limits.maximum_buffer_bytes {
            return Err(Error::MessageTooLarge {
                section: "buffer",
                actual: new_length,
                maximum: self.limits.maximum_buffer_bytes,
            });
        }
        self.buffer.extend_from_slice(bytes);
        let mut messages = Vec::new();
        loop {
            let Some(header_end) = find_bytes(&self.buffer, b"\r\n\r\n") else {
                if self.buffer.len() > self.limits.maximum_header_bytes {
                    return Err(Error::MessageTooLarge {
                        section: "headers",
                        actual: self.buffer.len(),
                        maximum: self.limits.maximum_header_bytes,
                    });
                }
                break;
            };
            if header_end + 4 > self.limits.maximum_header_bytes {
                return Err(Error::MessageTooLarge {
                    section: "headers",
                    actual: header_end + 4,
                    maximum: self.limits.maximum_header_bytes,
                });
            }
            let (request_line, headers) = parse_head(&self.buffer[..header_end])?;
            let content_length = parse_content_length(&headers)?;
            if content_length > self.limits.maximum_body_bytes {
                return Err(Error::MessageTooLarge {
                    section: "body",
                    actual: content_length,
                    maximum: self.limits.maximum_body_bytes,
                });
            }
            let body_start = header_end + 4;
            let message_end = body_start
                .checked_add(content_length)
                .ok_or_else(|| Error::InvalidRtsp("Content-Length overflow".to_owned()))?;
            if self.buffer.len() < message_end {
                break;
            }
            let body = self.buffer[body_start..message_end].to_vec();
            let mut parts = request_line.split(' ');
            let method = parts.next().unwrap_or_default();
            let target = parts.next().unwrap_or_default();
            let version = parts.next().unwrap_or_default();
            if method.is_empty()
                || target.is_empty()
                || version != "RTSP/1.0"
                || parts.next().is_some()
                || !method
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
            {
                return Err(Error::InvalidRtsp("invalid request line".to_owned()));
            }
            messages.push(RtspRequest {
                method: method.to_owned(),
                target: target.to_owned(),
                version: version.to_owned(),
                headers,
                body,
            });
            self.buffer.drain(..message_end);
        }
        Ok(messages)
    }

    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }
}

fn parse_content_length(headers: &HeaderMap) -> Result<usize> {
    match headers.get("content-length") {
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| Error::InvalidRtsp("invalid Content-Length".to_owned())),
        None => Ok(0),
    }
}

fn parse_event_message(
    start_line: &str,
    headers: HeaderMap,
    body: Vec<u8>,
) -> Result<EventChannelMessage> {
    let mut parts = start_line.splitn(3, ' ');
    let first = parts.next().unwrap_or_default();
    let second = parts.next().unwrap_or_default();
    let third = parts.next().unwrap_or_default();
    if first == "RTSP/1.0" || first == "HTTP/1.1" {
        let status = second
            .parse::<u16>()
            .map_err(|_| Error::InvalidRtsp("invalid event response status".to_owned()))?;
        if !(100..=599).contains(&status)
            || third.is_empty()
            || !third.is_ascii()
            || third.contains(['\r', '\n'])
        {
            return Err(Error::InvalidRtsp("invalid event response line".to_owned()));
        }
        return Ok(EventChannelMessage::Response(RtspResponse {
            status,
            reason: third.to_owned(),
            headers,
            body,
        }));
    }
    if first.is_empty()
        || second.is_empty()
        || third != "RTSP/1.0"
        || !first
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
    {
        return Err(Error::InvalidRtsp("invalid event request line".to_owned()));
    }
    Ok(EventChannelMessage::Request(RtspRequest {
        method: first.to_owned(),
        target: second.to_owned(),
        version: third.to_owned(),
        headers,
        body,
    }))
}

fn parse_head(head: &[u8]) -> Result<(String, HeaderMap)> {
    let text = std::str::from_utf8(head)
        .map_err(|_| Error::InvalidRtsp("headers are not valid UTF-8".to_owned()))?;
    let mut lines = text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| Error::InvalidRtsp("missing request line".to_owned()))?;
    if !request_line.is_ascii() {
        return Err(Error::InvalidRtsp(
            "request line contains non-ASCII data".to_owned(),
        ));
    }
    let mut headers = HeaderMap::new();
    for line in lines {
        if line.is_empty() || line.starts_with([' ', '\t']) {
            return Err(Error::InvalidRtsp("empty or folded header line".to_owned()));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| Error::InvalidRtsp("header has no colon".to_owned()))?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(Error::InvalidRtsp("invalid header name".to_owned()));
        }
        let name = name.to_ascii_lowercase();
        let value = value.trim();
        if name == "x-apple-client-name" {
            if value.chars().any(char::is_control) {
                return Err(Error::InvalidRtsp(
                    "X-Apple-Client-Name contains control data".to_owned(),
                ));
            }
        } else if !value.is_ascii() {
            return Err(Error::InvalidRtsp(format!(
                "header {name} contains non-ASCII data"
            )));
        }
        if headers.insert(name, value.to_owned()).is_some() {
            return Err(Error::InvalidRtsp("duplicate header".to_owned()));
        }
    }
    Ok((request_line.to_owned(), headers))
}

pub fn encode_response(request: &RtspRequest, response: &RtspResponse) -> Result<Vec<u8>> {
    if !(100..=599).contains(&response.status)
        || response.reason.contains(['\r', '\n'])
        || !response.reason.is_ascii()
    {
        return Err(Error::InvalidRtsp("invalid response status".to_owned()));
    }
    let mut headers: BTreeMap<&str, &str> = response
        .headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    if let Some(cseq) = request.headers.get("cseq") {
        headers.insert("cseq", cseq);
    }
    let content_length = response.body.len().to_string();
    headers.insert("content-length", &content_length);
    let mut encoded = format!(
        "{} {} {}\r\n",
        request.version, response.status, response.reason
    )
    .into_bytes();
    for (name, value) in headers {
        if name.contains(['\r', '\n', ':']) || value.contains(['\r', '\n']) {
            return Err(Error::InvalidRtsp("response header injection".to_owned()));
        }
        encoded.extend_from_slice(name.as_bytes());
        encoded.extend_from_slice(b": ");
        encoded.extend_from_slice(value.as_bytes());
        encoded.extend_from_slice(b"\r\n");
    }
    encoded.extend_from_slice(b"\r\n");
    encoded.extend_from_slice(&response.body);
    Ok(encoded)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn incremental_parser_handles_pipelining() {
        let mut codec = RtspCodec::new(RtspLimits::default()).expect("codec");
        assert!(
            codec
                .push(b"GET /info RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 2\r\n\r\n")
                .expect("partial")
                .is_empty()
        );
        let messages = codec
            .push(b"okOPTIONS * RTSP/1.0\r\nCSeq: 2\r\n\r\n")
            .expect("messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].body, b"ok");
        assert_eq!(messages[1].method, "OPTIONS");
    }

    #[test]
    fn event_codec_accepts_interleaved_requests_and_responses() {
        let mut codec = EventChannelCodec::new(RtspLimits::default()).expect("codec");
        let wire = b"RTSP/1.0 200 OK\r\nCSeq: 7\r\nContent-Length: 0\r\n\r\nPOST /command RTSP/1.0\r\nCSeq: 9\r\nContent-Length: 3\r\n\r\nabc";
        let first = codec.push(&wire[..21]).expect("partial");
        assert!(first.is_empty());
        let messages = codec.push(&wire[21..]).expect("complete");
        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[0],
            EventChannelMessage::Response(response)
                if response.status == 200 && response.headers.get("cseq") == Some(&"7".to_owned())
        ));
        assert!(matches!(
            &messages[1],
            EventChannelMessage::Request(request)
                if request.target == "/command" && request.body == b"abc"
        ));
        assert_eq!(codec.buffered_len(), 0);
    }

    #[test]
    fn event_codec_rejects_malformed_response_status() {
        let mut codec = EventChannelCodec::new(RtspLimits::default()).expect("codec");
        assert!(
            codec
                .push(b"RTSP/1.0 999 Bad\r\nContent-Length: 0\r\n\r\n")
                .is_err()
        );
    }

    #[test]
    fn duplicate_content_length_is_rejected() {
        let mut codec = RtspCodec::new(RtspLimits::default()).expect("codec");
        let result =
            codec.push(b"POST /x RTSP/1.0\r\nContent-Length: 1\r\ncontent-length: 2\r\n\r\nxx");
        assert!(matches!(result, Err(Error::InvalidRtsp(_))));
    }

    #[test]
    fn apple_client_name_accepts_well_formed_utf8() {
        let mut codec = RtspCodec::new(RtspLimits::default()).expect("codec");
        let messages = codec
            .push(
                "POST /pair-setup RTSP/1.0\r\nX-Apple-Client-Name: Neel’s iPhone\r\nContent-Length: 0\r\n\r\n"
                    .as_bytes(),
            )
            .expect("UTF-8 Apple client name");
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].headers.get("x-apple-client-name"),
            Some(&"Neel’s iPhone".to_owned())
        );
    }

    #[test]
    fn non_ascii_remains_rejected_in_other_headers() {
        let mut codec = RtspCodec::new(RtspLimits::default()).expect("codec");
        let error = codec
            .push("GET /info RTSP/1.0\r\nX-Other: café\r\n\r\n".as_bytes())
            .expect_err("non-ASCII generic header");
        assert!(matches!(error, Error::InvalidRtsp(_)));
    }

    #[test]
    fn apple_client_name_rejects_control_data() {
        let mut codec = RtspCodec::new(RtspLimits::default()).expect("codec");
        let error = codec
            .push(b"GET /info RTSP/1.0\r\nX-Apple-Client-Name: phone\0name\r\n\r\n")
            .expect_err("control data");
        assert!(matches!(error, Error::InvalidRtsp(_)));
    }

    #[test]
    fn response_echoes_cseq_and_sets_length() {
        let request = RtspRequest {
            method: "GET".to_owned(),
            target: "/info".to_owned(),
            version: "RTSP/1.0".to_owned(),
            headers: HeaderMap::from([("cseq".to_owned(), "7".to_owned())]),
            body: Vec::new(),
        };
        let mut response = RtspResponse::new(200, "OK");
        response.body = b"yes".to_vec();
        let wire = encode_response(&request, &response).expect("response");
        let text = String::from_utf8(wire).expect("ASCII");
        assert!(text.contains("cseq: 7\r\n"));
        assert!(text.contains("content-length: 3\r\n"));
    }

    proptest! {
        #[test]
        fn parser_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
            let limits = RtspLimits {
                maximum_header_bytes: 1024,
                maximum_body_bytes: 2048,
                maximum_buffer_bytes: 4096,
            };
            let mut codec = RtspCodec::new(limits).expect("codec");
            let _ = codec.push(&bytes);
        }
    }
}
