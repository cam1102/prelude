use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoFormat {
    pub width: u16,
    pub height: u16,
    pub maximum_fps: u16,
}

impl VideoFormat {
    pub fn validate(self) -> Result<Self> {
        if self.width == 0 || self.height == 0 || self.maximum_fps == 0 {
            return Err(Error::InvalidVideo(
                "dimensions and frame rate must be non-zero".to_owned(),
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedVideoFrame {
    pub data: Vec<u8>,
    pub presentation_time_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264AccessUnit {
    frame: EncodedVideoFrame,
    nal_unit_types: Vec<u8>,
}

impl H264AccessUnit {
    pub fn parse_annex_b(frame: EncodedVideoFrame) -> Result<Self> {
        let starts = annex_b_starts(&frame.data);
        if starts.is_empty() {
            return Err(Error::InvalidVideo(
                "H.264 access unit has no Annex B start code".to_owned(),
            ));
        }
        let mut nal_unit_types = Vec::with_capacity(starts.len());
        for (index, (payload_start, _)) in starts.iter().copied().enumerate() {
            let end = starts
                .get(index + 1)
                .map_or(frame.data.len(), |(next, prefix)| next - prefix);
            if payload_start >= end {
                return Err(Error::InvalidVideo("empty H.264 NAL unit".to_owned()));
            }
            let unit_type = frame.data[payload_start] & 0x1f;
            if unit_type == 0 || unit_type >= 24 {
                return Err(Error::InvalidVideo(format!(
                    "unsupported H.264 NAL unit type {unit_type}"
                )));
            }
            nal_unit_types.push(unit_type);
        }
        Ok(Self {
            frame,
            nal_unit_types,
        })
    }

    pub fn encoded(&self) -> &EncodedVideoFrame {
        &self.frame
    }

    pub fn nal_unit_types(&self) -> &[u8] {
        &self.nal_unit_types
    }

    pub fn is_key_frame(&self) -> bool {
        self.nal_unit_types.contains(&5)
    }

    pub fn has_parameter_sets(&self) -> bool {
        self.nal_unit_types.contains(&7) && self.nal_unit_types.contains(&8)
    }
}

fn annex_b_starts(data: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut offset = 0;
    while offset + 3 <= data.len() {
        let prefix = if data.get(offset..offset + 4) == Some(&[0, 0, 0, 1]) {
            4
        } else if data.get(offset..offset + 3) == Some(&[0, 0, 1]) {
            3
        } else {
            offset += 1;
            continue;
        };
        starts.push((offset + prefix, prefix));
        offset += prefix;
    }
    starts
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedVideoFrame {
    pub width: u16,
    pub height: u16,
    pub presentation_time_micros: u64,
    pub pixels: Vec<u8>,
}

pub trait VideoDecoder: std::fmt::Debug + Send {
    fn configure(&mut self, format: VideoFormat) -> Result<()>;
    fn decode(&mut self, access_unit: &H264AccessUnit) -> Result<Option<DecodedVideoFrame>>;
    fn flush(&mut self) -> Result<()>;
}

pub trait VideoRenderer: std::fmt::Debug + Send {
    fn present(&mut self, frame: &DecodedVideoFrame) -> Result<()>;
}

#[derive(Debug, Default)]
pub struct UnsupportedVideoDecoder;

impl VideoDecoder for UnsupportedVideoDecoder {
    fn configure(&mut self, _format: VideoFormat) -> Result<()> {
        Err(Error::Unsupported("no H.264 decoder backend is configured"))
    }

    fn decode(&mut self, _access_unit: &H264AccessUnit) -> Result<Option<DecodedVideoFrame>> {
        Err(Error::Unsupported("no H.264 decoder backend is configured"))
    }

    fn flush(&mut self) -> Result<()> {
        Err(Error::Unsupported("no H.264 decoder backend is configured"))
    }
}

#[derive(Debug, Default)]
pub struct UnsupportedVideoRenderer;

impl VideoRenderer for UnsupportedVideoRenderer {
    fn present(&mut self, _frame: &DecodedVideoFrame) -> Result<()> {
        Err(Error::Unsupported("no display renderer is configured"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_parser_finds_parameter_sets_and_idr() {
        let access_unit = H264AccessUnit::parse_annex_b(EncodedVideoFrame {
            data: vec![
                0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4,
            ],
            presentation_time_micros: 42,
        })
        .expect("access unit");
        assert_eq!(access_unit.nal_unit_types(), &[7, 8, 5]);
        assert!(access_unit.has_parameter_sets());
        assert!(access_unit.is_key_frame());
    }

    #[test]
    fn missing_start_code_is_rejected() {
        assert!(matches!(
            H264AccessUnit::parse_annex_b(EncodedVideoFrame {
                data: vec![0x65, 1, 2],
                presentation_time_micros: 0,
            }),
            Err(Error::InvalidVideo(_))
        ));
    }

    #[test]
    fn placeholder_decoder_reports_unsupported() {
        assert!(matches!(
            UnsupportedVideoDecoder.configure(VideoFormat {
                width: 800,
                height: 480,
                maximum_fps: 60,
            }),
            Err(Error::Unsupported(_))
        ));
    }
}
