use bytes::Bytes;
use prelude_iap2::csm::{CsmError, CsmFrame, CsmParameter, auth::AuthenticationMessage};
use proptest::prelude::*;

#[test]
fn aa00_serial_request_is_presence_only() {
    let message = AuthenticationMessage::RequestCertificate {
        request_serial_number: true,
    };
    let frame = message.to_frame();
    assert_eq!(frame.parameters[0].payload.len(), 0);
    assert_eq!(
        AuthenticationMessage::from_frame(&frame).expect("decode"),
        message
    );
}

#[test]
fn auth_parser_rejects_unexpected_parameters() {
    let frame = CsmFrame {
        message_id: prelude_iap2::csm::auth::AUTHENTICATION_SUCCEEDED,
        parameters: vec![CsmParameter::presence(7)],
    };
    assert_eq!(
        AuthenticationMessage::from_frame(&frame),
        Err(CsmError::UnexpectedParameter(7))
    );
}

#[test]
fn decode_exact_rejects_trailing_data() {
    let frame = CsmFrame::empty(0x1234);
    let mut bytes = frame.encode().expect("encode").to_vec();
    bytes.push(0);
    assert_eq!(
        CsmFrame::decode_exact(&bytes),
        Err(CsmError::TrailingBytes(1))
    );
}

proptest! {
    #[test]
    fn arbitrary_csm_frames_round_trip(
        message_id in any::<u16>(),
        values in proptest::collection::vec(
            (any::<u16>(), proptest::collection::vec(any::<u8>(), 0..64)),
            0..16,
        ),
    ) {
        let frame = CsmFrame {
            message_id,
            parameters: values
                .into_iter()
                .map(|(id, payload)| CsmParameter::new(id, Bytes::from(payload)))
                .collect(),
        };
        let encoded = frame.encode().expect("encode");
        prop_assert_eq!(CsmFrame::decode_exact(&encoded).expect("decode"), frame);
    }
}
