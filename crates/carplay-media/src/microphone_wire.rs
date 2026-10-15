use zeroize::{Zeroize, Zeroizing};

use crate::{
    Error, Result,
    audio_wire::{AudioStreamId, RTP_HEADER_LENGTH},
    wire_crypto,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneCodec {
    PcmS16,
    RawPcmS16Le,
    Opus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicrophoneConfig {
    pub stream_id: AudioStreamId,
    pub codec: MicrophoneCodec,
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub frame_duration_micros: u32,
    pub maximum_encoded_frame_bytes: usize,
    pub maximum_pcm_buffer_bytes: usize,
}

impl MicrophoneConfig {
    pub fn validate(self) -> Result<Self> {
        if self.stream_id != AudioStreamId::Main {
            return Err(Error::InvalidAudio(
                "CarPlay microphone uplink requires MainAudio stream type 100".to_owned(),
            ));
        }
        if !matches!(
            self.sample_rate_hz,
            8_000 | 16_000 | 24_000 | 32_000 | 44_100 | 48_000
        ) || !(1..=2).contains(&self.channels)
            || !matches!(
                self.frame_duration_micros,
                2_500 | 5_000 | 10_000 | 20_000 | 40_000 | 60_000
            )
            || self.maximum_encoded_frame_bytes == 0
        {
            return Err(Error::InvalidAudio(
                "invalid microphone sample rate, channels, frame duration, or frame limit"
                    .to_owned(),
            ));
        }
        if self.codec == MicrophoneCodec::Opus
            && !matches!(self.sample_rate_hz, 16_000 | 24_000 | 48_000)
        {
            return Err(Error::Unsupported(
                "CarPlay Opus microphone framing supports 16, 24, or 48 kHz",
            ));
        }
        let frame_bytes = self.pcm_frame_bytes()?;
        if self.maximum_pcm_buffer_bytes < frame_bytes
            || self.maximum_pcm_buffer_bytes < self.maximum_encoded_frame_bytes
        {
            return Err(Error::InvalidAudio(
                "microphone buffer limit is smaller than one frame".to_owned(),
            ));
        }
        Ok(self)
    }

    pub fn samples_per_frame(self) -> Result<u32> {
        let numerator = u64::from(self.sample_rate_hz) * u64::from(self.frame_duration_micros);
        if numerator % 1_000_000 != 0 {
            return Err(Error::InvalidAudio(
                "microphone duration is not integral at the sample rate".to_owned(),
            ));
        }
        u32::try_from(numerator / 1_000_000)
            .map_err(|_| Error::InvalidAudio("microphone frame sample count overflow".to_owned()))
    }

    pub fn pcm_frame_bytes(self) -> Result<usize> {
        usize::try_from(self.samples_per_frame()?)
            .ok()
            .and_then(|samples| samples.checked_mul(usize::from(self.channels)))
            .and_then(|samples| samples.checked_mul(2))
            .ok_or_else(|| Error::InvalidAudio("microphone frame length overflow".to_owned()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MicrophoneCounters {
    pub sequence: u16,
    pub timestamp: u32,
    pub nonce: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub struct MicrophonePacket {
    pub sequence: u16,
    pub timestamp: u32,
    pub nonce_counter: u64,
    pub samples: u32,
    pub encoded: Vec<u8>,
}

impl std::fmt::Debug for MicrophonePacket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MicrophonePacket")
            .field("sequence", &self.sequence)
            .field("timestamp", &self.timestamp)
            .field("nonce_counter", &self.nonce_counter)
            .field("samples", &self.samples)
            .field(
                "encoded",
                &format_args!("<redacted:{} bytes>", self.encoded.len()),
            )
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophonePacketizerState {
    Active,
    Stopped,
    Failed,
}

impl MicrophonePacketizerState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

pub struct MicrophonePacketizer {
    key: Option<Zeroizing<[u8; 32]>>,
    config: MicrophoneConfig,
    samples_per_frame: u32,
    pcm_frame_bytes: usize,
    pcm_buffer: Vec<u8>,
    counters: MicrophoneCounters,
    state: MicrophonePacketizerState,
}

impl std::fmt::Debug for MicrophonePacketizer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MicrophonePacketizer")
            .field("key", &"<redacted>")
            .field("config", &self.config)
            .field("samples_per_frame", &self.samples_per_frame)
            .field("pcm_frame_bytes", &self.pcm_frame_bytes)
            .field("buffered_pcm_bytes", &self.pcm_buffer.len())
            .field("counters", &self.counters)
            .field("state", &self.state)
            .finish()
    }
}

impl MicrophonePacketizer {
    pub fn new(key: [u8; 32], config: MicrophoneConfig) -> Result<Self> {
        Self::with_counters(key, config, MicrophoneCounters::default())
    }

    pub fn with_counters(
        key: [u8; 32],
        config: MicrophoneConfig,
        counters: MicrophoneCounters,
    ) -> Result<Self> {
        let config = config.validate()?;
        Ok(Self {
            key: Some(Zeroizing::new(key)),
            samples_per_frame: config.samples_per_frame()?,
            pcm_frame_bytes: config.pcm_frame_bytes()?,
            config,
            pcm_buffer: Vec::new(),
            counters,
            state: MicrophonePacketizerState::Active,
        })
    }

    pub fn new_raw_pcm_s16le(config: MicrophoneConfig) -> Result<Self> {
        if config.codec != MicrophoneCodec::RawPcmS16Le {
            return Err(Error::InvalidAudio(
                "raw microphone packetizer requires RawPcmS16Le".to_owned(),
            ));
        }
        let config = config.validate()?;
        Ok(Self {
            key: None,
            samples_per_frame: config.samples_per_frame()?,
            pcm_frame_bytes: config.pcm_frame_bytes()?,
            config,
            pcm_buffer: Vec::new(),
            counters: MicrophoneCounters::default(),
            state: MicrophonePacketizerState::Active,
        })
    }

    pub fn state(&self) -> MicrophonePacketizerState {
        self.state
    }

    pub fn config(&self) -> MicrophoneConfig {
        self.config
    }

    pub fn buffered_pcm_bytes(&self) -> usize {
        self.pcm_buffer.len()
    }

    pub fn push_pcm_s16le(&mut self, pcm: &[u8]) -> Result<Vec<MicrophonePacket>> {
        self.require_active()?;
        if !matches!(
            self.config.codec,
            MicrophoneCodec::PcmS16 | MicrophoneCodec::RawPcmS16Le
        ) {
            return Err(Error::Unsupported(
                "Opus microphone framing requires externally encoded Opus frames",
            ));
        }
        if pcm.len() > self.config.maximum_pcm_buffer_bytes {
            return self.fail(Error::InvalidAudio(
                "microphone PCM input chunk exceeds configured maximum".to_owned(),
            ));
        }
        let total = self
            .pcm_buffer
            .len()
            .checked_add(pcm.len())
            .ok_or_else(|| Error::InvalidAudio("microphone PCM buffer overflow".to_owned()))?;
        let maximum_working = self
            .config
            .maximum_pcm_buffer_bytes
            .checked_add(self.pcm_frame_bytes)
            .ok_or_else(|| Error::InvalidAudio("microphone buffer limit overflow".to_owned()))?;
        if total > maximum_working {
            return self.fail(Error::InvalidAudio(
                "microphone PCM buffer exceeds configured maximum".to_owned(),
            ));
        }
        self.pcm_buffer.extend_from_slice(pcm);
        let frame_count = self.pcm_buffer.len() / self.pcm_frame_bytes;
        let mut packets = Vec::with_capacity(frame_count);
        for _ in 0..frame_count {
            let mut frame = self.pcm_buffer[..self.pcm_frame_bytes].to_vec();
            if self.config.codec == MicrophoneCodec::PcmS16 {
                for sample in frame.chunks_exact_mut(2) {
                    sample.swap(0, 1);
                }
            }
            packets.push(self.packetize(&frame)?);
            self.pcm_buffer.drain(..self.pcm_frame_bytes);
        }
        Ok(packets)
    }

    pub fn packetize_opus(&mut self, encoded_opus: &[u8]) -> Result<MicrophonePacket> {
        self.require_active()?;
        if self.config.codec != MicrophoneCodec::Opus {
            return Err(Error::Unsupported(
                "PCM microphone framing does not accept encoded Opus",
            ));
        }
        if encoded_opus.is_empty() || encoded_opus.len() > self.config.maximum_encoded_frame_bytes {
            return self.fail(Error::InvalidAudio(
                "encoded Opus microphone frame is empty or oversized".to_owned(),
            ));
        }
        self.packetize(encoded_opus)
    }

    pub fn teardown(&mut self) {
        if let Some(key) = self.key.as_mut() {
            key.zeroize();
        }
        self.pcm_buffer.zeroize();
        self.pcm_buffer.clear();
        self.state = MicrophonePacketizerState::Stopped;
    }

    fn packetize(&mut self, payload: &[u8]) -> Result<MicrophonePacket> {
        if self.counters.nonce == u64::MAX {
            return self.fail(Error::InvalidAudio(
                "microphone nonce counter is exhausted".to_owned(),
            ));
        }
        let sequence = self.counters.sequence;
        let timestamp = self.counters.timestamp;
        let nonce_counter = self.counters.nonce;
        let mut header = [0; RTP_HEADER_LENGTH];
        header[0] = 0x80;
        header[1] = self.config.stream_id as u8;
        header[2..4].copy_from_slice(&sequence.to_be_bytes());
        header[4..8].copy_from_slice(&timestamp.to_be_bytes());
        header[8..12].copy_from_slice(&0_u32.to_be_bytes());
        let encoded = if self.config.codec == MicrophoneCodec::RawPcmS16Le {
            payload.to_vec()
        } else {
            let key = self.key.as_ref().ok_or_else(|| {
                Error::InvalidAudio("encrypted microphone packetizer has no key".to_owned())
            })?;
            let sealed = wire_crypto::seal(
                key,
                &wire_crypto::nonce(nonce_counter),
                payload,
                &header[4..],
            )?;
            let mut encoded = Vec::with_capacity(RTP_HEADER_LENGTH + sealed.len() + 8);
            encoded.extend_from_slice(&header);
            encoded.extend_from_slice(&sealed);
            encoded.extend_from_slice(&nonce_counter.to_le_bytes());
            encoded
        };
        self.counters.sequence = sequence.wrapping_add(1);
        self.counters.timestamp = timestamp.wrapping_add(self.samples_per_frame);
        self.counters.nonce += 1;
        Ok(MicrophonePacket {
            sequence,
            timestamp,
            nonce_counter,
            samples: self.samples_per_frame,
            encoded,
        })
    }

    fn require_active(&self) -> Result<()> {
        if self.state != MicrophonePacketizerState::Active {
            return Err(Error::NotReady {
                component: "microphone packetizer",
                state: self.state.as_str(),
            });
        }
        Ok(())
    }

    fn fail<T>(&mut self, error: Error) -> Result<T> {
        if let Some(key) = self.key.as_mut() {
            key.zeroize();
        }
        self.pcm_buffer.zeroize();
        self.pcm_buffer.clear();
        self.state = MicrophonePacketizerState::Failed;
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_wire::{AudioFormatId, AudioWireDecoder, AudioWireEvent, AudioWireLimits};

    const KEY: [u8; 32] = [0x33; 32];

    fn config(codec: MicrophoneCodec) -> MicrophoneConfig {
        MicrophoneConfig {
            stream_id: AudioStreamId::Main,
            codec,
            sample_rate_hz: 16_000,
            channels: 1,
            frame_duration_micros: 20_000,
            maximum_encoded_frame_bytes: 4096,
            maximum_pcm_buffer_bytes: 4096,
        }
    }

    #[test]
    fn incremental_pcm_is_big_endian_framed_and_livi_encrypted() {
        let mut packetizer =
            MicrophonePacketizer::new(KEY, config(MicrophoneCodec::PcmS16)).expect("packetizer");
        let frame_bytes = 320 * 2;
        let mut pcm = vec![0; frame_bytes];
        for sample in pcm.chunks_exact_mut(2) {
            sample.copy_from_slice(&0x1122_u16.to_le_bytes());
        }
        assert!(
            packetizer
                .push_pcm_s16le(&pcm[..111])
                .expect("partial")
                .is_empty()
        );
        let packets = packetizer.push_pcm_s16le(&pcm[111..]).expect("complete");
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].sequence, 0);
        assert_eq!(packets[0].timestamp, 0);
        assert_eq!(packets[0].nonce_counter, 0);

        let mut decoder = AudioWireDecoder::new(
            KEY,
            AudioStreamId::Main,
            AudioFormatId::new(0x10),
            AudioWireLimits::default(),
        )
        .expect("decoder");
        let events = decoder.decode_at(&packets[0].encoded, 1).expect("decode");
        let AudioWireEvent::Frame(frame) = events.last().expect("frame") else {
            panic!("frame")
        };
        assert_eq!(&frame.payload[..2], &0x1122_u16.to_be_bytes());
    }

    #[test]
    fn raw_pcm_packetizer_preserves_little_endian_capture_bytes() {
        let mut packetizer =
            MicrophonePacketizer::new_raw_pcm_s16le(config(MicrophoneCodec::RawPcmS16Le))
                .expect("raw packetizer");
        let frame_bytes = 320 * 2;
        let mut pcm = vec![0; frame_bytes];
        for sample in pcm.chunks_exact_mut(2) {
            sample.copy_from_slice(&0x1122_u16.to_le_bytes());
        }

        let packets = packetizer.push_pcm_s16le(&pcm).expect("raw PCM");
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].encoded, pcm);
        assert_eq!(packets[0].samples, 320);
        assert_eq!(packets[0].sequence, 0);
        assert_eq!(packets[0].timestamp, 0);
    }

    #[test]
    fn opus_requires_external_frames_and_is_not_encoded_here() {
        let mut packetizer =
            MicrophonePacketizer::new(KEY, config(MicrophoneCodec::Opus)).expect("packetizer");
        assert!(matches!(
            packetizer.push_pcm_s16le(&[0; 640]),
            Err(Error::Unsupported(_))
        ));
        let packet = packetizer.packetize_opus(&[1, 2, 3, 4]).expect("Opus");
        let sealed = &packet.encoded[RTP_HEADER_LENGTH..packet.encoded.len() - 8];
        let plaintext = wire_crypto::open(
            &KEY,
            &wire_crypto::nonce(0),
            sealed,
            &packet.encoded[4..RTP_HEADER_LENGTH],
        )
        .expect("open");
        assert_eq!(plaintext, [1, 2, 3, 4]);
    }

    #[test]
    fn sequence_and_timestamp_wrap_without_reusing_nonce() {
        let counters = MicrophoneCounters {
            sequence: u16::MAX,
            timestamp: u32::MAX - 10,
            nonce: 7,
        };
        let mut packetizer =
            MicrophonePacketizer::with_counters(KEY, config(MicrophoneCodec::Opus), counters)
                .expect("packetizer");
        let first = packetizer.packetize_opus(&[1]).expect("first");
        let second = packetizer.packetize_opus(&[2]).expect("second");
        assert_eq!(first.sequence, u16::MAX);
        assert_eq!(second.sequence, 0);
        assert_eq!(second.timestamp, (u32::MAX - 10).wrapping_add(320));
        assert_eq!(second.nonce_counter, 8);
    }

    #[test]
    fn oversize_nonce_exhaustion_and_teardown_are_terminal() {
        let mut limited = config(MicrophoneCodec::Opus);
        limited.maximum_encoded_frame_bytes = 2;
        let mut packetizer = MicrophonePacketizer::new(KEY, limited).expect("packetizer");
        assert!(matches!(
            packetizer.packetize_opus(&[1, 2, 3]),
            Err(Error::InvalidAudio(_))
        ));
        assert_eq!(packetizer.state(), MicrophonePacketizerState::Failed);

        let counters = MicrophoneCounters {
            nonce: u64::MAX,
            ..MicrophoneCounters::default()
        };
        let mut packetizer =
            MicrophonePacketizer::with_counters(KEY, config(MicrophoneCodec::Opus), counters)
                .expect("packetizer");
        assert!(matches!(
            packetizer.packetize_opus(&[1]),
            Err(Error::InvalidAudio(_))
        ));

        let mut packetizer =
            MicrophonePacketizer::new(KEY, config(MicrophoneCodec::PcmS16)).expect("packetizer");
        packetizer.teardown();
        assert!(matches!(
            packetizer.push_pcm_s16le(&[0; 640]),
            Err(Error::NotReady { .. })
        ));
    }
}
