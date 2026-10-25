use std::collections::VecDeque;

use crate::{Error, Result};

pub const TIMING_PACKET_LENGTH: usize = 32;
pub const TIMING_REQUEST_TYPE: u8 = 210;
pub const TIMING_RESPONSE_TYPE: u8 = 211;

const NTP_SCALE: i128 = 1_i128 << 32;
const NANOS_PER_SECOND: i128 = 1_000_000_000;
const STALE_REQUEST_HISTORY: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NtpTimestamp(u64);

impl NtpTimestamp {
    pub const ZERO: Self = Self(0);

    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn from_parts(seconds: u32, fractional: u32) -> Self {
        Self(((seconds as u64) << 32) | fractional as u64)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn seconds(self) -> u32 {
        (self.0 >> 32) as u32
    }

    pub const fn fractional(self) -> u32 {
        self.0 as u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingPacketKind {
    Request,
    Response,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingPacket {
    pub kind: TimingPacketKind,
    pub originate: NtpTimestamp,
    pub receive: NtpTimestamp,
    pub transmit: NtpTimestamp,
}

impl TimingPacket {
    pub fn parse(encoded: &[u8]) -> Result<Self> {
        if encoded.len() != TIMING_PACKET_LENGTH {
            return Err(Error::InvalidTiming(format!(
                "timing packet is {} bytes; expected {TIMING_PACKET_LENGTH}",
                encoded.len()
            )));
        }
        if encoded[0] != 0x80 || u16::from_be_bytes([encoded[2], encoded[3]]) != 7 {
            return Err(Error::InvalidTiming(
                "timing packet has invalid version or RTCP length".to_owned(),
            ));
        }
        let kind = match encoded[1] {
            TIMING_REQUEST_TYPE => TimingPacketKind::Request,
            TIMING_RESPONSE_TYPE => TimingPacketKind::Response,
            packet_type => {
                return Err(Error::InvalidTiming(format!(
                    "unsupported timing packet type {packet_type}"
                )));
            }
        };
        Ok(Self {
            kind,
            originate: read_ntp(encoded, 8)?,
            receive: read_ntp(encoded, 16)?,
            transmit: read_ntp(encoded, 24)?,
        })
    }

    pub fn encode(self) -> [u8; TIMING_PACKET_LENGTH] {
        let mut encoded = [0; TIMING_PACKET_LENGTH];
        encoded[0] = 0x80;
        encoded[1] = match self.kind {
            TimingPacketKind::Request => TIMING_REQUEST_TYPE,
            TimingPacketKind::Response => TIMING_RESPONSE_TYPE,
        };
        encoded[2..4].copy_from_slice(&7_u16.to_be_bytes());
        write_ntp(&mut encoded, 8, self.originate);
        write_ntp(&mut encoded, 16, self.receive);
        write_ntp(&mut encoded, 24, self.transmit);
        encoded
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingSample {
    pub originate: NtpTimestamp,
    pub peer_receive: NtpTimestamp,
    pub peer_transmit: NtpTimestamp,
    pub local_receive: NtpTimestamp,
    pub offset_ns: i128,
    pub round_trip_ns: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimingEvent {
    Response([u8; TIMING_PACKET_LENGTH]),
    Sample(TimingSample),
    Stale,
    Teardown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingState {
    Idle,
    Pending,
    Synchronized,
    Stopped,
    Failed,
}

impl TimingState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Pending => "pending",
            Self::Synchronized => "synchronized",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug)]
pub struct TimingCodec {
    state: TimingState,
    pending_transmit: Option<NtpTimestamp>,
    previous_transmits: VecDeque<NtpTimestamp>,
    synchronized: bool,
}

impl Default for TimingCodec {
    fn default() -> Self {
        Self {
            state: TimingState::Idle,
            pending_transmit: None,
            previous_transmits: VecDeque::with_capacity(STALE_REQUEST_HISTORY),
            synchronized: false,
        }
    }
}

impl TimingCodec {
    pub fn state(&self) -> TimingState {
        self.state
    }

    pub fn request(&mut self, transmit: NtpTimestamp) -> Result<[u8; TIMING_PACKET_LENGTH]> {
        if !matches!(self.state, TimingState::Idle | TimingState::Synchronized) {
            return Err(Error::NotReady {
                component: "timing codec",
                state: self.state.as_str(),
            });
        }
        let packet = TimingPacket {
            kind: TimingPacketKind::Request,
            originate: NtpTimestamp::ZERO,
            receive: NtpTimestamp::ZERO,
            transmit,
        }
        .encode();
        if self.previous_transmits.len() == STALE_REQUEST_HISTORY {
            self.previous_transmits.pop_front();
        }
        self.previous_transmits.push_back(transmit);
        self.pending_transmit = Some(transmit);
        self.state = TimingState::Pending;
        Ok(packet)
    }

    pub fn expire_pending(&mut self) -> Result<()> {
        if self.state != TimingState::Pending {
            return Err(Error::NotReady {
                component: "timing codec",
                state: self.state.as_str(),
            });
        }
        self.pending_transmit = None;
        self.state = if self.synchronized {
            TimingState::Synchronized
        } else {
            TimingState::Idle
        };
        Ok(())
    }

    pub fn receive(
        &mut self,
        encoded: &[u8],
        local_receive: NtpTimestamp,
        response_transmit: NtpTimestamp,
    ) -> Result<TimingEvent> {
        if matches!(self.state, TimingState::Stopped | TimingState::Failed) {
            return Err(Error::NotReady {
                component: "timing codec",
                state: self.state.as_str(),
            });
        }
        let result = self.receive_packet(encoded, local_receive, response_transmit);
        if result.is_err() {
            self.pending_transmit = None;
            self.state = TimingState::Failed;
        }
        result
    }

    pub fn teardown(&mut self) -> Option<TimingEvent> {
        if self.state == TimingState::Stopped {
            return None;
        }
        self.pending_transmit = None;
        self.state = TimingState::Stopped;
        Some(TimingEvent::Teardown)
    }

    fn receive_packet(
        &mut self,
        encoded: &[u8],
        local_receive: NtpTimestamp,
        response_transmit: NtpTimestamp,
    ) -> Result<TimingEvent> {
        let packet = TimingPacket::parse(encoded)?;
        match packet.kind {
            TimingPacketKind::Request => {
                if packet.originate != NtpTimestamp::ZERO
                    || packet.receive != NtpTimestamp::ZERO
                    || packet.transmit == NtpTimestamp::ZERO
                {
                    return Err(Error::InvalidTiming(
                        "timing request has invalid originate, receive, or transmit fields"
                            .to_owned(),
                    ));
                }
                Ok(TimingEvent::Response(
                    TimingPacket {
                        kind: TimingPacketKind::Response,
                        originate: packet.transmit,
                        receive: local_receive,
                        transmit: response_transmit,
                    }
                    .encode(),
                ))
            }
            TimingPacketKind::Response => {
                if self.pending_transmit != Some(packet.originate)
                    && self.previous_transmits.contains(&packet.originate)
                {
                    return Ok(TimingEvent::Stale);
                }
                let pending = self.pending_transmit.ok_or_else(|| {
                    Error::InvalidTiming("unsolicited or replayed timing response".to_owned())
                })?;
                if packet.originate != pending {
                    return Err(Error::InvalidTiming(
                        "timing response does not match pending T1".to_owned(),
                    ));
                }
                if packet.receive == NtpTimestamp::ZERO || packet.transmit == NtpTimestamp::ZERO {
                    return Err(Error::InvalidTiming(
                        "timing response has a zero peer timestamp".to_owned(),
                    ));
                }
                let offset_units = (ntp_delta(packet.receive, pending)
                    + ntp_delta(packet.transmit, local_receive))
                    / 2;
                let rtt_units =
                    ntp_delta(local_receive, pending) - ntp_delta(packet.transmit, packet.receive);
                if rtt_units < 0 {
                    return Err(Error::InvalidTiming(
                        "timing response has a negative round trip".to_owned(),
                    ));
                }
                let sample = TimingSample {
                    originate: pending,
                    peer_receive: packet.receive,
                    peer_transmit: packet.transmit,
                    local_receive,
                    offset_ns: fixed_to_ns(offset_units),
                    round_trip_ns: fixed_to_ns(rtt_units) as u128,
                };
                self.pending_transmit = None;
                self.synchronized = true;
                self.state = TimingState::Synchronized;
                Ok(TimingEvent::Sample(sample))
            }
        }
    }
}

fn read_ntp(encoded: &[u8], offset: usize) -> Result<NtpTimestamp> {
    let raw = u64::from_be_bytes(
        encoded
            .get(offset..offset + 8)
            .ok_or_else(|| Error::InvalidTiming("NTP timestamp is truncated".to_owned()))?
            .try_into()
            .map_err(|_| Error::InvalidTiming("NTP timestamp is truncated".to_owned()))?,
    );
    Ok(NtpTimestamp::new(raw))
}

fn write_ntp(encoded: &mut [u8], offset: usize, timestamp: NtpTimestamp) {
    encoded[offset..offset + 8].copy_from_slice(&timestamp.raw().to_be_bytes());
}

fn ntp_delta(later: NtpTimestamp, earlier: NtpTimestamp) -> i128 {
    i128::from(later.raw()) - i128::from(earlier.raw())
}

fn fixed_to_ns(units: i128) -> i128 {
    units * NANOS_PER_SECOND / NTP_SCALE
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: u64 = 1_u64 << 32;

    #[test]
    fn livi_request_and_response_wire_layout_is_exact() {
        let mut codec = TimingCodec::default();
        let request = codec
            .request(NtpTimestamp::new(0x1122_3344_5566_7788))
            .expect("request");
        assert_eq!(
            hex::encode(request),
            "80d2000700000000000000000000000000000000000000001122334455667788"
        );

        let phone_request = TimingPacket {
            kind: TimingPacketKind::Request,
            originate: NtpTimestamp::ZERO,
            receive: NtpTimestamp::ZERO,
            transmit: NtpTimestamp::new(0x0102_0304_0506_0708),
        }
        .encode();
        let event = codec
            .receive(
                &phone_request,
                NtpTimestamp::new(0x1111_1111_2222_2222),
                NtpTimestamp::new(0x3333_3333_4444_4444),
            )
            .expect("response");
        let TimingEvent::Response(response) = event else {
            panic!("response")
        };
        assert_eq!(
            hex::encode(response),
            "80d3000700000000010203040506070811111111222222223333333344444444"
        );
    }

    #[test]
    fn matching_response_yields_offset_and_round_trip_sample() {
        let mut codec = TimingCodec::default();
        let t1 = NtpTimestamp::new(100 * SECOND);
        codec.request(t1).expect("request");
        let response = TimingPacket {
            kind: TimingPacketKind::Response,
            originate: t1,
            receive: NtpTimestamp::new(102 * SECOND),
            transmit: NtpTimestamp::new(103 * SECOND),
        }
        .encode();
        let event = codec
            .receive(
                &response,
                NtpTimestamp::new(104 * SECOND),
                NtpTimestamp::new(104 * SECOND),
            )
            .expect("sample");
        let TimingEvent::Sample(sample) = event else {
            panic!("sample")
        };
        assert_eq!(sample.offset_ns, 500_000_000);
        assert_eq!(sample.round_trip_ns, 3_000_000_000);
        assert_eq!(codec.state(), TimingState::Synchronized);
        assert_eq!(
            codec
                .receive(
                    &response,
                    NtpTimestamp::new(104 * SECOND),
                    NtpTimestamp::new(104 * SECOND)
                )
                .expect("replayed response"),
            TimingEvent::Stale
        );
        assert_eq!(codec.state(), TimingState::Synchronized);
    }

    #[test]
    fn first_sample_preserves_a_phase_error_larger_than_signed_ntp_half_era() {
        let mut codec = TimingCodec::default();
        let local_t1 = NtpTimestamp::new(4_000_000_000 * SECOND);
        codec.request(local_t1).expect("request");
        let peer_t2 = NtpTimestamp::new(1_792_786_471 * SECOND);
        let peer_t3 = NtpTimestamp::new(1_792_786_472 * SECOND);
        let response = TimingPacket {
            kind: TimingPacketKind::Response,
            originate: local_t1,
            receive: peer_t2,
            transmit: peer_t3,
        }
        .encode();
        let event = codec
            .receive(
                &response,
                NtpTimestamp::new(4_000_000_002 * SECOND),
                NtpTimestamp::ZERO,
            )
            .expect("sample");
        let TimingEvent::Sample(sample) = event else {
            panic!("sample")
        };
        assert_eq!(sample.offset_ns, -2_207_213_529_500_000_000);
        assert_eq!(sample.round_trip_ns, 1_000_000_000);
    }

    #[test]
    fn mismatch_replay_negative_rtt_and_malformed_packets_fail_closed() {
        let mut codec = TimingCodec::default();
        codec
            .request(NtpTimestamp::new(10 * SECOND))
            .expect("request");
        let mismatch = TimingPacket {
            kind: TimingPacketKind::Response,
            originate: NtpTimestamp::new(11 * SECOND),
            receive: NtpTimestamp::new(12 * SECOND),
            transmit: NtpTimestamp::new(13 * SECOND),
        }
        .encode();
        assert!(matches!(
            codec.receive(
                &mismatch,
                NtpTimestamp::new(14 * SECOND),
                NtpTimestamp::ZERO
            ),
            Err(Error::InvalidTiming(_))
        ));
        assert_eq!(codec.state(), TimingState::Failed);

        let mut codec = TimingCodec::default();
        assert!(matches!(
            codec.receive(&[0; 4], NtpTimestamp::ZERO, NtpTimestamp::ZERO),
            Err(Error::InvalidTiming(_))
        ));
        assert_eq!(codec.state(), TimingState::Failed);

        let mut codec = TimingCodec::default();
        let t1 = NtpTimestamp::new(10 * SECOND);
        codec.request(t1).expect("request");
        let negative = TimingPacket {
            kind: TimingPacketKind::Response,
            originate: t1,
            receive: NtpTimestamp::new(11 * SECOND),
            transmit: NtpTimestamp::new(20 * SECOND),
        }
        .encode();
        assert!(matches!(
            codec.receive(
                &negative,
                NtpTimestamp::new(12 * SECOND),
                NtpTimestamp::ZERO
            ),
            Err(Error::InvalidTiming(_))
        ));
    }

    #[test]
    fn late_and_duplicate_responses_to_sent_requests_are_stale() {
        let response = |originate: NtpTimestamp| {
            TimingPacket {
                kind: TimingPacketKind::Response,
                originate,
                receive: NtpTimestamp::new(originate.raw() + SECOND / 10),
                transmit: NtpTimestamp::new(originate.raw() + SECOND / 10),
            }
            .encode()
        };
        let mut codec = TimingCodec::default();
        let first = NtpTimestamp::new(10 * SECOND);
        let second = NtpTimestamp::new(11 * SECOND);
        codec.request(first).expect("first request");
        codec.expire_pending().expect("expire");
        codec.request(second).expect("second request");

        assert_eq!(
            codec
                .receive(
                    &response(first),
                    NtpTimestamp::new(11 * SECOND + SECOND / 2),
                    NtpTimestamp::ZERO
                )
                .expect("late response"),
            TimingEvent::Stale
        );
        assert_eq!(codec.state(), TimingState::Pending);

        let event = codec
            .receive(
                &response(second),
                NtpTimestamp::new(11 * SECOND + SECOND / 5),
                NtpTimestamp::ZERO,
            )
            .expect("current response");
        assert!(matches!(event, TimingEvent::Sample(_)));
        assert_eq!(
            codec
                .receive(
                    &response(second),
                    NtpTimestamp::new(11 * SECOND + SECOND / 4),
                    NtpTimestamp::ZERO
                )
                .expect("duplicate response"),
            TimingEvent::Stale
        );
        assert_eq!(codec.state(), TimingState::Synchronized);
    }

    #[test]
    fn pending_timeout_and_teardown_are_explicit() {
        let mut codec = TimingCodec::default();
        codec.request(NtpTimestamp::new(SECOND)).expect("request");
        assert!(matches!(
            codec.request(NtpTimestamp::new(2 * SECOND)),
            Err(Error::NotReady { .. })
        ));
        codec.expire_pending().expect("expire");
        assert_eq!(codec.state(), TimingState::Idle);
        assert_eq!(codec.teardown(), Some(TimingEvent::Teardown));
        assert_eq!(codec.teardown(), None);
    }
}
