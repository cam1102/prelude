use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PcmFormat {
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub bits_per_sample: u8,
}

impl PcmFormat {
    pub fn validate(self) -> Result<Self> {
        if !(8_000..=192_000).contains(&self.sample_rate_hz) {
            return Err(Error::InvalidAudio(
                "sample rate is outside 8 kHz..=192 kHz".to_owned(),
            ));
        }
        if !(1..=8).contains(&self.channels) {
            return Err(Error::InvalidAudio(
                "channel count is outside 1..=8".to_owned(),
            ));
        }
        if !matches!(self.bits_per_sample, 16 | 24 | 32) {
            return Err(Error::InvalidAudio(
                "only 16-, 24-, and 32-bit integer PCM are represented".to_owned(),
            ));
        }
        Ok(self)
    }

    pub fn frame_bytes(self) -> usize {
        usize::from(self.channels) * usize::from(self.bits_per_sample / 8)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub format: PcmFormat,
    pub presentation_time_micros: u64,
    pub data: Vec<u8>,
}

impl AudioPacket {
    pub fn validate(self) -> Result<Self> {
        let format = self.format.validate()?;
        if self.data.is_empty() || self.data.len() % format.frame_bytes() != 0 {
            return Err(Error::InvalidAudio(
                "PCM payload is empty or not frame-aligned".to_owned(),
            ));
        }
        Ok(Self { format, ..self })
    }
}

pub trait AudioSink: std::fmt::Debug + Send {
    fn configure(&mut self, format: PcmFormat) -> Result<()>;
    fn write(&mut self, packet: &AudioPacket) -> Result<()>;
    fn drain(&mut self) -> Result<()>;
}

pub trait MicrophoneSource: std::fmt::Debug + Send {
    fn format(&self) -> Result<PcmFormat>;
    fn read(&mut self) -> Result<AudioPacket>;
}

#[derive(Debug, Default)]
pub struct UnsupportedAudioSink;

impl AudioSink for UnsupportedAudioSink {
    fn configure(&mut self, _format: PcmFormat) -> Result<()> {
        Err(Error::Unsupported("no PCM audio sink is configured"))
    }

    fn write(&mut self, _packet: &AudioPacket) -> Result<()> {
        Err(Error::Unsupported("no PCM audio sink is configured"))
    }

    fn drain(&mut self) -> Result<()> {
        Err(Error::Unsupported("no PCM audio sink is configured"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioMixer {
    main_gain_q15: i16,
    alternate_gain_q15: i16,
}

impl AudioMixer {
    pub const UNITY_GAIN_Q15: i16 = i16::MAX;

    pub fn new(main_gain_q15: i16, alternate_gain_q15: i16) -> Self {
        Self {
            main_gain_q15,
            alternate_gain_q15,
        }
    }

    pub fn mix_i16(&self, main: &[i16], alternate: &[i16]) -> Result<Vec<i16>> {
        if main.len() != alternate.len() {
            return Err(Error::InvalidAudio(
                "main and alternate buses have different sample counts".to_owned(),
            ));
        }
        Ok(main
            .iter()
            .zip(alternate)
            .map(|(&main, &alternate)| {
                let main = i32::from(main) * i32::from(self.main_gain_q15);
                let alternate = i32::from(alternate) * i32::from(self.alternate_gain_q15);
                let mixed = (main + alternate) / i32::from(Self::UNITY_GAIN_Q15);
                mixed.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
            })
            .collect())
    }
}

impl Default for AudioMixer {
    fn default() -> Self {
        Self::new(Self::UNITY_GAIN_Q15, Self::UNITY_GAIN_Q15)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixer_includes_alternate_and_saturates() {
        let output = AudioMixer::default()
            .mix_i16(&[30_000, -30_000, 0], &[10_000, -10_000, 2_000])
            .expect("mix");
        assert_eq!(output, [i16::MAX, i16::MIN, 2_000]);
    }

    #[test]
    fn pcm_payload_must_be_frame_aligned() {
        let packet = AudioPacket {
            format: PcmFormat {
                sample_rate_hz: 48_000,
                channels: 2,
                bits_per_sample: 16,
            },
            presentation_time_micros: 0,
            data: vec![0; 3],
        };
        assert!(matches!(packet.validate(), Err(Error::InvalidAudio(_))));
    }
}
