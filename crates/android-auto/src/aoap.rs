use std::time::{Duration, Instant};

use crate::{
    APPLE_VENDOR_ID, AccessoryIdentity, AoapCandidate, AoapMode, AoapTimeouts, CancellationToken,
    Error, GET_PROTOCOL_REQUEST, Result, SEND_STRING_REQUEST, START_ACCESSORY_REQUEST,
    STRING_DESCRIPTION, STRING_MANUFACTURER, STRING_MODEL, STRING_SERIAL, STRING_URI,
    STRING_VERSION, UsbBackend, UsbDeviceSnapshot, UsbHandle, UsbTopology,
    is_probe_candidate_with_vendor_ids,
};

const IDENTITY_FIELDS: [(&str, u16); 6] = [
    ("manufacturer", STRING_MANUFACTURER),
    ("model", STRING_MODEL),
    ("description", STRING_DESCRIPTION),
    ("version", STRING_VERSION),
    ("uri", STRING_URI),
    ("serial", STRING_SERIAL),
];
const MAX_IDENTITY_FIELD_BYTES: usize = 255;

pub fn discover<B: UsbBackend>(
    backend: &mut B,
    control_timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<Vec<AoapCandidate>> {
    discover_with_vendor_ids(backend, control_timeout, cancellation, &[])
}

pub fn discover_with_vendor_ids<B: UsbBackend>(
    backend: &mut B,
    control_timeout: Duration,
    cancellation: &CancellationToken,
    additional_vendor_ids: &[u16],
) -> Result<Vec<AoapCandidate>> {
    let mut candidates = Vec::new();
    for device in backend.devices()? {
        check_cancelled(cancellation, "probing USB devices")?;

        if device.is_accessory_mode() {
            candidates.push(AoapCandidate {
                device,
                mode: AoapMode::Accessory,
            });
            continue;
        }
        if device.vendor_id == APPLE_VENDOR_ID {
            continue;
        }
        if !is_probe_candidate_with_vendor_ids(&device, additional_vendor_ids) {
            continue;
        }

        let Ok(mut handle) = backend.open(&device) else {
            continue;
        };
        if let Ok(protocol_version) = get_protocol(&mut handle, control_timeout) {
            if protocol_version >= 1 {
                candidates.push(AoapCandidate {
                    device,
                    mode: AoapMode::Normal { protocol_version },
                });
            }
        }
    }
    candidates.sort_by(|left, right| left.device.topology.cmp(&right.device.topology));
    Ok(candidates)
}

pub fn open_accessory<B: UsbBackend>(
    backend: &mut B,
    candidate: &AoapCandidate,
    identity: &AccessoryIdentity,
    timeouts: AoapTimeouts,
    cancellation: &CancellationToken,
) -> Result<crate::UsbAoapTransport<B::Handle>> {
    validate_candidate(candidate)?;
    validate_identity(identity)?;
    check_cancelled(cancellation, "opening Android accessory")?;

    let accessory = match candidate.mode {
        AoapMode::Accessory => candidate.device.clone(),
        AoapMode::Normal { .. } => {
            let mut handle = open_with_retry(
                backend,
                &candidate.device,
                timeouts.open_attempts,
                timeouts.retry_interval,
                cancellation,
                "opening Android device for AOAP handshake",
            )?;
            if candidate.device.active_configuration.is_none() {
                if let Some(configuration) = candidate.device.configurations.first() {
                    handle
                        .set_active_configuration(configuration.value)
                        .map_err(|source| {
                            Error::usb("selecting a configuration for AOAP handshake", source)
                        })?;
                }
            }
            run_handshake(&mut handle, identity, timeouts.control, cancellation)?;
            drop(handle);
            wait_for_accessory(backend, &candidate.device.topology, timeouts, cancellation)?
        }
    };

    let mut handle = open_with_retry(
        backend,
        &accessory,
        timeouts.open_attempts,
        timeouts.retry_interval,
        cancellation,
        "opening Android accessory",
    )?;
    let endpoints = match crate::select_bulk_endpoints(&accessory) {
        Ok(endpoints) => endpoints,
        Err(error) => {
            let _ = handle.reset();
            return Err(error);
        }
    };

    if accessory.active_configuration != Some(endpoints.configuration) {
        if let Err(source) = handle.set_active_configuration(endpoints.configuration) {
            let _ = handle.reset();
            return Err(Error::usb(
                "selecting Android accessory configuration",
                source,
            ));
        }
    }

    claim_with_retry(
        backend,
        &mut handle,
        endpoints.interface,
        timeouts.claim_attempts,
        timeouts.retry_interval,
        cancellation,
    )?;
    if endpoints.alternate_setting != 0 {
        if let Err(source) =
            handle.set_alternate_setting(endpoints.interface, endpoints.alternate_setting)
        {
            let _ = handle.release_interface(endpoints.interface);
            let _ = handle.reset();
            return Err(Error::usb(
                "selecting Android accessory alternate setting",
                source,
            ));
        }
    }

    Ok(crate::UsbAoapTransport::new(
        handle,
        accessory,
        endpoints,
        timeouts.transfer_poll_interval,
    ))
}

pub fn run_handshake<H: UsbHandle>(
    handle: &mut H,
    identity: &AccessoryIdentity,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<u16> {
    validate_identity(identity)?;
    check_cancelled(cancellation, "running AOAP handshake")?;
    let protocol = get_protocol(handle, timeout)?;
    if protocol < 1 {
        return Err(Error::UnsupportedProtocol(protocol));
    }

    let values = [
        identity.manufacturer.as_str(),
        identity.model.as_str(),
        identity.description.as_str(),
        identity.version.as_str(),
        identity.uri.as_str(),
        identity.serial.as_str(),
    ];
    for ((_, index), value) in IDENTITY_FIELDS.into_iter().zip(values) {
        check_cancelled(cancellation, "sending AOAP identity")?;
        let mut terminated = Vec::with_capacity(value.len() + 1);
        terminated.extend_from_slice(value.as_bytes());
        terminated.push(0);
        let written = handle
            .control_write(SEND_STRING_REQUEST, 0, index, &terminated, timeout)
            .map_err(|source| Error::usb("sending AOAP identity", source))?;
        if written != terminated.len() {
            return Err(Error::NoProgress {
                operation: "sending AOAP identity",
            });
        }
    }

    check_cancelled(cancellation, "starting AOAP accessory mode")?;
    let written = handle
        .control_write(START_ACCESSORY_REQUEST, 0, 0, &[], timeout)
        .map_err(|source| Error::usb("starting AOAP accessory mode", source))?;
    if written != 0 {
        return Err(Error::NoProgress {
            operation: "starting AOAP accessory mode",
        });
    }
    Ok(protocol)
}

fn get_protocol<H: UsbHandle>(handle: &mut H, timeout: Duration) -> Result<u16> {
    let mut response = [0_u8; 2];
    let length = handle
        .control_read(GET_PROTOCOL_REQUEST, 0, 0, &mut response, timeout)
        .map_err(|source| Error::usb("reading AOAP protocol version", source))?;
    if length != response.len() {
        return Err(Error::MalformedProtocolResponse { actual: length });
    }
    Ok(u16::from_le_bytes(response))
}

fn validate_candidate(candidate: &AoapCandidate) -> Result<()> {
    if candidate.device.topology.port_path.is_empty() {
        return Err(Error::MissingPortPath);
    }
    match candidate.mode {
        AoapMode::Accessory if candidate.device.is_accessory_mode() => Ok(()),
        AoapMode::Normal { protocol_version }
            if protocol_version >= 1
                && candidate.device.vendor_id != APPLE_VENDOR_ID
                && !candidate.device.is_accessory_mode() =>
        {
            Ok(())
        }
        AoapMode::Accessory | AoapMode::Normal { .. } => Err(Error::NotAoapCandidate),
    }
}

fn validate_identity(identity: &AccessoryIdentity) -> Result<()> {
    let values = [
        identity.manufacturer.as_str(),
        identity.model.as_str(),
        identity.description.as_str(),
        identity.version.as_str(),
        identity.uri.as_str(),
        identity.serial.as_str(),
    ];
    for ((field, _), value) in IDENTITY_FIELDS.into_iter().zip(values) {
        if value.is_empty()
            || value.as_bytes().contains(&0)
            || value.len() + 1 > MAX_IDENTITY_FIELD_BYTES
        {
            return Err(Error::InvalidIdentity { field });
        }
    }
    Ok(())
}

fn wait_for_accessory<B: UsbBackend>(
    backend: &mut B,
    topology: &UsbTopology,
    timeouts: AoapTimeouts,
    cancellation: &CancellationToken,
) -> Result<UsbDeviceSnapshot> {
    let started = Instant::now();
    loop {
        check_cancelled(cancellation, "waiting for Android accessory re-enumeration")?;
        if let Some(device) = backend
            .devices()?
            .into_iter()
            .find(|device| device.topology == *topology && device.is_accessory_mode())
        {
            return Ok(device);
        }
        let elapsed = started.elapsed();
        if elapsed >= timeouts.reenumeration {
            return Err(Error::ReenumerationTimeout {
                topology: topology.clone(),
                timeout: timeouts.reenumeration,
            });
        }
        backend.sleep(
            timeouts
                .poll_interval
                .min(timeouts.reenumeration.saturating_sub(elapsed)),
        );
    }
}

fn open_with_retry<B: UsbBackend>(
    backend: &mut B,
    device: &UsbDeviceSnapshot,
    attempts: usize,
    retry_interval: Duration,
    cancellation: &CancellationToken,
    operation: &'static str,
) -> Result<B::Handle> {
    let attempts = attempts.max(1);
    let mut last_error = rusb::Error::Other;
    for attempt in 0..attempts {
        check_cancelled(cancellation, operation)?;
        match backend.open(device) {
            Ok(handle) => return Ok(handle),
            Err(source) => last_error = source,
        }
        if attempt + 1 < attempts {
            backend.sleep(retry_interval);
        }
    }
    Err(Error::usb(operation, last_error))
}

fn claim_with_retry<B: UsbBackend>(
    backend: &mut B,
    handle: &mut B::Handle,
    interface: u8,
    attempts: usize,
    retry_interval: Duration,
    cancellation: &CancellationToken,
) -> Result<()> {
    let operation = "claiming Android accessory interface";
    let attempts = attempts.max(1);
    let mut last_error = rusb::Error::Other;
    for attempt in 0..attempts {
        if let Err(error) = check_cancelled(cancellation, operation) {
            let _ = handle.reset();
            return Err(error);
        }
        match handle.claim_interface(interface) {
            Ok(()) => return Ok(()),
            Err(source) => last_error = source,
        }
        if attempt + 1 < attempts {
            backend.sleep(retry_interval);
        }
    }
    let _ = handle.reset();
    Err(Error::usb(operation, last_error))
}

pub(crate) fn check_cancelled(
    cancellation: &CancellationToken,
    operation: &'static str,
) -> Result<()> {
    if cancellation.is_cancelled() {
        Err(Error::Cancelled { operation })
    } else {
        Ok(())
    }
}
