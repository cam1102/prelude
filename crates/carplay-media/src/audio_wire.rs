use zeroize::{Zeroize, Zeroizing};

use crate::{Error, Result, wire_crypto};

pub const RTP_HEADER_LENGTH: usize = 12;
pub const AIRPLAY_AUDIO_TAIL_LENGTH: usize = wire_crypto::TAG_LENGTH + 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum AudioStreamId {
    Main = 100,
    Alternate = 101,
    High = 102,
}

impl TryFrom<u8> for AudioStreamId {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            100 => Ok(Self::Main),
            101 => Ok(Self::Alternate),
            102 => Ok(Self::High),
            _ => Err(Error::InvalidAudio(format!(
                "unsupported CarPlay audio stream type {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AudioFormatId(u64);

impl AudioFormatId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn codec(self) -> Result<EncodedAudioCodec> {
        let codec = match self.0 {
            0x4 => EncodedAudioCodec::PcmS16Be {
                sample_rate_hz: 8_000,
                channels: 1,
            },
            0x8 => EncodedAudioCodec::PcmS16Be {
                sample_rate_hz: 8_000,
                channels: 2,
            },
            0x10 => EncodedAudioCodec::PcmS16Be {
                sample_rate_hz: 16_000,
                channels: 1,
            },
            0x20 => EncodedAudioCodec::PcmS16Be {
                sample_rate_hz: 16_000,
                channels: 2,
            },
            0x40 => EncodedAudioCodec::PcmS16Be {
                sample_rate_hz: 24_000,
                channels: 1,
            },
            0x80 => EncodedAudioCodec::PcmS16Be {
                sample_rate_hz: 24_000,
                channels: 2,
            },
            0x100 => EncodedAudioCodec::PcmS16Be {
                sample_rate_hz: 32_000,
                channels: 1,
            },
            0x200 => EncodedAudioCodec::PcmS16Be {
                sample_rate_hz: 32_000,
                channels: 2,
            },
            0x400 => EncodedAudioCodec::PcmS16Be {
                sample_rate_hz: 44_100,
                channels: 1,
            },
            0x800 => EncodedAudioCodec::PcmS16Be {
                sample_rate_hz: 44_100,
                channels: 2,
            },
            0x4000 => EncodedAudioCodec::PcmS16Be {
                sample_rate_hz: 48_000,
                channels: 1,
            },
            0x8000 => EncodedAudioCodec::PcmS16Be {
                sample_rate_hz: 48_000,
                channels: 2,
            },
            0x40_0000 => EncodedAudioCodec::AacLc {
                sample_rate_hz: 44_100,
                channels: 2,
            },
            0x80_0000 => EncodedAudioCodec::AacLc {
                sample_rate_hz: 48_000,
                channels: 2,
            },
            0x1000_0000 => EncodedAudioCodec::Opus {
                sample_rate_hz: 16_000,
                channels: 1,
            },
            0x2000_0000 => EncodedAudioCodec::Opus {
                sample_rate_hz: 24_000,
                channels: 1,
            },
            0x4000_0000 => EncodedAudioCodec::Opus {
                sample_rate_hz: 48_000,
                channels: 1,
            },
            _ => {
                return Err(Error::Unsupported(
                    "negotiated CarPlay audio format has no wire representation",
                ));
            }
        };
        Ok(codec)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodedAudioCodec {
    PcmS16Be { sample_rate_hz: u32, channels: u8 },
    AacLc { sample_rate_hz: u32, channels: u8 },
    Opus { sample_rate_hz: u32, channels: u8 },
}

impl EncodedAudioCodec {
    pub const fn sample_rate_hz(self) -> u32 {
        match self {
            Self::PcmS16Be { sample_rate_hz, .. }
            | Self::AacLc { sample_rate_hz, .. }
            | Self::Opus { sample_rate_hz, .. } => sample_rate_hz,
        }
    }

    pub const fn channels(self) -> u8 {
        match self {
            Self::PcmS16Be { channels, .. }
            | Self::AacLc { channels, .. }
            | Self::Opus { channels, .. } => channels,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaClockOrigin {
    pub first_sample: u32,
    pub origin_monotonic_ns: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub struct EncryptedAudioFrame {
    pub stream_id: AudioStreamId,
    pub format_id: AudioFormatId,
    pub codec: EncodedAudioCodec,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub nonce_counter: u64,
    pub payload: Vec<u8>,
    pub rtp_packet: Vec<u8>,
}

impl std::fmt::Debug for EncryptedAudioFrame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EncryptedAudioFrame")
            .field("stream_id", &self.stream_id)
            .field("format_id", &self.format_id)
            .field("codec", &self.codec)
            .field("sequence", &self.sequence)
            .field("timestamp", &self.timestamp)
            .field("ssrc", &self.ssrc)
            .field("nonce_counter", &self.nonce_counter)
            .field(
                "payload",
                &format_args!("<redacted:{} bytes>", self.payload.len()),
            )
            .field(
                "rtp_packet",
                &format_args!("<redacted:{} bytes>", self.rtp_packet.len()),
            )
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioWireEvent {
    Started(MediaClockOrigin),
    Frame(EncryptedAudioFrame),
    Teardown { was_active: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioWireState {
    Active,
    Stopped,
    Failed,
}

impl AudioWireState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioWireLimits {
    pub maximum_packet_bytes: usize,
    pub maximum_payload_bytes: usize,
}

impl Default for AudioWireLimits {
    fn default() -> Self {
        Self {
            maximum_packet_bytes: 64 * 1024,
            maximum_payload_bytes: 64 * 1024 - RTP_HEADER_LENGTH - AIRPLAY_AUDIO_TAIL_LENGTH,
        }
    }
}

pub struct AudioWireDecoder {
    key: Zeroizing<[u8; 32]>,
    stream_id: AudioStreamId,
    format_id: AudioFormatId,
    codec: EncodedAudioCodec,
    limits: AudioWireLimits,
    expected_nonce: u64,
    wire_payload_type: Option<u8>,
    last_sequence: Option<u16>,
    last_timestamp: Option<u32>,
    ssrc: Option<u32>,
    origin: Option<MediaClockOrigin>,
    pending_flush: Option<AudioFlushEpoch>,
    state: AudioWireState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioFlushEpoch {
    sequence: Option<u16>,
    timestamp: Option<u32>,
}

impl AudioFlushEpoch {
    fn accepts(self, sequence: u16, timestamp: u32) -> bool {
        self.sequence
            .is_none_or(|target| sequence.wrapping_sub(target) < 0x8000)
            && self
                .timestamp
                .is_none_or(|target| timestamp.wrapping_sub(target) < 0x8000_0000)
    }
}

impl std::fmt::Debug for AudioWireDecoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AudioWireDecoder")
            .field("key", &"<redacted>")
            .field("stream_id", &self.stream_id)
            .field("format_id", &self.format_id)
            .field("codec", &self.codec)
            .field("limits", &self.limits)
            .field("expected_nonce", &self.expected_nonce)
            .field("wire_payload_type", &self.wire_payload_type)
            .field("last_sequence", &self.last_sequence)
            .field("last_timestamp", &self.last_timestamp)
            .field("ssrc", &self.ssrc)
            .field("origin", &self.origin)
            .field("pending_flush", &self.pending_flush)
            .field("state", &self.state)
            .finish()
    }
}

impl AudioWireDecoder {
    pub fn new(
        key: [u8; 32],
        stream_id: AudioStreamId,
        format_id: AudioFormatId,
        limits: AudioWireLimits,
    ) -> Result<Self> {
        let minimum = RTP_HEADER_LENGTH + AIRPLAY_AUDIO_TAIL_LENGTH + 1;
        if limits.maximum_packet_bytes < minimum
            || limits.maximum_payload_bytes == 0
            || limits.maximum_payload_bytes
                > limits.maximum_packet_bytes - RTP_HEADER_LENGTH - AIRPLAY_AUDIO_TAIL_LENGTH
        {
            return Err(Error::InvalidAudio(
                "invalid encrypted audio limits".to_owned(),
            ));
        }
        let codec = format_id.codec()?;
        Ok(Self {
            key: Zeroizing::new(key),
            stream_id,
            format_id,
            codec,
            limits,
            expected_nonce: 0,
            wire_payload_type: None,
            last_sequence: None,
            last_timestamp: None,
            ssrc: None,
            origin: None,
            pending_flush: None,
            state: AudioWireState::Active,
        })
    }

    pub fn state(&self) -> AudioWireState {
        self.state
    }

    pub fn origin(&self) -> Option<MediaClockOrigin> {
        self.origin
    }

    pub fn last_timestamp(&self) -> Option<u32> {
        self.last_timestamp
    }

    pub fn flush(&mut self, next_sequence: Option<u16>, next_timestamp: Option<u32>) -> Result<()> {
        if self.state != AudioWireState::Active {
            return Err(Error::NotReady {
                component: "audio wire decoder",
                state: self.state.as_str(),
            });
        }
        self.last_sequence = None;
        self.last_timestamp = None;
        self.wire_payload_type = None;
        self.ssrc = None;
        self.origin = None;
        self.pending_flush = Some(AudioFlushEpoch {
            sequence: next_sequence,
            timestamp: next_timestamp,
        });
        Ok(())
    }

    pub fn decode_at(&mut self, packet: &[u8], monotonic_ns: u64) -> Result<Vec<AudioWireEvent>> {
        if self.state != AudioWireState::Active {
            return Err(Error::NotReady {
                component: "audio wire decoder",
                state: self.state.as_str(),
            });
        }
        let result = self.decode_packet(packet, monotonic_ns);
        if result.is_err() {
            self.state = AudioWireState::Failed;
            self.key.zeroize();
        }
        result
    }

    pub fn teardown(&mut self) -> Option<AudioWireEvent> {
        if self.state == AudioWireState::Stopped {
            return None;
        }
        let was_active = self.origin.is_some();
        self.key.zeroize();
        self.state = AudioWireState::Stopped;
        Some(AudioWireEvent::Teardown { was_active })
    }

    fn decode_packet(&mut self, packet: &[u8], monotonic_ns: u64) -> Result<Vec<AudioWireEvent>> {
        let minimum = RTP_HEADER_LENGTH + AIRPLAY_AUDIO_TAIL_LENGTH + 1;
        if packet.len() < minimum || packet.len() > self.limits.maximum_packet_bytes {
            return Err(Error::InvalidAudio(format!(
                "encrypted RTP packet is {} bytes; expected {minimum}..={} bytes",
                packet.len(),
                self.limits.maximum_packet_bytes
            )));
        }
        if packet[0] != 0x80 {
            return Err(Error::InvalidAudio(
                "only fixed 12-byte RTP v2 headers are accepted".to_owned(),
            ));
        }
        let payload_type = packet[1] & 0x7f;
        let sequence = u16::from_be_bytes([packet[2], packet[3]]);
        let timestamp = u32::from_be_bytes(
            packet[4..8]
                .try_into()
                .map_err(|_| Error::InvalidAudio("RTP timestamp is truncated".to_owned()))?,
        );
        let ssrc = u32::from_be_bytes(
            packet[8..12]
                .try_into()
                .map_err(|_| Error::InvalidAudio("RTP SSRC is truncated".to_owned()))?,
        );
        let nonce_offset = packet.len() - 8;
        let tag_offset = nonce_offset - wire_crypto::TAG_LENGTH;
        let nonce_counter = u64::from_le_bytes(
            packet[nonce_offset..]
                .try_into()
                .map_err(|_| Error::InvalidAudio("audio nonce is truncated".to_owned()))?,
        );
        if nonce_counter < self.expected_nonce {
            return Ok(Vec::new());
        }
        if nonce_counter == u64::MAX {
            return Err(Error::InvalidAudio("audio nonce is exhausted".to_owned()));
        }
        let ciphertext = &packet[RTP_HEADER_LENGTH..tag_offset];
        if ciphertext.is_empty() || ciphertext.len() > self.limits.maximum_payload_bytes {
            return Err(Error::InvalidAudio(
                "encrypted audio payload is empty or oversized".to_owned(),
            ));
        }
        let mut sealed = Vec::with_capacity(ciphertext.len() + wire_crypto::TAG_LENGTH);
        sealed.extend_from_slice(ciphertext);
        sealed.extend_from_slice(&packet[tag_offset..nonce_offset]);
        let payload = wire_crypto::open(
            &self.key,
            &wire_crypto::nonce(nonce_counter),
            &sealed,
            &packet[4..RTP_HEADER_LENGTH],
        )?;
        if payload.is_empty() || payload.len() > self.limits.maximum_payload_bytes {
            return Err(Error::InvalidAudio(
                "decrypted audio payload is empty or oversized".to_owned(),
            ));
        }
        if self
            .pending_flush
            .is_some_and(|epoch| !epoch.accepts(sequence, timestamp))
        {
            self.expected_nonce = nonce_counter + 1;
            return Ok(Vec::new());
        }
        if self
            .wire_payload_type
            .is_some_and(|expected| expected != payload_type)
        {
            return Err(Error::InvalidAudio(
                "RTP payload type changed during the stream".to_owned(),
            ));
        }
        let mut rtp_packet = Vec::with_capacity(RTP_HEADER_LENGTH + payload.len());
        rtp_packet.extend_from_slice(&packet[..RTP_HEADER_LENGTH]);
        rtp_packet[1] = (rtp_packet[1] & 0x80) | (self.stream_id as u8 & 0x7f);
        rtp_packet.extend_from_slice(&payload);

        if self.last_sequence.is_some_and(|last| {
            let delta = sequence.wrapping_sub(last);
            delta == 0 || delta >= 0x8000
        }) {
            return Err(Error::InvalidAudio(
                "RTP sequence is replayed or moved backwards".to_owned(),
            ));
        }
        if self.last_timestamp.is_some_and(|last| {
            let delta = timestamp.wrapping_sub(last);
            delta == 0 || delta >= 0x8000_0000
        }) {
            return Err(Error::InvalidAudio(
                "RTP timestamp is replayed or moved backwards".to_owned(),
            ));
        }
        if self.ssrc.is_some_and(|expected| expected != ssrc) {
            return Err(Error::InvalidAudio(
                "RTP SSRC changed during the stream".to_owned(),
            ));
        }

        self.expected_nonce = nonce_counter + 1;
        self.wire_payload_type = Some(payload_type);
        self.last_sequence = Some(sequence);
        self.last_timestamp = Some(timestamp);
        self.ssrc = Some(ssrc);
        self.pending_flush = None;
        let mut events = Vec::with_capacity(2);
        if self.origin.is_none() {
            let origin = MediaClockOrigin {
                first_sample: timestamp,
                origin_monotonic_ns: monotonic_ns,
            };
            self.origin = Some(origin);
            events.push(AudioWireEvent::Started(origin));
        }
        events.push(AudioWireEvent::Frame(EncryptedAudioFrame {
            stream_id: self.stream_id,
            format_id: self.format_id,
            codec: self.codec,
            sequence,
            timestamp,
            ssrc,
            nonce_counter,
            payload,
            rtp_packet,
        }));
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [0x22; 32];

    fn packet(sequence: u16, timestamp: u32, nonce: u64, payload: &[u8]) -> Vec<u8> {
        packet_with_ssrc(sequence, timestamp, nonce, 0xdead_beef, payload)
    }

    fn packet_with_ssrc(
        sequence: u16,
        timestamp: u32,
        nonce: u64,
        ssrc: u32,
        payload: &[u8],
    ) -> Vec<u8> {
        packet_with_payload_type(
            sequence,
            timestamp,
            nonce,
            ssrc,
            AudioStreamId::Main as u8,
            payload,
        )
    }

    fn packet_with_payload_type(
        sequence: u16,
        timestamp: u32,
        nonce: u64,
        ssrc: u32,
        payload_type: u8,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut header = [0; RTP_HEADER_LENGTH];
        header[0] = 0x80;
        header[1] = payload_type & 0x7f;
        header[2..4].copy_from_slice(&sequence.to_be_bytes());
        header[4..8].copy_from_slice(&timestamp.to_be_bytes());
        header[8..12].copy_from_slice(&ssrc.to_be_bytes());
        let sealed = wire_crypto::seal(&KEY, &wire_crypto::nonce(nonce), payload, &header[4..])
            .expect("seal");
        let mut packet = Vec::new();
        packet.extend_from_slice(&header);
        packet.extend_from_slice(&sealed);
        packet.extend_from_slice(&nonce.to_le_bytes());
        packet
    }

    #[test]
    fn phone_owned_payload_type_is_latched_and_normalized_for_gstreamer() {
        let mut decoder = new_decoder();
        let wire = packet_with_payload_type(7, 4_711, 0, 0xdead_beef, 96, b"opus-frame");
        let events = decoder.decode_at(&wire, 123_456).expect("packet");
        let AudioWireEvent::Frame(frame) = events.last().expect("frame event") else {
            panic!("frame")
        };
        assert_eq!(wire[1] & 0x7f, 96);
        assert_eq!(frame.rtp_packet[1] & 0x7f, AudioStreamId::Main as u8);
        assert_eq!(&frame.rtp_packet[12..], b"opus-frame");

        let changed = packet_with_payload_type(8, 4_712, 1, 0xdead_beef, 97, b"changed");
        assert!(matches!(
            decoder.decode_at(&changed, 123_457),
            Err(Error::InvalidAudio(_))
        ));
        assert_eq!(decoder.state(), AudioWireState::Failed);
    }

    #[test]
    fn normalized_phone_payload_type_reaches_opus_and_aac_pipeline_caps() {
        for format in [0x4000_0000, 0x80_0000] {
            let mut decoder = AudioWireDecoder::new(
                KEY,
                AudioStreamId::Main,
                AudioFormatId::new(format),
                AudioWireLimits::default(),
            )
            .expect("decoder");
            let wire = packet_with_payload_type(7, 4_711, 0, 0xdead_beef, 96, b"encoded");
            let events = decoder.decode_at(&wire, 123_456).expect("packet");
            let AudioWireEvent::Frame(frame) = events.last().expect("frame event") else {
                panic!("frame")
            };
            let plan = crate::AudioStreamPlan::new(AudioStreamId::Main, AudioFormatId::new(format))
                .expect("pipeline plan");
            let adapted = plan.adapt_frame(frame).expect("adapted packet");
            assert_eq!(adapted[1] & 0x7f, AudioStreamId::Main as u8);
            if format == 0x80_0000 {
                assert_ne!(adapted[1] & 0x80, 0);
                assert_eq!(&adapted[12..16], &[0, 16, 0, 56]);
            }
        }
    }

    fn new_decoder() -> AudioWireDecoder {
        AudioWireDecoder::new(
            KEY,
            AudioStreamId::Main,
            AudioFormatId::new(0x4000_0000),
            AudioWireLimits::default(),
        )
        .expect("decoder")
    }

    #[test]
    fn livi_packet_layout_decrypts_and_pins_media_clock_origin() {
        let mut decoder = new_decoder();
        let wire = packet(7, 4_711, 0, b"opus-frame");
        assert_eq!(
            hex::encode(&wire),
            "8064000700001267deadbeef3fbb8e03ab64fa6d8a86ad99b17b2295d98e4de5af28354f563f0000000000000000"
        );
        let events = decoder.decode_at(&wire, 123_456).expect("packet");
        assert_eq!(
            events[0],
            AudioWireEvent::Started(MediaClockOrigin {
                first_sample: 4_711,
                origin_monotonic_ns: 123_456
            })
        );
        let AudioWireEvent::Frame(frame) = &events[1] else {
            panic!("frame")
        };
        assert_eq!(frame.payload, b"opus-frame");
        assert_eq!(&frame.rtp_packet[..12], &wire[..12]);
        assert_eq!(frame.nonce_counter, 0);
        assert_eq!(
            decoder.origin(),
            Some(MediaClockOrigin {
                first_sample: 4_711,
                origin_monotonic_ns: 123_456
            })
        );
    }

    #[test]
    fn flush_resets_rtp_order_and_origin_without_reusing_a_nonce() {
        let mut decoder = new_decoder();
        decoder
            .decode_at(&packet(100, 48_000, 0, b"before"), 1)
            .expect("initial packet");
        decoder.flush(Some(7), Some(900)).expect("flush");
        let events = decoder
            .decode_at(&packet(7, 900, 1, b"after"), 2)
            .expect("post-flush packet");
        assert_eq!(
            events.first(),
            Some(&AudioWireEvent::Started(MediaClockOrigin {
                first_sample: 900,
                origin_monotonic_ns: 2,
            }))
        );
        assert!(decoder.decode_at(&packet(8, 901, 0, b"replay"), 3).is_ok());
        assert_eq!(decoder.last_timestamp(), Some(900));
    }

    #[test]
    fn flush_drops_in_flight_old_epoch_packets_and_relatches_stream_identity() {
        let mut decoder = new_decoder();
        decoder
            .decode_at(
                &packet_with_payload_type(100, 1_000, 0, 0xdead_beef, 96, b"before"),
                1,
            )
            .expect("initial packet");
        decoder.flush(Some(103), Some(1_300)).expect("flush");

        let stale = decoder
            .decode_at(
                &packet_with_payload_type(101, 1_100, 1, 0xdead_beef, 96, b"stale"),
                2,
            )
            .expect("stale packet is nonfatal");
        assert!(stale.is_empty());
        assert_eq!(decoder.state(), AudioWireState::Active);
        assert_eq!(decoder.origin(), None);
        assert_eq!(decoder.last_timestamp(), None);

        let events = decoder
            .decode_at(&packet_with_payload_type(103, 1_300, 2, 7, 97, b"after"), 3)
            .expect("new epoch packet");
        assert_eq!(
            events.first(),
            Some(&AudioWireEvent::Started(MediaClockOrigin {
                first_sample: 1_300,
                origin_monotonic_ns: 3,
            }))
        );
        assert_eq!(decoder.state(), AudioWireState::Active);

        let changed_again = packet_with_payload_type(104, 1_301, 3, 8, 98, b"changed again");
        assert!(matches!(
            decoder.decode_at(&changed_again, 4),
            Err(Error::InvalidAudio(_))
        ));
    }

    #[test]
    fn newest_rapid_flush_epoch_replaces_the_previous_fence() {
        let mut decoder = new_decoder();
        decoder
            .decode_at(&packet(100, 1_000, 0, b"before"), 1)
            .expect("initial packet");
        decoder.flush(Some(103), Some(1_300)).expect("first flush");
        assert!(
            decoder
                .decode_at(&packet(101, 1_100, 1, b"stale"), 2)
                .expect("stale packet")
                .is_empty()
        );
        decoder.flush(Some(205), Some(2_500)).expect("second flush");
        assert!(
            decoder
                .decode_at(&packet(104, 1_400, 2, b"superseded epoch"), 3)
                .expect("superseded packet")
                .is_empty()
        );
        let events = decoder
            .decode_at(&packet(205, 2_500, 3, b"latest"), 4)
            .expect("latest epoch packet");
        assert!(matches!(events.first(), Some(AudioWireEvent::Started(_))));
    }

    #[test]
    fn flush_fence_accepts_target_loss_and_counter_wrap() {
        let mut decoder = new_decoder();
        decoder
            .decode_at(&packet(100, 1_000, 0, b"before"), 1)
            .expect("initial packet");
        decoder.flush(Some(103), Some(1_300)).expect("flush");
        let events = decoder
            .decode_at(&packet(104, 1_400, 1, b"target was lost"), 2)
            .expect("packet after lost target");
        assert!(matches!(events.first(), Some(AudioWireEvent::Started(_))));

        decoder
            .flush(Some(u16::MAX), Some(u32::MAX - 10))
            .expect("wrapping flush");
        assert!(
            decoder
                .decode_at(&packet(u16::MAX - 1, u32::MAX - 20, 2, b"pre-target"), 3,)
                .expect("pre-target packet")
                .is_empty()
        );
        let events = decoder
            .decode_at(&packet(0, 5, 3, b"wrapped"), 4)
            .expect("wrapped packet");
        assert!(matches!(events.first(), Some(AudioWireEvent::Started(_))));
    }

    #[test]
    fn flush_fence_requires_every_advertised_coordinate_and_authentication() {
        let mut decoder = new_decoder();
        decoder
            .decode_at(&packet(100, 1_000, 0, b"before"), 1)
            .expect("initial packet");
        decoder.flush(Some(200), Some(2_000)).expect("flush");
        assert!(
            decoder
                .decode_at(&packet(201, 1_900, 1, b"timestamp before"), 2)
                .expect("timestamp-before packet")
                .is_empty()
        );
        assert!(
            decoder
                .decode_at(&packet(199, 2_100, 2, b"sequence before"), 3)
                .expect("sequence-before packet")
                .is_empty()
        );

        let mut tampered = packet(200, 2_000, 3, b"tampered");
        tampered[RTP_HEADER_LENGTH] ^= 1;
        assert!(matches!(
            decoder.decode_at(&tampered, 4),
            Err(Error::Crypto(_))
        ));
        assert_eq!(decoder.state(), AudioWireState::Failed);
    }

    #[test]
    fn sequence_and_timestamp_wrap_are_accepted_once() {
        let mut decoder = new_decoder();
        decoder
            .decode_at(&packet(u16::MAX, u32::MAX - 5, 0, b"a"), 1)
            .expect("first");
        let events = decoder
            .decode_at(&packet(0, 3, 1, b"b"), 2)
            .expect("wrapped");
        let AudioWireEvent::Frame(frame) = events.last().expect("event") else {
            panic!("frame")
        };
        assert_eq!(frame.sequence, 0);
        assert_eq!(frame.timestamp, 3);
    }

    #[test]
    fn replay_is_ignored_while_tamper_and_ssrc_change_fail_closed() {
        let mut decoder = new_decoder();
        let first = packet(1, 10, 0, b"a");
        decoder.decode_at(&first, 1).expect("first");
        assert!(
            decoder
                .decode_at(&first, 2)
                .expect("late replay")
                .is_empty()
        );
        assert_eq!(decoder.state(), AudioWireState::Active);

        let mut decoder = new_decoder();
        let mut tampered = packet(1, 10, 0, b"a");
        tampered[12] ^= 1;
        assert!(matches!(
            decoder.decode_at(&tampered, 1),
            Err(Error::Crypto(_))
        ));
        assert_eq!(decoder.state(), AudioWireState::Failed);

        let mut decoder = new_decoder();
        decoder
            .decode_at(&packet(1, 10, 0, b"a"), 1)
            .expect("first");
        let changed = packet_with_ssrc(2, 20, 1, 7, b"b");
        assert!(matches!(
            decoder.decode_at(&changed, 2),
            Err(Error::InvalidAudio(_))
        ));
    }

    #[test]
    fn udp_loss_resynchronizes_and_late_packets_are_dropped() {
        let mut decoder = new_decoder();
        decoder
            .decode_at(&packet(1, 10, 0, b"a"), 1)
            .expect("first");
        let recovered = decoder
            .decode_at(&packet(4, 40, 3, b"d"), 4)
            .expect("forward gap");
        let AudioWireEvent::Frame(frame) = recovered.last().expect("recovered frame") else {
            panic!("frame")
        };
        assert_eq!(frame.sequence, 4);
        assert_eq!(frame.nonce_counter, 3);
        assert!(
            decoder
                .decode_at(&packet(2, 20, 1, b"b"), 5)
                .expect("late packet")
                .is_empty()
        );
        assert_eq!(decoder.state(), AudioWireState::Active);
    }

    #[test]
    fn malformed_oversize_and_teardown_are_terminal() {
        let mut decoder = new_decoder();
        assert!(matches!(
            decoder.decode_at(&[0; 12], 1),
            Err(Error::InvalidAudio(_))
        ));
        assert_eq!(decoder.state(), AudioWireState::Failed);

        let mut decoder = new_decoder();
        assert_eq!(
            decoder.teardown(),
            Some(AudioWireEvent::Teardown { was_active: false })
        );
        assert_eq!(decoder.teardown(), None);
        assert!(matches!(
            decoder.decode_at(&packet(1, 1, 0, b"x"), 1),
            Err(Error::NotReady { .. })
        ));
    }
}
