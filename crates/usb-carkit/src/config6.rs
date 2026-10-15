use std::{thread, time::Duration};

use rusb::{DeviceHandle, GlobalContext};

use crate::{
    ConfigurationPolicy, ConfigurationRequest, DeviceDiscovery, Error, IphoneCandidate,
    LibUsbConfigurator, LibUsbDiscovery, Result, UsbConfigurationController,
    discovery::normalize_apple_usb_serial,
};

const CARPLAY_CONFIGURATION: u8 = 6;
const REVEAL_REQUEST_TYPE: u8 = 0xc0;
const REVEAL_REQUEST: u8 = 0x52;
const REVEAL_INDEX: u16 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Config6Policy {
    pub allow_private_configuration: bool,
    pub allow_configuration_change: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config6Timeouts {
    pub control_transfer: Duration,
    pub reconnect: Duration,
    pub poll_interval: Duration,
}

impl Default for Config6Timeouts {
    fn default() -> Self {
        Self {
            control_transfer: Duration::from_secs(3),
            reconnect: Duration::from_secs(5),
            poll_interval: Duration::from_millis(200),
        }
    }
}

pub trait Config6Backend: std::fmt::Debug + Send {
    fn reveal(&mut self, candidate: &IphoneCandidate, timeout: Duration) -> Result<()>;
    fn scan(&mut self) -> Result<Vec<IphoneCandidate>>;
    fn select(
        &mut self,
        candidate: &IphoneCandidate,
        configuration: u8,
        private_apple_configuration: bool,
        policy: Config6Policy,
    ) -> Result<()>;
    fn sleep(&mut self, duration: Duration);
}

#[derive(Debug, Default)]
pub struct LibUsbConfig6Backend {
    discovery: LibUsbDiscovery,
    configurator: LibUsbConfigurator,
}

impl Config6Backend for LibUsbConfig6Backend {
    fn reveal(&mut self, candidate: &IphoneCandidate, timeout: Duration) -> Result<()> {
        let handle = open_candidate(candidate)?;
        let mut response = [0];
        let received = handle.read_control(
            REVEAL_REQUEST_TYPE,
            REVEAL_REQUEST,
            0,
            REVEAL_INDEX,
            &mut response,
            timeout,
        )?;
        if received != response.len() {
            return Err(Error::InvalidDescriptor(format!(
                "CarPlay configuration reveal returned {received} bytes; expected {}",
                response.len()
            )));
        }
        Ok(())
    }

    fn scan(&mut self) -> Result<Vec<IphoneCandidate>> {
        self.discovery.scan()
    }

    fn select(
        &mut self,
        candidate: &IphoneCandidate,
        configuration: u8,
        private_apple_configuration: bool,
        policy: Config6Policy,
    ) -> Result<()> {
        self.configurator.configure(
            candidate,
            ConfigurationRequest {
                value: configuration,
                private_apple_configuration,
            },
            ConfigurationPolicy {
                allow_configuration_changes: policy.allow_configuration_change,
                allow_private_apple_configurations: policy.allow_private_configuration,
            },
        )?;
        Ok(())
    }

    fn sleep(&mut self, duration: Duration) {
        thread::sleep(duration);
    }
}

#[derive(Debug)]
pub struct Config6Activator<B = LibUsbConfig6Backend> {
    backend: B,
    timeouts: Config6Timeouts,
}

impl Default for Config6Activator<LibUsbConfig6Backend> {
    fn default() -> Self {
        Self::new(LibUsbConfig6Backend::default(), Config6Timeouts::default())
    }
}

impl<B: Config6Backend> Config6Activator<B> {
    pub fn new(backend: B, timeouts: Config6Timeouts) -> Self {
        Self { backend, timeouts }
    }

    pub fn activate(
        &mut self,
        candidate: &IphoneCandidate,
        policy: Config6Policy,
    ) -> Result<IphoneCandidate> {
        if !policy.allow_private_configuration || !policy.allow_configuration_change {
            return Err(Error::Unsupported(
                "configuration 6 requires explicit private-configuration and mutation policy",
            ));
        }
        let serial = candidate
            .identity
            .serial_number
            .as_deref()
            .filter(|serial| !serial.is_empty())
            .ok_or(Error::NotReady {
                component: "configuration 6",
                state: "device_serial_unavailable",
            })?;

        let mut current = candidate.clone();
        if !advertises_config6(&current) {
            self.backend
                .reveal(&current, self.timeouts.control_transfer)?;
            current = self.wait_for(serial, advertises_config6, "configuration 6 reveal")?;
        }
        if current.active_configuration != Some(CARPLAY_CONFIGURATION) {
            self.backend
                .select(&current, CARPLAY_CONFIGURATION, true, policy)?;
            current = self.wait_for(
                serial,
                |candidate| candidate.active_configuration == Some(CARPLAY_CONFIGURATION),
                "configuration 6 selection",
            )?;
        }
        Ok(current)
    }

    pub fn restore(
        &mut self,
        candidate: &IphoneCandidate,
        previous_configuration: Option<u8>,
        policy: Config6Policy,
    ) -> Result<IphoneCandidate> {
        let Some(previous_configuration) = previous_configuration else {
            return Err(Error::NotReady {
                component: "USB configuration restoration",
                state: "previous_configuration_unknown",
            });
        };
        if previous_configuration == CARPLAY_CONFIGURATION {
            return Ok(candidate.clone());
        }
        if !policy.allow_configuration_change {
            return Err(Error::Unsupported(
                "USB configuration restoration requires mutation policy",
            ));
        }
        let serial = candidate
            .identity
            .serial_number
            .as_deref()
            .filter(|serial| !serial.is_empty())
            .ok_or(Error::NotReady {
                component: "USB configuration restoration",
                state: "device_serial_unavailable",
            })?;
        self.backend
            .select(candidate, previous_configuration, false, policy)?;
        self.wait_for(
            serial,
            |candidate| candidate.active_configuration == Some(previous_configuration),
            "USB configuration restoration",
        )
    }

    fn wait_for(
        &mut self,
        serial: &str,
        predicate: impl Fn(&IphoneCandidate) -> bool,
        operation: &'static str,
    ) -> Result<IphoneCandidate> {
        if self.timeouts.poll_interval.is_zero() {
            return Err(Error::InvalidDescriptor(
                "configuration 6 poll interval is zero".to_owned(),
            ));
        }
        let attempts = self
            .timeouts
            .reconnect
            .as_nanos()
            .checked_div(self.timeouts.poll_interval.as_nanos())
            .unwrap_or(0)
            .saturating_add(1);
        for attempt in 0..attempts {
            if let Some(candidate) = self.backend.scan()?.into_iter().find(|candidate| {
                candidate.identity.serial_number.as_deref() == Some(serial) && predicate(candidate)
            }) {
                return Ok(candidate);
            }
            if attempt + 1 < attempts {
                self.backend.sleep(self.timeouts.poll_interval);
            }
        }
        Err(Error::Timeout {
            operation,
            timeout: self.timeouts.reconnect,
        })
    }
}

fn advertises_config6(candidate: &IphoneCandidate) -> bool {
    candidate
        .configurations
        .iter()
        .any(|configuration| configuration.value == CARPLAY_CONFIGURATION)
}

fn open_candidate(candidate: &IphoneCandidate) -> Result<DeviceHandle<GlobalContext>> {
    let devices = rusb::devices()?;
    let device = devices
        .iter()
        .find(|device| {
            device.bus_number() == candidate.identity.address.bus
                && device.address() == candidate.identity.address.address
        })
        .ok_or(Error::DeviceNotFound {
            bus: candidate.identity.address.bus,
            address: candidate.identity.address.address,
        })?;
    let descriptor = device.device_descriptor()?;
    if descriptor.vendor_id() != candidate.identity.vendor_id
        || descriptor.product_id() != candidate.identity.product_id
    {
        return Err(Error::IdentityChanged {
            stage: "opening the selected device for configuration-6 reveal",
        });
    }
    let handle = device.open()?;
    if let Some(expected) = candidate.identity.serial_number.as_deref() {
        let observed = handle
            .read_serial_number_string_ascii(&descriptor)
            .map_err(Error::Usb)?;
        if normalize_apple_usb_serial(&observed)? != expected {
            return Err(Error::IdentityChanged {
                stage: "validating the selected device serial for configuration-6 reveal",
            });
        }
    }
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use crate::{APPLE_VENDOR_ID, DeviceIdentity, UsbAddress, UsbConfigurationInfo};

    use super::*;

    #[derive(Debug, Default)]
    struct FakeBackend {
        scans: VecDeque<Vec<IphoneCandidate>>,
        reveal_count: usize,
        select_count: usize,
        selections: Vec<(u8, bool)>,
        sleep_count: usize,
    }

    impl Config6Backend for FakeBackend {
        fn reveal(&mut self, _candidate: &IphoneCandidate, _timeout: Duration) -> Result<()> {
            self.reveal_count += 1;
            Ok(())
        }

        fn scan(&mut self) -> Result<Vec<IphoneCandidate>> {
            Ok(self.scans.pop_front().unwrap_or_default())
        }

        fn select(
            &mut self,
            _candidate: &IphoneCandidate,
            configuration: u8,
            private_apple_configuration: bool,
            _policy: Config6Policy,
        ) -> Result<()> {
            self.select_count += 1;
            self.selections
                .push((configuration, private_apple_configuration));
            Ok(())
        }

        fn sleep(&mut self, _duration: Duration) {
            self.sleep_count += 1;
        }
    }

    fn candidate(configurations: &[u8], active: Option<u8>) -> IphoneCandidate {
        IphoneCandidate {
            identity: DeviceIdentity {
                address: UsbAddress { bus: 1, address: 2 },
                vendor_id: APPLE_VENDOR_ID,
                product_id: 0x12a8,
                serial_number: Some("device-udid".to_owned()),
            },
            active_configuration: active,
            configurations: configurations
                .iter()
                .map(|&value| UsbConfigurationInfo {
                    value,
                    interfaces: Vec::new(),
                })
                .collect(),
        }
    }

    fn timeouts() -> Config6Timeouts {
        Config6Timeouts {
            control_transfer: Duration::from_millis(1),
            reconnect: Duration::from_millis(3),
            poll_interval: Duration::from_millis(1),
        }
    }

    #[test]
    fn reveal_reconnect_and_select_reconnect_are_both_required() {
        let backend = FakeBackend {
            scans: VecDeque::from([
                Vec::new(),
                vec![candidate(&[1, 6], Some(1))],
                Vec::new(),
                vec![candidate(&[1, 6], Some(6))],
            ]),
            ..FakeBackend::default()
        };
        let mut activator = Config6Activator::new(backend, timeouts());
        let active = activator
            .activate(
                &candidate(&[1], Some(1)),
                Config6Policy {
                    allow_private_configuration: true,
                    allow_configuration_change: true,
                },
            )
            .expect("configuration 6");
        assert_eq!(active.active_configuration, Some(6));
        assert_eq!(activator.backend.reveal_count, 1);
        assert_eq!(activator.backend.select_count, 1);
        assert_eq!(activator.backend.sleep_count, 2);
    }

    #[test]
    fn reconnect_timeout_is_reported_without_fallback() {
        let mut activator = Config6Activator::new(FakeBackend::default(), timeouts());
        assert!(matches!(
            activator.activate(
                &candidate(&[1], Some(1)),
                Config6Policy {
                    allow_private_configuration: true,
                    allow_configuration_change: true,
                },
            ),
            Err(Error::Timeout {
                operation: "configuration 6 reveal",
                ..
            })
        ));
    }

    #[test]
    fn private_configuration_is_fail_closed() {
        let mut activator = Config6Activator::new(FakeBackend::default(), timeouts());
        assert!(matches!(
            activator.activate(&candidate(&[1, 6], Some(6)), Config6Policy::default()),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn restore_selects_the_previous_public_configuration() {
        let backend = FakeBackend {
            scans: VecDeque::from([Vec::new(), vec![candidate(&[1, 6], Some(1))]]),
            ..FakeBackend::default()
        };
        let mut activator = Config6Activator::new(backend, timeouts());
        let restored = activator
            .restore(
                &candidate(&[1, 6], Some(6)),
                Some(1),
                Config6Policy {
                    allow_private_configuration: true,
                    allow_configuration_change: true,
                },
            )
            .expect("restore");
        assert_eq!(restored.active_configuration, Some(1));
        assert_eq!(activator.backend.selections, [(1, false)]);
    }
}
