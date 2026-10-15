use std::time::Duration;

use prelude_mfi::{
    CERTIFICATE_MAX_LEN, CERTIFICATE_MIN_LEN, CHALLENGE_LEN, Error, MfiAuth, MockOperation,
    MockTransport, RESPONSE_LEN,
};
use proptest::prelude::*;

#[test]
fn constructor_enforces_authentication_protocol_major_three() {
    let transport = MockTransport::default();
    transport.set_protocol_major(2);
    let error = MfiAuth::new(transport).expect_err("major version must fail");
    assert!(matches!(
        error,
        Error::UnsupportedProtocolMajor {
            expected: 3,
            actual: 2
        }
    ));
}

#[test]
fn reads_certificate_with_both_required_settle_intervals() {
    let transport = MockTransport::default();
    let state = transport.state();
    let mut auth = MfiAuth::new(transport).expect("valid Auth 3.0 mock");
    let certificate = auth.certificate().expect("certificate");
    assert_eq!(certificate.len(), CERTIFICATE_MIN_LEN);

    let state = state.lock().expect("state");
    let settle_count = state
        .operations
        .iter()
        .filter(|operation| **operation == MockOperation::Sleep(Duration::from_millis(10)))
        .count();
    assert_eq!(settle_count, 2);
}

#[test]
fn rejects_out_of_range_certificate_length() {
    let transport = MockTransport::default();
    transport.set_register(0x30, 606u16.to_be_bytes().to_vec());
    let mut auth = MfiAuth::new(transport).expect("valid Auth 3.0 mock");
    assert!(matches!(
        auth.certificate(),
        Err(Error::InvalidCertificateLength { actual: 606, .. })
    ));
}

#[test]
fn converts_ascii_certificate_serial_to_binary_for_aa06() {
    let transport = MockTransport::default();
    let mut auth = MfiAuth::new(transport).expect("valid Auth 3.0 mock");
    assert_eq!(
        auth.certificate_serial().expect("serial"),
        [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]
    );
}

#[test]
fn rejects_non_hex_certificate_serial() {
    let transport = MockTransport::default();
    transport.set_certificate_serial_ascii(*b"00112233445566778899AABBCCDDEEFZ");
    let mut auth = MfiAuth::new(transport).expect("valid Auth 3.0 mock");
    assert!(matches!(
        auth.certificate_serial(),
        Err(Error::InvalidCertificateSerialHex {
            index: 31,
            byte: b'Z'
        })
    ));
}

#[test]
fn sign_checks_challenge_status_and_response_length() {
    let transport = MockTransport::default();
    let state = transport.state();
    let mut auth = MfiAuth::new(transport).expect("valid Auth 3.0 mock");
    let challenge = [0x42; CHALLENGE_LEN];
    let response = auth.sign(&challenge).expect("signature");
    assert_eq!(response, [0x5a; RESPONSE_LEN]);

    let operations = &state.lock().expect("state").operations;
    assert!(operations.contains(&MockOperation::Write(0x21, challenge.to_vec())));
    assert!(operations.contains(&MockOperation::Write(0x10, vec![1])));
    assert!(operations.contains(&MockOperation::Sleep(Duration::from_millis(500))));
    assert!(operations.contains(&MockOperation::Read(0x11, 2)));
    assert!(operations.contains(&MockOperation::Read(0x12, RESPONSE_LEN)));
}

#[test]
fn sign_rejects_failed_status() {
    let transport = MockTransport::default();
    transport.set_sign_result(0, vec![0x5a; RESPONSE_LEN]);
    let mut auth = MfiAuth::new(transport).expect("valid Auth 3.0 mock");
    assert!(matches!(
        auth.sign(&[0; CHALLENGE_LEN]),
        Err(Error::AuthenticationNotReady { status: 0 })
    ));
}

#[test]
fn sign_rejects_wrong_response_length() {
    let transport = MockTransport::default();
    transport.set_sign_result(0x10, vec![0x5a; RESPONSE_LEN - 1]);
    let mut auth = MfiAuth::new(transport).expect("valid Auth 3.0 mock");
    assert!(matches!(
        auth.sign(&[0; CHALLENGE_LEN]),
        Err(Error::InvalidResponseLength { actual: 63, .. })
    ));
}

proptest! {
    #[test]
    fn every_permitted_certificate_length_round_trips(
        length in CERTIFICATE_MIN_LEN..=CERTIFICATE_MAX_LEN,
        fill in any::<u8>(),
    ) {
        let transport = MockTransport::default();
        transport.set_certificate(vec![fill; length]);
        let mut auth = MfiAuth::new(transport).expect("valid Auth 3.0 mock");
        prop_assert_eq!(auth.certificate().expect("certificate"), vec![fill; length]);
    }
}
