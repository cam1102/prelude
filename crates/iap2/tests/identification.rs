use prelude_iap2::csm::{
    auth::{
        ACCESSORY_AUTHENTICATION_SERIAL_NUMBER, AUTHENTICATION_CERTIFICATE,
        REQUEST_AUTHENTICATION_CERTIFICATE,
    },
    carplay::{CARPLAY_START_SESSION, CarPlayStartSession, WiredCarPlayStart},
    decode_parameter_group,
    identification::{
        IDENTIFICATION_ACCEPTED, IDENTIFICATION_INFORMATION, IDENTIFICATION_REJECTED,
        IdentificationError, IdentificationInformation, START_IDENTIFICATION,
    },
};

#[test]
fn wired_identification_declares_control_v2_auth_and_carplay() {
    let identification = IdentificationInformation::wired_carplay(
        "Prelude",
        "Prelude",
        "Vanta Labs",
        "PR-0001",
        "0.1.0",
        "rev-a",
        3,
    );
    let frame = identification.to_frame().expect("identification");
    assert_eq!(frame.message_id, IDENTIFICATION_INFORMATION);

    let sent = frame
        .parameter(6)
        .expect("messages sent")
        .payload
        .chunks_exact(2)
        .map(|word| u16::from_be_bytes([word[0], word[1]]))
        .collect::<Vec<_>>();
    let received = frame
        .parameter(7)
        .expect("messages received")
        .payload
        .chunks_exact(2)
        .map(|word| u16::from_be_bytes([word[0], word[1]]))
        .collect::<Vec<_>>();
    assert!(sent.contains(&AUTHENTICATION_CERTIFICATE));
    assert!(sent.contains(&ACCESSORY_AUTHENTICATION_SERIAL_NUMBER));
    assert!(sent.contains(&CARPLAY_START_SESSION));
    assert!(received.contains(&REQUEST_AUTHENTICATION_CERTIFICATE));
    assert!(!sent.contains(&IDENTIFICATION_INFORMATION));
    assert!(!received.contains(&START_IDENTIFICATION));
    assert!(!received.contains(&IDENTIFICATION_ACCEPTED));
    assert!(!received.contains(&IDENTIFICATION_REJECTED));

    let usb = decode_parameter_group(&frame.parameter(16).expect("USB host").payload)
        .expect("USB host group");
    assert_eq!(
        usb.iter()
            .find(|parameter| parameter.id == 0)
            .expect("transport identifier")
            .payload[..],
        [0, 0]
    );
    assert_eq!(
        usb.iter()
            .find(|parameter| parameter.id == 1)
            .expect("transport name")
            .payload
            .as_ref(),
        b"USBHostTransport\0".as_slice()
    );
    assert!(
        usb.iter()
            .any(|parameter| parameter.id == 2 && parameter.payload.is_empty())
    );
    assert_eq!(
        usb.iter()
            .find(|parameter| parameter.id == 3)
            .expect("interface")
            .payload[..],
        [3]
    );
    assert!(
        usb.iter()
            .any(|parameter| parameter.id == 4 && parameter.payload.is_empty())
    );
    assert_eq!(
        frame.parameter(8).expect("power capability").payload[..],
        [0]
    );
}

#[test]
fn identification_fails_without_an_active_iap2_transport() {
    let mut identification = IdentificationInformation::wired_carplay(
        "Prelude",
        "Prelude",
        "Vanta Labs",
        "PR-0001",
        "0.1.0",
        "rev-a",
        3,
    );
    identification
        .usb_host_transport
        .as_mut()
        .expect("USB host")
        .supports_iap2_connection = false;
    assert_eq!(
        identification.validate(),
        Err(IdentificationError::NoIap2Transport)
    );
}

#[test]
fn wired_start_session_encodes_each_link_local_address() {
    let frame = CarPlayStartSession {
        wired: Some(WiredCarPlayStart {
            ip_addresses: vec!["169.254.10.1".into(), "fe80::1".into()],
        }),
        wireless: None,
        port: Some(7000),
        device_identifier: Some("device-id".into()),
        public_key: None,
        source_version: Some("1.0".into()),
    }
    .to_frame()
    .expect("start session");
    assert_eq!(frame.message_id, CARPLAY_START_SESSION);
    let wired =
        decode_parameter_group(&frame.parameter(0).expect("wired").payload).expect("wired group");
    assert_eq!(
        wired.iter().filter(|parameter| parameter.id == 0).count(),
        2
    );
}
