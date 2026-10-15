use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub const APPLE_VENDOR_ID: u16 = 0x05ac;
pub const APPLE_USB_AUDIO_ADAPTER_PRODUCT_ID: u16 = 0x110a;
const MAX_APPLE_USB_SERIAL_BYTES: usize = 96;

pub(crate) fn normalize_apple_usb_serial(value: &str) -> Result<String> {
    let value = value.trim_end_matches('\0');
    if value.is_empty()
        || value.len() > MAX_APPLE_USB_SERIAL_BYTES
        || !value.is_ascii()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(Error::InvalidDescriptor(
            "Apple USB serial descriptor is malformed".to_owned(),
        ));
    }
    Ok(value.to_ascii_uppercase())
}

fn is_apple_mobile_candidate(vendor_id: u16, product_id: u16) -> bool {
    if vendor_id != APPLE_VENDOR_ID || product_id == APPLE_USB_AUDIO_ADAPTER_PRODUCT_ID {
        return false;
    }
    product_id & 0xff00 == 0x1200 && matches!(product_id & 0x00f0, 0x0090 | 0x00a0)
}

fn is_iphone_product_name(value: &str) -> bool {
    value == "iPhone"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UsbAddress {
    pub bus: u8,
    pub address: u8,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub address: UsbAddress,
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial_number: Option<String>,
}

impl std::fmt::Debug for DeviceIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceIdentity")
            .field("address", &self.address)
            .field("vendor_id", &format_args!("0x{:04x}", self.vendor_id))
            .field("product_id", &format_args!("0x{:04x}", self.product_id))
            .field(
                "serial_number",
                &self.serial_number.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

impl DeviceIdentity {
    pub fn is_apple(&self) -> bool {
        self.vendor_id == APPLE_VENDOR_ID
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointInfo {
    pub address: u8,
    pub max_packet_size: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbInterfaceInfo {
    pub number: u8,
    pub alternate_setting: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub endpoints: Vec<EndpointInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbConfigurationInfo {
    pub value: u8,
    pub interfaces: Vec<UsbInterfaceInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IphoneCandidate {
    pub identity: DeviceIdentity,
    pub active_configuration: Option<u8>,
    pub configurations: Vec<UsbConfigurationInfo>,
}

pub trait DeviceDiscovery: std::fmt::Debug + Send + Sync {
    fn scan(&self) -> Result<Vec<IphoneCandidate>>;
}

#[derive(Debug, Default)]
pub struct LibUsbDiscovery;

impl DeviceDiscovery for LibUsbDiscovery {
    fn scan(&self) -> Result<Vec<IphoneCandidate>> {
        let devices = rusb::devices()?;
        let mut candidates = Vec::new();
        for device in devices.iter() {
            let descriptor = device.device_descriptor()?;
            if !is_apple_mobile_candidate(descriptor.vendor_id(), descriptor.product_id()) {
                continue;
            }

            let handle = device.open().ok();
            let product_is_known_non_iphone = handle
                .as_ref()
                .and_then(|handle| handle.read_product_string_ascii(&descriptor).ok())
                .is_some_and(|product| !is_iphone_product_name(&product));
            if product_is_known_non_iphone {
                continue;
            }
            let serial_number = handle
                .as_ref()
                .and_then(|handle| handle.read_serial_number_string_ascii(&descriptor).ok())
                .map(|serial| normalize_apple_usb_serial(&serial))
                .transpose()?;
            let active_configuration = device
                .active_config_descriptor()
                .ok()
                .map(|configuration| configuration.number());
            let mut configurations = Vec::new();
            for index in 0..descriptor.num_configurations() {
                let configuration = device.config_descriptor(index)?;
                let mut interfaces = Vec::new();
                for interface in configuration.interfaces() {
                    for interface_descriptor in interface.descriptors() {
                        let endpoints = interface_descriptor
                            .endpoint_descriptors()
                            .map(|endpoint| EndpointInfo {
                                address: endpoint.address(),
                                max_packet_size: endpoint.max_packet_size(),
                            })
                            .collect();
                        interfaces.push(UsbInterfaceInfo {
                            number: interface_descriptor.interface_number(),
                            alternate_setting: interface_descriptor.setting_number(),
                            class: interface_descriptor.class_code(),
                            subclass: interface_descriptor.sub_class_code(),
                            protocol: interface_descriptor.protocol_code(),
                            endpoints,
                        });
                    }
                }
                configurations.push(UsbConfigurationInfo {
                    value: configuration.number(),
                    interfaces,
                });
            }
            candidates.push(IphoneCandidate {
                identity: DeviceIdentity {
                    address: UsbAddress {
                        bus: device.bus_number(),
                        address: device.address(),
                    },
                    vendor_id: descriptor.vendor_id(),
                    product_id: descriptor.product_id(),
                    serial_number,
                },
                active_configuration,
                configurations,
            });
        }
        candidates.sort_by_key(|candidate| {
            (
                candidate.identity.address.bus,
                candidate.identity.address.address,
            )
        });
        Ok(candidates)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConfigurationPolicy {
    pub allow_configuration_changes: bool,
    pub allow_private_apple_configurations: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigurationRequest {
    pub value: u8,
    pub private_apple_configuration: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredDevice {
    pub identity: DeviceIdentity,
    pub active_configuration: u8,
}

pub trait UsbConfigurationController: std::fmt::Debug + Send + Sync {
    fn configure(
        &self,
        candidate: &IphoneCandidate,
        request: ConfigurationRequest,
        policy: ConfigurationPolicy,
    ) -> Result<ConfiguredDevice>;
}

#[derive(Debug, Default)]
pub struct LibUsbConfigurator;

impl UsbConfigurationController for LibUsbConfigurator {
    fn configure(
        &self,
        candidate: &IphoneCandidate,
        request: ConfigurationRequest,
        policy: ConfigurationPolicy,
    ) -> Result<ConfiguredDevice> {
        if !candidate.identity.is_apple() {
            return Err(Error::Unsupported(
                "only Apple USB devices may enter a carkit session",
            ));
        }
        if !candidate
            .configurations
            .iter()
            .any(|configuration| configuration.value == request.value)
        {
            return Err(Error::ConfigurationMissing(request.value));
        }
        if candidate.active_configuration != Some(request.value)
            && !policy.allow_configuration_changes
        {
            return Err(Error::Unsupported(
                "USB configuration changes are disabled by policy",
            ));
        }
        if request.private_apple_configuration && !policy.allow_private_apple_configurations {
            return Err(Error::Unsupported(
                "private Apple USB configurations are disabled by policy",
            ));
        }

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
                stage: "opening the selected device for configuration",
            });
        }
        let handle = device.open()?;
        let observed_product = handle
            .read_product_string_ascii(&descriptor)
            .ok()
            .filter(|product| !is_iphone_product_name(product));
        if observed_product.is_some() {
            return Err(Error::IdentityChanged {
                stage: "validating the selected device product before configuration",
            });
        }
        let observed_serial = handle
            .read_serial_number_string_ascii(&descriptor)
            .ok()
            .map(|serial| normalize_apple_usb_serial(&serial))
            .transpose()?;
        if candidate.identity.serial_number.is_some()
            && observed_serial != candidate.identity.serial_number
        {
            return Err(Error::IdentityChanged {
                stage: "validating the selected device serial before configuration",
            });
        }
        if candidate.active_configuration != Some(request.value) {
            match handle.set_auto_detach_kernel_driver(true) {
                Ok(()) | Err(rusb::Error::NotSupported) => {}
                Err(error) => return Err(error.into()),
            }
            if let Ok(configuration) = device.active_config_descriptor() {
                for interface in configuration.interfaces() {
                    let number = interface.number();
                    match handle.kernel_driver_active(number) {
                        Ok(true) => handle.detach_kernel_driver(number)?,
                        Ok(false) | Err(rusb::Error::NotSupported) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            handle.set_active_configuration(request.value)?;
        }
        Ok(ConfiguredDevice {
            identity: candidate.identity.clone(),
            active_configuration: request.value,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_usb_serial_normalization_removes_only_trailing_nul_padding() {
        assert_eq!(
            normalize_apple_usb_serial("0000811000093062360a401e\0\0\0").expect("serial"),
            "0000811000093062360A401E"
        );
        assert!(normalize_apple_usb_serial("00008110\0BAD").is_err());
        assert!(normalize_apple_usb_serial("../../phone").is_err());
        assert!(normalize_apple_usb_serial("").is_err());
    }

    #[test]
    fn default_policy_is_fail_closed() {
        assert_eq!(
            ConfigurationPolicy::default(),
            ConfigurationPolicy {
                allow_configuration_changes: false,
                allow_private_apple_configurations: false,
            }
        );
    }

    #[test]
    fn only_apple_vendor_is_recognized() {
        let identity = DeviceIdentity {
            address: UsbAddress { bus: 1, address: 2 },
            vendor_id: APPLE_VENDOR_ID,
            product_id: 0x1234,
            serial_number: None,
        };
        assert!(identity.is_apple());
    }

    #[test]
    fn apple_usb_headphone_adapter_is_not_an_iphone_candidate() {
        assert!(!is_apple_mobile_candidate(
            APPLE_VENDOR_ID,
            APPLE_USB_AUDIO_ADAPTER_PRODUCT_ID
        ));
        assert!(is_apple_mobile_candidate(APPLE_VENDOR_ID, 0x12a8));
        assert!(is_apple_mobile_candidate(APPLE_VENDOR_ID, 0x1290));
        assert!(!is_apple_mobile_candidate(APPLE_VENDOR_ID, 0x1901));
        assert!(!is_apple_mobile_candidate(APPLE_VENDOR_ID, 0x8600));
        assert!(!is_apple_mobile_candidate(0x1234, 0x12a8));
        assert!(!is_apple_mobile_candidate(APPLE_VENDOR_ID, 0x030d));
        assert!(!is_apple_mobile_candidate(APPLE_VENDOR_ID, 0x1402));
        assert!(!is_apple_mobile_candidate(APPLE_VENDOR_ID, 0x1202));
        assert!(is_iphone_product_name("iPhone"));
        assert!(!is_iphone_product_name("iPad"));
        assert!(!is_iphone_product_name("Mac"));
    }

    #[test]
    fn device_debug_redacts_the_serial_number() {
        let identity = DeviceIdentity {
            address: UsbAddress { bus: 1, address: 2 },
            vendor_id: APPLE_VENDOR_ID,
            product_id: 0x1234,
            serial_number: Some("sensitive-udid".to_owned()),
        };
        let debug = format!("{identity:?}");
        assert!(!debug.contains("sensitive-udid"));
        assert!(debug.contains("[redacted]"));
    }
}
