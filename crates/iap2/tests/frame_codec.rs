use bytes::Bytes;
use prelude_iap2::frame::{FrameError, LINK_HEADER_LEN};
use prelude_iap2::{ControlBits, LinkPacket, LinkPacketDecoder, LinkSynchronization, SessionType};
use proptest::prelude::*;

#[test]
fn wired_profile_is_control_session_version_two() {
    let profile = LinkSynchronization::wired_carplay();
    profile.validate().expect("valid profile");
    assert!(
        profile.sessions.iter().any(|session| {
            session.session_type == SessionType::Control && session.version == 2
        })
    );
    assert_eq!(
        LinkSynchronization::decode(&profile.encode().expect("encode")).expect("decode"),
        profile
    );
}

#[test]
fn incremental_decoder_preserves_partial_and_multiple_packets() {
    let first = LinkPacket::with_payload(ControlBits::ACK, 1, 2, 1, Bytes::from_static(b"first"));
    let second = LinkPacket::header_only(ControlBits::ACK, 2, 3);
    let mut bytes = first.encode().expect("first encode").to_vec();
    bytes.extend_from_slice(&second.encode().expect("second encode"));

    let mut decoder = LinkPacketDecoder::default();
    decoder.push(&bytes[..5]);
    assert_eq!(decoder.next_packet().expect("partial"), None);
    decoder.push(&bytes[5..]);
    assert_eq!(decoder.next_packet().expect("first"), Some(first));
    assert_eq!(decoder.next_packet().expect("second"), Some(second));
    assert_eq!(decoder.next_packet().expect("empty"), None);
}

#[test]
fn checksum_corruption_is_rejected() {
    let packet =
        LinkPacket::with_payload(ControlBits::ACK, 1, 2, 1, Bytes::from_static(b"payload"));
    let mut bytes = packet.encode().expect("encode").to_vec();
    bytes[LINK_HEADER_LEN] ^= 1;
    assert_eq!(
        LinkPacket::decode_prefix(&bytes),
        Err(FrameError::BadPayloadChecksum)
    );
}

#[test]
fn unknown_control_bits_are_rejected() {
    let packet = LinkPacket::header_only(ControlBits::ACK, 1, 2);
    let mut bytes = packet.encode().expect("encode").to_vec();
    bytes[4] |= 1;
    let sum = bytes[..8]
        .iter()
        .fold(0u8, |accumulator, byte| accumulator.wrapping_add(*byte));
    bytes[8] = 0u8.wrapping_sub(sum);
    assert_eq!(
        LinkPacket::decode_prefix(&bytes),
        Err(FrameError::UnknownControlBits(1))
    );
}

proptest! {
    #[test]
    fn arbitrary_packets_round_trip(
        payload in proptest::collection::vec(any::<u8>(), 0..2048),
        sequence in any::<u8>(),
        acknowledgement in any::<u8>(),
        session_id in any::<u8>(),
        control_nibble in 0u8..16,
    ) {
        let control = ControlBits::from_bits_retain(control_nibble << 4);
        let packet = if payload.is_empty() {
            LinkPacket::header_only(control, sequence, acknowledgement)
        } else {
            LinkPacket::with_payload(
                control,
                sequence,
                acknowledgement,
                session_id,
                Bytes::from(payload),
            )
        };
        let encoded = packet.encode().expect("encode");
        let (decoded, consumed) = LinkPacket::decode_prefix(&encoded)
            .expect("decode")
            .expect("complete");
        prop_assert_eq!(consumed, encoded.len());
        prop_assert_eq!(decoded, packet);
    }
}
