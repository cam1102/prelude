#![cfg_attr(not(any(test, target_os = "linux")), allow(dead_code, unused_imports))]

use std::{
    collections::BTreeSet,
    fmt, fs, io,
    net::Ipv6Addr,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(target_os = "linux")]
use std::{
    process::{Command, Stdio},
    thread::{self, JoinHandle},
    time::Instant,
};

#[cfg(target_os = "linux")]
use rusb::{Direction, GlobalContext, Recipient, RequestType, TransferType};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
use crate::IphoneCandidate;
use crate::{
    CancellationToken, DeviceIdentity, Error, NcmFrame, Ntb16, Ntb16Encoder, NtbParameters, Result,
    TapEndpoint, discovery::normalize_apple_usb_serial,
};

const APPLE_VENDOR_HEX: &str = "05ac";
const CARPLAY_NCM_CONTROL_INTERFACE: u8 = 3;
const CARPLAY_NCM_DATA_INTERFACE: u8 = 4;
#[cfg(target_os = "linux")]
const CDC_COMMUNICATIONS_CLASS: u8 = 0x02;
#[cfg(target_os = "linux")]
const CDC_NCM_SUBCLASS: u8 = 0x0d;
#[cfg(target_os = "linux")]
const CDC_DATA_CLASS: u8 = 0x0a;
const CDC_FUNCTIONAL_DESCRIPTOR: u8 = 0x24;
const CDC_UNION_SUBTYPE: u8 = 0x06;
const CDC_ETHERNET_SUBTYPE: u8 = 0x0f;
const CDC_NCM_SUBTYPE: u8 = 0x1a;
#[cfg(target_os = "linux")]
const GET_NTB_PARAMETERS: u8 = 0x80;
#[cfg(target_os = "linux")]
const SET_ETHERNET_PACKET_FILTER: u8 = 0x43;
#[cfg(target_os = "linux")]
const SET_NTB_FORMAT: u8 = 0x84;
#[cfg(target_os = "linux")]
const SET_NTB_INPUT_SIZE: u8 = 0x86;
#[cfg(target_os = "linux")]
const SET_CRC_MODE: u8 = 0x8a;
const NTB_PARAMETERS_LENGTH: usize = 28;
const NTB16_SUPPORTED: u16 = 1 << 0;
const NTB32_SUPPORTED: u16 = 1 << 1;
const NTB_MINIMUM_INPUT_SIZE: usize = 2_048;
const NTB_PREFERRED_INPUT_SIZE: usize = 16_384;
const NCM_CAPABILITY_ETHERNET_FILTER: u8 = 1 << 0;
#[cfg(target_os = "linux")]
const NCM_CAPABILITY_CRC_MODE: u8 = 1 << 4;
const PACKET_FILTER_IPV6_LINK_LOCAL: u16 = (1 << 1) | (1 << 2) | (1 << 3);
const IPV6_LINK_LOCAL_SCOPE: u32 = 0x20;
const IFA_F_DADFAILED: u32 = 0x08;
const IFA_F_TENTATIVE: u32 = 0x40;
const MAX_INTERFACE_NAME_BYTES: usize = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WiredNcmConfig {
    pub sysfs_usb_devices: PathBuf,
    pub sysfs_net_class: PathBuf,
    pub if_inet6_path: PathBuf,
    pub tap_name: String,
    pub kernel_bind_wait_timeout: Duration,
    pub link_wait_timeout: Duration,
    pub poll_interval: Duration,
    pub usb_transfer_timeout: Duration,
    pub usb_write_timeout: Duration,
    pub command_timeout: Duration,
}

impl Default for WiredNcmConfig {
    fn default() -> Self {
        Self {
            sysfs_usb_devices: PathBuf::from("/sys/bus/usb/devices"),
            sysfs_net_class: PathBuf::from("/sys/class/net"),
            if_inet6_path: PathBuf::from("/proc/net/if_inet6"),
            tap_name: "cpusb0".to_owned(),
            kernel_bind_wait_timeout: Duration::from_secs(2),
            link_wait_timeout: Duration::from_secs(20),
            poll_interval: Duration::from_millis(100),
            usb_transfer_timeout: Duration::from_millis(100),
            usb_write_timeout: Duration::from_secs(2),
            command_timeout: Duration::from_secs(3),
        }
    }
}

impl WiredNcmConfig {
    pub fn validate(&self) -> Result<()> {
        if !self.sysfs_usb_devices.is_absolute()
            || !self.sysfs_net_class.is_absolute()
            || !self.if_inet6_path.is_absolute()
        {
            return Err(Error::Ncm(
                "sysfs and procfs paths must be absolute".to_owned(),
            ));
        }
        validate_interface_name(&self.tap_name)?;
        if self.kernel_bind_wait_timeout.is_zero()
            || self.link_wait_timeout.is_zero()
            || self.poll_interval.is_zero()
            || self.usb_transfer_timeout.is_zero()
            || self.usb_write_timeout.is_zero()
            || self.command_timeout.is_zero()
        {
            return Err(Error::Ncm("NCM timeouts must be non-zero".to_owned()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NcmCarrierKind {
    Kernel,
    Userspace,
}

#[derive(Clone, PartialEq, Eq)]
struct UsbAssociation {
    sysfs_name: String,
    bus: u8,
    address: u8,
}

impl fmt::Debug for UsbAssociation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UsbAssociation")
            .field("sysfs_name", &self.sysfs_name)
            .field("bus", &self.bus)
            .field("address", &self.address)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KernelNcmInterface {
    name: String,
    driver: String,
}

fn associate_usb_device(root: &Path, identity: &DeviceIdentity) -> Result<UsbAssociation> {
    let expected_serial = identity.serial_number.as_deref().ok_or(Error::NotReady {
        component: "wired NCM association",
        state: "iphone_udid_unavailable",
    })?;
    let mut matches = Vec::new();
    for entry in fs::read_dir(root).map_err(|source| Error::io("scanning USB sysfs", source))? {
        let entry = entry.map_err(|source| Error::io("scanning USB sysfs", source))?;
        let path = entry.path();
        if !path.join("busnum").exists() || !path.join("devnum").exists() {
            continue;
        }
        let bus = match read_decimal_u8(&path.join("busnum")) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let address = match read_decimal_u8(&path.join("devnum")) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if bus != identity.address.bus || address != identity.address.address {
            continue;
        }
        let vendor = read_trimmed(&path.join("idVendor"))?;
        if !vendor.eq_ignore_ascii_case(APPLE_VENDOR_HEX) {
            return Err(Error::Ncm(
                "the USB sysfs topology slot no longer contains an Apple device".to_owned(),
            ));
        }
        let product = u16::from_str_radix(&read_trimmed(&path.join("idProduct"))?, 16)
            .map_err(|_| Error::Ncm("USB sysfs product ID is malformed".to_owned()))?;
        if product != identity.product_id {
            return Err(Error::IdentityChanged {
                stage: "associating the NCM sysfs product",
            });
        }
        let serial = normalize_apple_usb_serial(&read_trimmed(&path.join("serial"))?)?;
        if serial != expected_serial {
            return Err(Error::IdentityChanged {
                stage: "associating the NCM sysfs serial",
            });
        }
        let sysfs_name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::Ncm("USB sysfs topology name is not UTF-8".to_owned()))?;
        matches.push(UsbAssociation {
            sysfs_name,
            bus,
            address,
        });
    }
    match matches.len() {
        0 => Err(Error::NotReady {
            component: "wired NCM association",
            state: "selected_iphone_topology_missing",
        }),
        1 => Ok(matches.remove(0)),
        _ => Err(Error::Ncm(
            "multiple USB sysfs entries matched the selected iPhone".to_owned(),
        )),
    }
}

fn find_kernel_ncm(
    root: &Path,
    association: &UsbAssociation,
    descriptors: &NcmDescriptorSet,
) -> Result<Option<KernelNcmInterface>> {
    let prefix = format!("{}:", association.sysfs_name);
    let mut matches = Vec::new();
    for entry in
        fs::read_dir(root).map_err(|source| Error::io("scanning USB interfaces", source))?
    {
        let entry = entry.map_err(|source| Error::io("scanning USB interfaces", source))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        let driver_link = entry.path().join("driver");
        let Ok(driver_target) = fs::read_link(driver_link) else {
            continue;
        };
        let Some(driver) = driver_target.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if driver != "cdc_ncm" {
            continue;
        }
        let interface_number = match read_trimmed(&entry.path().join("bInterfaceNumber")) {
            Ok(value) => u8::from_str_radix(&value, 16)
                .map_err(|_| Error::Ncm("USB interface number is malformed".to_owned()))?,
            Err(_) => continue,
        };
        if interface_number != descriptors.control_interface {
            continue;
        }
        let net = entry.path().join("net");
        for net_entry in fs::read_dir(net)
            .map_err(|source| Error::io("scanning USB network interfaces", source))?
        {
            let net_entry =
                net_entry.map_err(|source| Error::io("scanning USB network interfaces", source))?;
            let interface = net_entry
                .file_name()
                .into_string()
                .map_err(|_| Error::Ncm("network interface name is not UTF-8".to_owned()))?;
            validate_interface_name(&interface)?;
            matches.push(KernelNcmInterface {
                name: interface,
                driver: driver.to_owned(),
            });
        }
    }
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => Err(Error::Ncm(
            "multiple kernel NCM interfaces matched the selected iPhone topology".to_owned(),
        )),
    }
}

fn read_decimal_u8(path: &Path) -> Result<u8> {
    read_trimmed(path)?
        .parse()
        .map_err(|_| Error::Ncm("USB sysfs address is malformed".to_owned()))
}

fn read_trimmed(path: &Path) -> Result<String> {
    let value =
        fs::read_to_string(path).map_err(|source| Error::io("reading USB sysfs", source))?;
    let value = value.trim();
    if value.is_empty() || value.len() > 128 || value.as_bytes().contains(&0) {
        return Err(Error::Ncm("USB sysfs property is malformed".to_owned()));
    }
    Ok(value.to_owned())
}

fn validate_interface_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_INTERFACE_NAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(Error::Ncm("network interface name is unsafe".to_owned()));
    }
    Ok(())
}

fn parse_link_local_addresses(contents: &str, interface: &str) -> Result<Vec<Ipv6Addr>> {
    validate_interface_name(interface)?;
    let mut addresses = BTreeSet::new();
    for line in contents.lines() {
        let fields: Vec<_> = line.split_ascii_whitespace().collect();
        if fields.len() != 6 || fields[5] != interface {
            continue;
        }
        if fields[0].len() != 32 {
            return Err(Error::Ncm(
                "kernel IPv6 address record is malformed".to_owned(),
            ));
        }
        let scope = u32::from_str_radix(fields[3], 16)
            .map_err(|_| Error::Ncm("kernel IPv6 scope is malformed".to_owned()))?;
        let flags = u32::from_str_radix(fields[4], 16)
            .map_err(|_| Error::Ncm("kernel IPv6 address flags are malformed".to_owned()))?;
        let mut octets = [0_u8; 16];
        for (index, octet) in octets.iter_mut().enumerate() {
            *octet = u8::from_str_radix(&fields[0][index * 2..index * 2 + 2], 16)
                .map_err(|_| Error::Ncm("kernel IPv6 address is malformed".to_owned()))?;
        }
        let address = Ipv6Addr::from(octets);
        if scope == IPV6_LINK_LOCAL_SCOPE && address.is_unicast_link_local() {
            if flags & IFA_F_DADFAILED != 0 {
                return Err(Error::Ncm(
                    "NCM link-local IPv6 duplicate-address detection failed".to_owned(),
                ));
            }
            if flags & IFA_F_TENTATIVE != 0 {
                continue;
            }
            addresses.insert(address);
        }
    }
    Ok(addresses.into_iter().collect())
}

#[cfg(target_os = "linux")]
fn read_link_local(path: &Path, interface: &str) -> Result<Option<Ipv6Addr>> {
    let contents = fs::read_to_string(path)
        .map_err(|source| Error::io("reading kernel IPv6 interface state", source))?;
    let addresses = parse_link_local_addresses(&contents, interface)?;
    match addresses.as_slice() {
        [] => Ok(None),
        [address] => Ok(Some(*address)),
        _ => Err(Error::Ncm(
            "network interface has multiple link-local IPv6 addresses".to_owned(),
        )),
    }
}

#[cfg(target_os = "linux")]
fn wait_for_link_local(
    config: &WiredNcmConfig,
    interface: &str,
    cancellation: &CancellationToken,
) -> Result<Ipv6Addr> {
    let deadline = Instant::now() + config.link_wait_timeout;
    loop {
        cancellation.check("waiting for the NCM link-local IPv6 address")?;
        if !config.sysfs_net_class.join(interface).exists() {
            return Err(Error::NotReady {
                component: "wired NCM interface",
                state: "selected_network_interface_disappeared",
            });
        }
        if let Some(address) = read_link_local(&config.if_inet6_path, interface)? {
            return Ok(address);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(Error::Timeout {
                operation: "waiting for an actual NCM link-local IPv6 address",
                timeout: config.link_wait_timeout,
            });
        }
        std::thread::sleep(
            config
                .poll_interval
                .min(deadline.saturating_duration_since(now)),
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CdcFunctionals {
    union: Option<(u8, Vec<u8>)>,
    mac_index: Option<u8>,
    network_capabilities: Option<u8>,
}

fn parse_cdc_functionals(extra: &[u8]) -> Result<CdcFunctionals> {
    let mut result = CdcFunctionals {
        union: None,
        mac_index: None,
        network_capabilities: None,
    };
    let mut offset = 0;
    while offset < extra.len() {
        let length = usize::from(*extra.get(offset).ok_or_else(|| {
            Error::InvalidDescriptor("truncated CDC functional descriptor".to_owned())
        })?);
        if length < 2 || offset + length > extra.len() {
            return Err(Error::InvalidDescriptor(
                "malformed CDC functional descriptor length".to_owned(),
            ));
        }
        let descriptor = &extra[offset..offset + length];
        if descriptor[1] == CDC_FUNCTIONAL_DESCRIPTOR && length >= 3 {
            match descriptor[2] {
                CDC_UNION_SUBTYPE => {
                    if length < 5 || result.union.is_some() {
                        return Err(Error::InvalidDescriptor(
                            "ambiguous CDC union descriptor".to_owned(),
                        ));
                    }
                    result.union = Some((descriptor[3], descriptor[4..].to_vec()));
                }
                CDC_ETHERNET_SUBTYPE => {
                    if length < 4 || descriptor[3] == 0 || result.mac_index.is_some() {
                        return Err(Error::InvalidDescriptor(
                            "ambiguous CDC Ethernet MAC descriptor".to_owned(),
                        ));
                    }
                    result.mac_index = Some(descriptor[3]);
                }
                CDC_NCM_SUBTYPE => {
                    if length < 6 || result.network_capabilities.is_some() {
                        return Err(Error::InvalidDescriptor(
                            "ambiguous CDC NCM functional descriptor".to_owned(),
                        ));
                    }
                    result.network_capabilities = Some(descriptor[5]);
                }
                _ => {}
            }
        }
        offset += length;
    }
    Ok(result)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedNtbParameters {
    advertised_input_maximum: usize,
    negotiated_input_maximum: usize,
    supports_ntb32: bool,
    output: NtbParameters,
}

fn parse_ntb_parameters(bytes: &[u8]) -> Result<ParsedNtbParameters> {
    if bytes.len() < NTB_PARAMETERS_LENGTH {
        return Err(Error::InvalidDescriptor(
            "GET_NTB_PARAMETERS response is truncated".to_owned(),
        ));
    }
    let declared_length = usize::from(read_le_u16(bytes, 0)?);
    if declared_length < NTB_PARAMETERS_LENGTH || declared_length > bytes.len() {
        return Err(Error::InvalidDescriptor(
            "GET_NTB_PARAMETERS response length is invalid".to_owned(),
        ));
    }
    let formats = read_le_u16(bytes, 2)?;
    if formats & NTB16_SUPPORTED == 0 {
        return Err(Error::Unsupported(
            "iPhone NCM interface does not support NTB16",
        ));
    }
    let input_maximum = usize::try_from(read_le_u32(bytes, 4)?)
        .map_err(|_| Error::InvalidDescriptor("NCM input size is invalid".to_owned()))?;
    let maximum_size = usize::try_from(read_le_u32(bytes, 16)?)
        .map_err(|_| Error::InvalidDescriptor("NCM output size is invalid".to_owned()))?;
    let parameters = NtbParameters {
        maximum_size: maximum_size.min(usize::from(u16::MAX)),
        payload_divisor: usize::from(read_le_u16(bytes, 20)?),
        payload_remainder: usize::from(read_le_u16(bytes, 22)?),
        alignment: usize::from(read_le_u16(bytes, 24)?),
        maximum_datagrams: usize::from(read_le_u16(bytes, 26)?).max(1),
    };
    Ntb16Encoder::new(parameters)?;
    if input_maximum < NTB_MINIMUM_INPUT_SIZE {
        return Err(Error::InvalidDescriptor(
            "NCM input maximum is below the CDC-NCM minimum".to_owned(),
        ));
    }
    let advertised_input_maximum = input_maximum.min(usize::from(u16::MAX));
    Ok(ParsedNtbParameters {
        advertised_input_maximum,
        negotiated_input_maximum: advertised_input_maximum.min(NTB_PREFERRED_INPUT_SIZE),
        supports_ntb32: formats & NTB32_SUPPORTED != 0,
        output: parameters,
    })
}

fn read_le_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| Error::InvalidDescriptor("truncated NCM parameter".to_owned()))?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_le_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| Error::InvalidDescriptor("truncated NCM parameter".to_owned()))?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn parse_mac(value: &str) -> Result<[u8; 6]> {
    if value.len() != 12 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::InvalidDescriptor(
            "CDC Ethernet MAC string is malformed".to_owned(),
        ));
    }
    let mut result = [0_u8; 6];
    for (index, octet) in result.iter_mut().enumerate() {
        *octet = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| Error::InvalidDescriptor("CDC Ethernet MAC is malformed".to_owned()))?;
    }
    if result == [0; 6] || result[0] & 1 != 0 {
        return Err(Error::InvalidDescriptor(
            "CDC Ethernet MAC is not a usable unicast address".to_owned(),
        ));
    }
    Ok(result)
}

fn parse_kernel_mac(value: &str) -> Result<[u8; 6]> {
    if value.len() != 17
        || value.bytes().enumerate().any(|(index, byte)| {
            index % 3 == 2 && byte != b':' || index % 3 != 2 && !byte.is_ascii_hexdigit()
        })
    {
        return Err(Error::Ncm(
            "kernel NCM interface MAC address is malformed".to_owned(),
        ));
    }
    let compact: String = value
        .chars()
        .filter(|character| *character != ':')
        .collect();
    parse_mac(&compact)
        .map_err(|_| Error::Ncm("kernel NCM interface MAC address is invalid".to_owned()))
}

#[cfg(target_os = "linux")]
fn read_kernel_mac(net_class: &Path, interface: &str) -> Result<[u8; 6]> {
    validate_interface_name(interface)?;
    parse_kernel_mac(&read_trimmed(&net_class.join(interface).join("address"))?)
}

trait NcmUsbTransport: fmt::Debug + Send + 'static {
    fn input_maximum_size(&self) -> usize;
    fn output_parameters(&self) -> NtbParameters;
    fn read_ntb(&mut self, bytes: &mut [u8], timeout: Duration) -> Result<Option<usize>>;
    fn write_ntb(&mut self, bytes: &[u8], timeout: Duration) -> Result<()>;
}

fn run_bridge<U: NcmUsbTransport, T: TapEndpoint>(
    mut usb: U,
    mut tap: T,
    cancellation: &CancellationToken,
    read_timeout: Duration,
    write_timeout: Duration,
) -> Result<()> {
    let mut input = vec![0_u8; usb.input_maximum_size()];
    let mut encoder = Ntb16Encoder::new(usb.output_parameters())?;
    loop {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        if let Some(length) = usb.read_ntb(&mut input, read_timeout)? {
            if length == 0 || length > input.len() {
                return Err(Error::InvalidNtb(
                    "USB NCM read returned an invalid length".to_owned(),
                ));
            }
            let ntb = Ntb16::parse(&input[..length])?;
            for frame in ntb.frames {
                tap.transmit(&frame)
                    .map_err(|source| Error::io("writing an NCM frame to TAP", source))?;
            }
        }
        if let Some(frame) = tap
            .receive(Duration::ZERO)
            .map_err(|source| Error::io("reading an Ethernet frame from TAP", source))?
        {
            let ntb = encoder.encode(&[frame])?;
            match usb.write_ntb(&ntb, write_timeout) {
                Ok(()) | Err(Error::Usb(rusb::Error::Timeout)) => {}
                Err(error) => return Err(error),
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NcmDescriptorSet {
    control_interface: u8,
    data_interface: u8,
    data_alternate: u8,
    bulk_in: u8,
    bulk_out: u8,
    mac_index: u8,
    network_capabilities: u8,
}

fn select_ncm_descriptors(matches: Vec<NcmDescriptorSet>) -> Result<NcmDescriptorSet> {
    let mut carplay = matches.into_iter().filter(|candidate| {
        candidate.control_interface == CARPLAY_NCM_CONTROL_INTERFACE
            && candidate.data_interface == CARPLAY_NCM_DATA_INTERFACE
    });
    let selected = carplay.next().ok_or_else(|| {
        Error::InvalidDescriptor(
            "configuration 6 has no CarPlay NCM control/data interface 3/4".to_owned(),
        )
    })?;
    if carplay.next().is_some() {
        return Err(Error::InvalidDescriptor(
            "configuration 6 duplicates the CarPlay NCM interface pair".to_owned(),
        ));
    }
    Ok(selected)
}

#[cfg(target_os = "linux")]
fn inspect_ncm_descriptors(
    candidate: &IphoneCandidate,
    association: &UsbAssociation,
) -> Result<NcmDescriptorSet> {
    if candidate.active_configuration != Some(6) {
        return Err(Error::NotReady {
            component: "wired NCM descriptor inspection",
            state: "configuration_6_not_active",
        });
    }
    let devices = rusb::devices()?;
    let device = devices
        .iter()
        .find(|device| {
            device.bus_number() == association.bus && device.address() == association.address
        })
        .ok_or(Error::DeviceNotFound {
            bus: association.bus,
            address: association.address,
        })?;
    let device_descriptor = device.device_descriptor()?;
    if device_descriptor.vendor_id() != candidate.identity.vendor_id
        || device_descriptor.product_id() != candidate.identity.product_id
    {
        return Err(Error::IdentityChanged {
            stage: "inspecting the configuration-6 NCM descriptors",
        });
    }
    let handle = device.open()?;
    let actual_serial =
        normalize_apple_usb_serial(&handle.read_serial_number_string_ascii(&device_descriptor)?)?;
    if candidate.identity.serial_number.as_deref() != Some(actual_serial.as_str()) {
        return Err(Error::IdentityChanged {
            stage: "validating the NCM descriptor device serial",
        });
    }
    let configuration = device.active_config_descriptor()?;
    if configuration.number() != 6 {
        return Err(Error::IdentityChanged {
            stage: "validating the NCM descriptor USB configuration",
        });
    }
    discover_ncm_descriptors(&configuration)
}

#[cfg(target_os = "linux")]
fn discover_ncm_descriptors(configuration: &rusb::ConfigDescriptor) -> Result<NcmDescriptorSet> {
    let mut matches = Vec::new();
    for interface in configuration.interfaces() {
        for control in interface.descriptors() {
            if control.class_code() != CDC_COMMUNICATIONS_CLASS
                || control.sub_class_code() != CDC_NCM_SUBCLASS
            {
                continue;
            }
            let functional = parse_cdc_functionals(control.extra())?;
            let (master, slaves) = functional.union.ok_or_else(|| {
                Error::InvalidDescriptor("NCM control interface has no CDC union".to_owned())
            })?;
            if master != control.interface_number() || slaves.len() != 1 {
                return Err(Error::InvalidDescriptor(
                    "NCM CDC union does not identify exactly one data interface".to_owned(),
                ));
            }
            let mac_index = functional.mac_index.ok_or_else(|| {
                Error::InvalidDescriptor("NCM control interface has no Ethernet MAC".to_owned())
            })?;
            let network_capabilities = functional.network_capabilities.ok_or_else(|| {
                Error::InvalidDescriptor(
                    "NCM control interface has no NCM functional descriptor".to_owned(),
                )
            })?;
            let data_number = slaves[0];
            let data = configuration
                .interfaces()
                .find(|candidate| candidate.number() == data_number)
                .ok_or_else(|| {
                    Error::InvalidDescriptor("NCM data interface is absent".to_owned())
                })?;
            for alternate in data.descriptors() {
                if alternate.setting_number() != 1 || alternate.class_code() != CDC_DATA_CLASS {
                    continue;
                }
                let mut bulk_in = None;
                let mut bulk_out = None;
                for endpoint in alternate.endpoint_descriptors() {
                    if endpoint.transfer_type() != TransferType::Bulk {
                        continue;
                    }
                    match endpoint.direction() {
                        Direction::In if bulk_in.replace(endpoint.address()).is_some() => {
                            return Err(Error::InvalidDescriptor(
                                "NCM data interface has multiple bulk IN endpoints".to_owned(),
                            ));
                        }
                        Direction::Out if bulk_out.replace(endpoint.address()).is_some() => {
                            return Err(Error::InvalidDescriptor(
                                "NCM data interface has multiple bulk OUT endpoints".to_owned(),
                            ));
                        }
                        _ => {}
                    }
                }
                if let (Some(bulk_in), Some(bulk_out)) = (bulk_in, bulk_out) {
                    matches.push(NcmDescriptorSet {
                        control_interface: control.interface_number(),
                        data_interface: data_number,
                        data_alternate: 1,
                        bulk_in,
                        bulk_out,
                        mac_index,
                        network_capabilities,
                    });
                }
            }
        }
    }
    if matches.is_empty() {
        Err(Error::NotReady {
            component: "userspace NCM",
            state: "cdc_ncm_interfaces_missing",
        })
    } else {
        select_ncm_descriptors(matches)
    }
}

#[cfg(target_os = "linux")]
struct RusbNcmTransport {
    handle: rusb::DeviceHandle<GlobalContext>,
    descriptors: NcmDescriptorSet,
    input_maximum: usize,
    output_parameters: NtbParameters,
}

#[cfg(target_os = "linux")]
impl fmt::Debug for RusbNcmTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RusbNcmTransport")
            .field("control_interface", &self.descriptors.control_interface)
            .field("data_interface", &self.descriptors.data_interface)
            .field("bulk_in", &self.descriptors.bulk_in)
            .field("bulk_out", &self.descriptors.bulk_out)
            .finish_non_exhaustive()
    }
}

#[cfg(target_os = "linux")]
impl RusbNcmTransport {
    fn open(
        candidate: &IphoneCandidate,
        association: &UsbAssociation,
        timeout: Duration,
    ) -> Result<(Self, [u8; 6])> {
        if candidate.active_configuration != Some(6) {
            return Err(Error::NotReady {
                component: "userspace NCM",
                state: "configuration_6_not_active",
            });
        }
        let devices = rusb::devices()?;
        let device = devices
            .iter()
            .find(|device| {
                device.bus_number() == association.bus && device.address() == association.address
            })
            .ok_or(Error::DeviceNotFound {
                bus: association.bus,
                address: association.address,
            })?;
        let device_descriptor = device.device_descriptor()?;
        if device_descriptor.vendor_id() != candidate.identity.vendor_id
            || device_descriptor.product_id() != candidate.identity.product_id
        {
            return Err(Error::IdentityChanged {
                stage: "opening the userspace NCM transport",
            });
        }
        let handle = device.open()?;
        let actual_serial = normalize_apple_usb_serial(
            &handle.read_serial_number_string_ascii(&device_descriptor)?,
        )?;
        if candidate.identity.serial_number.as_deref() != Some(actual_serial.as_str()) {
            return Err(Error::IdentityChanged {
                stage: "validating the userspace NCM USB serial",
            });
        }
        let configuration = device.active_config_descriptor()?;
        if configuration.number() != 6 {
            return Err(Error::IdentityChanged {
                stage: "validating the userspace NCM USB configuration",
            });
        }
        let descriptors = discover_ncm_descriptors(&configuration)?;
        for interface in [descriptors.control_interface, descriptors.data_interface] {
            match handle.kernel_driver_active(interface) {
                Ok(false) => {}
                Ok(true) => {
                    return Err(Error::NotReady {
                        component: "userspace NCM",
                        state: "kernel_driver_bound",
                    });
                }
                Err(error) => return Err(Error::Usb(error)),
            }
        }
        handle.claim_interface(descriptors.control_interface)?;
        if let Err(error) = handle.claim_interface(descriptors.data_interface) {
            let _ = handle.release_interface(descriptors.control_interface);
            return Err(Error::Usb(error));
        }
        let setup = (|| {
            let mut parameter_bytes = [0_u8; NTB_PARAMETERS_LENGTH];
            let request_type =
                rusb::request_type(Direction::In, RequestType::Class, Recipient::Interface);
            let length = handle.read_control(
                request_type,
                GET_NTB_PARAMETERS,
                0,
                u16::from(descriptors.control_interface),
                &mut parameter_bytes,
                timeout,
            )?;
            let parameters = parse_ntb_parameters(&parameter_bytes[..length])?;
            let request_type =
                rusb::request_type(Direction::Out, RequestType::Class, Recipient::Interface);
            if descriptors.network_capabilities & NCM_CAPABILITY_CRC_MODE != 0 {
                let written = handle.write_control(
                    request_type,
                    SET_CRC_MODE,
                    0,
                    u16::from(descriptors.control_interface),
                    &[],
                    timeout,
                )?;
                if written != 0 {
                    return Err(Error::Ncm(
                        "SET_CRC_MODE returned an invalid transfer length".to_owned(),
                    ));
                }
            }
            if parameters.supports_ntb32 {
                let written = handle.write_control(
                    request_type,
                    SET_NTB_FORMAT,
                    0,
                    u16::from(descriptors.control_interface),
                    &[],
                    timeout,
                )?;
                if written != 0 {
                    return Err(Error::Ncm(
                        "SET_NTB_FORMAT returned an invalid transfer length".to_owned(),
                    ));
                }
            }
            if parameters.negotiated_input_maximum != parameters.advertised_input_maximum {
                let input_size = u32::try_from(parameters.negotiated_input_maximum)
                    .map_err(|_| Error::Ncm("NCM input size exceeds u32".to_owned()))?
                    .to_le_bytes();
                let written = handle.write_control(
                    request_type,
                    SET_NTB_INPUT_SIZE,
                    0,
                    u16::from(descriptors.control_interface),
                    &input_size,
                    timeout,
                )?;
                if written != input_size.len() {
                    return Err(Error::Ncm(
                        "SET_NTB_INPUT_SIZE returned an invalid transfer length".to_owned(),
                    ));
                }
            }
            let mac = parse_mac(&handle.read_string_descriptor_ascii(descriptors.mac_index)?)?;
            handle.set_alternate_setting(descriptors.data_interface, descriptors.data_alternate)?;
            if descriptors.network_capabilities & NCM_CAPABILITY_ETHERNET_FILTER != 0 {
                let written = handle.write_control(
                    request_type,
                    SET_ETHERNET_PACKET_FILTER,
                    PACKET_FILTER_IPV6_LINK_LOCAL,
                    u16::from(descriptors.control_interface),
                    &[],
                    timeout,
                )?;
                if written != 0 {
                    return Err(Error::Ncm(
                        "SET_ETHERNET_PACKET_FILTER returned an invalid transfer length".to_owned(),
                    ));
                }
            }
            Ok((parameters.negotiated_input_maximum, parameters.output, mac))
        })();
        let (input_maximum, output_parameters, mac) = match setup {
            Ok(values) => values,
            Err(error) => {
                let _ = handle.release_interface(descriptors.data_interface);
                let _ = handle.release_interface(descriptors.control_interface);
                return Err(error);
            }
        };
        Ok((
            Self {
                handle,
                descriptors,
                input_maximum,
                output_parameters,
            },
            mac,
        ))
    }
}

#[cfg(target_os = "linux")]
impl Drop for RusbNcmTransport {
    fn drop(&mut self) {
        let _ = self
            .handle
            .set_alternate_setting(self.descriptors.data_interface, 0);
        let _ = self
            .handle
            .release_interface(self.descriptors.data_interface);
        let _ = self
            .handle
            .release_interface(self.descriptors.control_interface);
    }
}

#[cfg(target_os = "linux")]
impl NcmUsbTransport for RusbNcmTransport {
    fn input_maximum_size(&self) -> usize {
        self.input_maximum
    }

    fn output_parameters(&self) -> NtbParameters {
        self.output_parameters
    }

    fn read_ntb(&mut self, bytes: &mut [u8], timeout: Duration) -> Result<Option<usize>> {
        match self
            .handle
            .read_bulk(self.descriptors.bulk_in, bytes, timeout)
        {
            Ok(length) => Ok(Some(length)),
            Err(rusb::Error::Timeout) => Ok(None),
            Err(error) => Err(Error::Usb(error)),
        }
    }

    fn write_ntb(&mut self, bytes: &[u8], timeout: Duration) -> Result<()> {
        let written = self
            .handle
            .write_bulk(self.descriptors.bulk_out, bytes, timeout)?;
        if written != bytes.len() {
            return Err(Error::Ncm("short USB NCM bulk write".to_owned()));
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
struct LinuxTap {
    device: tun::Device,
    interface_name: String,
}

#[cfg(target_os = "linux")]
impl fmt::Debug for LinuxTap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LinuxTap")
            .field("interface_name", &self.interface_name)
            .finish_non_exhaustive()
    }
}

#[cfg(target_os = "linux")]
impl LinuxTap {
    fn create(interface_name: &str) -> Result<Self> {
        validate_interface_name(interface_name)?;
        let mut configuration = tun::Configuration::default();
        configuration
            .tun_name(interface_name)
            .layer(tun::Layer::L2)
            .platform_config(|platform| {
                platform.ensure_root_privileges(false);
            });
        let device = tun::create(&configuration)
            .map_err(|source| Error::Ncm(format!("opening /dev/net/tun failed: {source}")))?;
        Ok(Self {
            device,
            interface_name: interface_name.to_owned(),
        })
    }
}

#[cfg(target_os = "linux")]
impl TapEndpoint for LinuxTap {
    fn interface_name(&self) -> &str {
        &self.interface_name
    }

    fn receive(&mut self, timeout: Duration) -> io::Result<Option<NcmFrame>> {
        let mut bytes = vec![0_u8; usize::from(u16::MAX)];
        match self.device.recv_timeout(&mut bytes, timeout) {
            Ok(0) => Ok(None),
            Ok(length) => {
                bytes.truncate(length);
                NcmFrame::new(bytes)
                    .map(Some)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            }
            Err(error) if error.kind() == io::ErrorKind::TimedOut => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn transmit(&mut self, frame: &NcmFrame) -> io::Result<()> {
        let written = self.device.send(frame.as_bytes())?;
        if written != frame.as_bytes().len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short TAP frame write",
            ));
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn choose_ip_binary() -> Result<&'static str> {
    ["/sbin/ip", "/usr/sbin/ip", "/usr/bin/ip"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .ok_or_else(|| Error::Ncm("a fixed-path ip utility is unavailable".to_owned()))
}

#[cfg(target_os = "linux")]
fn run_ip(arguments: &[&str], timeout: Duration) -> Result<()> {
    let binary = choose_ip_binary()?;
    let mut child = Command::new(binary)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| Error::io("starting the fixed-path ip utility", source))?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|source| Error::io("waiting for the fixed-path ip utility", source))?
        {
            if status.success() {
                return Ok(());
            }
            return Err(Error::Ncm(format!(
                "fixed-path ip utility exited with status {status}"
            )));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Timeout {
                operation: "running the fixed-path ip utility",
                timeout,
            });
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn set_link_up(interface: &str, timeout: Duration) -> Result<()> {
    validate_interface_name(interface)?;
    run_ip(&["link", "set", "dev", interface, "up"], timeout)
}

#[cfg(target_os = "linux")]
fn set_link_down(interface: &str, timeout: Duration) -> Result<()> {
    validate_interface_name(interface)?;
    run_ip(&["link", "set", "dev", interface, "down"], timeout)
}

#[cfg(target_os = "linux")]
fn set_tap_mac_and_up(interface: &str, mac: [u8; 6], timeout: Duration) -> Result<()> {
    validate_interface_name(interface)?;
    let mac = format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );
    run_ip(&["link", "set", "dev", interface, "down"], timeout)?;
    run_ip(&["link", "set", "dev", interface, "address", &mac], timeout)?;
    run_ip(
        &["link", "set", "dev", interface, "addrgenmode", "eui64"],
        timeout,
    )?;
    run_ip(&["link", "set", "dev", interface, "up"], timeout)
}

#[cfg(target_os = "linux")]
fn interface_is_up(net_class: &Path, interface: &str) -> Result<bool> {
    validate_interface_name(interface)?;
    let flags = read_trimmed(&net_class.join(interface).join("flags"))?;
    let flags = flags.strip_prefix("0x").unwrap_or(&flags);
    let flags = u32::from_str_radix(flags, 16)
        .map_err(|_| Error::Ncm("network interface flags are malformed".to_owned()))?;
    Ok(flags & 1 != 0)
}

#[cfg(target_os = "linux")]
struct UserspaceNcmSession {
    interface_name: String,
    worker: Option<JoinHandle<Result<()>>>,
    cancellation: CancellationToken,
}

#[cfg(target_os = "linux")]
impl fmt::Debug for UserspaceNcmSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UserspaceNcmSession")
            .field("interface_name", &self.interface_name)
            .field(
                "worker_running",
                &self
                    .worker
                    .as_ref()
                    .is_some_and(|worker| !worker.is_finished()),
            )
            .finish()
    }
}

#[cfg(target_os = "linux")]
impl UserspaceNcmSession {
    fn stop(&mut self) -> Result<()> {
        self.cancellation.cancel();
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker
            .join()
            .map_err(|_| Error::Ncm("userspace NCM bridge panicked".to_owned()))?
    }

    fn is_alive(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
    }
}

#[cfg(target_os = "linux")]
impl Drop for UserspaceNcmSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(target_os = "linux")]
enum NcmSession {
    Kernel {
        interface_name: String,
        restore_down: bool,
    },
    Userspace(UserspaceNcmSession),
}

#[cfg(target_os = "linux")]
pub struct WiredNcm {
    kind: NcmCarrierKind,
    carplay_interface_number: u8,
    link_local: Ipv6Addr,
    mac_address: [u8; 6],
    command_timeout: Duration,
    net_class: PathBuf,
    session: NcmSession,
}

#[cfg(target_os = "linux")]
impl fmt::Debug for WiredNcm {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WiredNcm")
            .field("kind", &self.kind)
            .field("carplay_interface_number", &self.carplay_interface_number)
            .field("interface_name", &self.interface_name())
            .field("link_local_ready", &true)
            .field("alive", &self.is_alive())
            .finish_non_exhaustive()
    }
}

#[cfg(target_os = "linux")]
impl WiredNcm {
    pub fn start(
        candidate: &IphoneCandidate,
        config: WiredNcmConfig,
        cancellation: CancellationToken,
    ) -> Result<Self> {
        config.validate()?;
        let association = associate_usb_device(&config.sysfs_usb_devices, &candidate.identity)?;
        let expected_descriptors = inspect_ncm_descriptors(candidate, &association)?;
        let carplay_interface_number = expected_descriptors.control_interface;
        let kernel_deadline = Instant::now() + config.kernel_bind_wait_timeout;
        let kernel = loop {
            if let Some(kernel) = find_kernel_ncm(
                &config.sysfs_usb_devices,
                &association,
                &expected_descriptors,
            )? {
                break Some(kernel);
            }
            let now = Instant::now();
            if now >= kernel_deadline || cancellation.is_cancelled() {
                break None;
            }
            thread::sleep(
                config
                    .poll_interval
                    .min(kernel_deadline.saturating_duration_since(now)),
            );
        };
        if let Some(kernel) = kernel {
            let was_up = interface_is_up(&config.sysfs_net_class, &kernel.name)?;
            if !was_up {
                set_link_up(&kernel.name, config.command_timeout)?;
            }
            let link_local = match wait_for_link_local(&config, &kernel.name, &cancellation) {
                Ok(address) => Some(address),
                Err(error) => {
                    if !was_up {
                        let _ = set_link_down(&kernel.name, config.command_timeout);
                    }
                    match error {
                        Error::NotReady {
                            component: "wired NCM interface",
                            state: "selected_network_interface_disappeared",
                        } => {}
                        other => return Err(other),
                    }
                    None
                }
            };
            if let Some(link_local) = link_local {
                let mac_address = match read_kernel_mac(&config.sysfs_net_class, &kernel.name) {
                    Ok(mac_address) => mac_address,
                    Err(error) => {
                        if !was_up {
                            let _ = set_link_down(&kernel.name, config.command_timeout);
                        }
                        return Err(error);
                    }
                };
                return Ok(Self {
                    kind: NcmCarrierKind::Kernel,
                    carplay_interface_number,
                    link_local,
                    mac_address,
                    command_timeout: config.command_timeout,
                    net_class: config.sysfs_net_class,
                    session: NcmSession::Kernel {
                        interface_name: kernel.name,
                        restore_down: !was_up,
                    },
                });
            }
        }

        let (usb, host_mac) =
            RusbNcmTransport::open(candidate, &association, config.usb_transfer_timeout)?;
        if usb.descriptors != expected_descriptors {
            return Err(Error::IdentityChanged {
                stage: "revalidating the NCM descriptors before claim",
            });
        }
        if config.sysfs_net_class.join(&config.tap_name).exists() {
            return Err(Error::Ncm(
                "configured TAP interface name is already in use".to_owned(),
            ));
        }
        let tap = LinuxTap::create(&config.tap_name)?;
        set_tap_mac_and_up(&config.tap_name, host_mac, config.command_timeout)?;
        let worker_cancellation = cancellation.clone();
        let read_timeout = config.usb_transfer_timeout;
        let write_timeout = config.usb_write_timeout;
        let worker = thread::Builder::new()
            .name("prelude-ncm".to_owned())
            .spawn(move || run_bridge(usb, tap, &worker_cancellation, read_timeout, write_timeout))
            .map_err(|source| Error::io("starting the userspace NCM bridge", source))?;
        let mut session = UserspaceNcmSession {
            interface_name: config.tap_name.clone(),
            worker: Some(worker),
            cancellation: cancellation.clone(),
        };
        let link_local = match wait_for_link_local(&config, &config.tap_name, &cancellation) {
            Ok(address) if session.is_alive() => address,
            Ok(_) => {
                let bridge_error = session.stop().err();
                return Err(bridge_error.unwrap_or_else(|| {
                    Error::Ncm("userspace NCM bridge stopped during link setup".to_owned())
                }));
            }
            Err(error) => {
                let bridge_error = session.stop().err();
                return Err(bridge_error.unwrap_or(error));
            }
        };
        Ok(Self {
            kind: NcmCarrierKind::Userspace,
            carplay_interface_number,
            link_local,
            mac_address: host_mac,
            command_timeout: config.command_timeout,
            net_class: config.sysfs_net_class,
            session: NcmSession::Userspace(session),
        })
    }

    pub fn kind(&self) -> NcmCarrierKind {
        self.kind
    }

    pub fn carplay_interface_number(&self) -> u8 {
        self.carplay_interface_number
    }

    pub fn link_local_address(&self) -> Ipv6Addr {
        self.link_local
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac_address
    }

    pub fn interface_name(&self) -> &str {
        match &self.session {
            NcmSession::Kernel { interface_name, .. } => interface_name,
            NcmSession::Userspace(session) => &session.interface_name,
        }
    }

    pub fn is_alive(&self) -> bool {
        match &self.session {
            NcmSession::Kernel { interface_name, .. } => {
                self.net_class.join(interface_name).exists()
            }
            NcmSession::Userspace(session) => session.is_alive(),
        }
    }

    fn stop_inner(&mut self) -> Result<()> {
        match &mut self.session {
            NcmSession::Kernel {
                interface_name,
                restore_down,
            } if *restore_down => {
                let result = set_link_down(interface_name, self.command_timeout);
                if result.is_ok() {
                    *restore_down = false;
                }
                result
            }
            NcmSession::Kernel { .. } => Ok(()),
            NcmSession::Userspace(session) => session.stop(),
        }
    }

    pub fn stop(mut self) -> Result<()> {
        self.stop_inner()
    }
}

#[cfg(target_os = "linux")]
impl Drop for WiredNcm {
    fn drop(&mut self) {
        let _ = self.stop_inner();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use tempfile::TempDir;

    use super::*;
    use crate::UsbAddress;

    fn selected_identity(serial: &str) -> DeviceIdentity {
        DeviceIdentity {
            address: UsbAddress { bus: 1, address: 7 },
            vendor_id: crate::APPLE_VENDOR_ID,
            product_id: 0x12ab,
            serial_number: Some(serial.to_owned()),
        }
    }

    fn ncm_descriptors(control_interface: u8, data_interface: u8) -> NcmDescriptorSet {
        NcmDescriptorSet {
            control_interface,
            data_interface,
            data_alternate: 1,
            bulk_in: 0x88,
            bulk_out: 0x06,
            mac_index: 19,
            network_capabilities: NCM_CAPABILITY_ETHERNET_FILTER,
        }
    }

    fn write_device(root: &Path, topology: &str, serial: &str) {
        let device = root.join(topology);
        fs::create_dir_all(&device).expect("device");
        fs::write(device.join("busnum"), "1\n").expect("bus");
        fs::write(device.join("devnum"), "7\n").expect("dev");
        fs::write(device.join("idVendor"), "05ac\n").expect("vendor");
        fs::write(device.join("idProduct"), "12ab\n").expect("product");
        fs::write(device.join("serial"), format!("{serial}\n")).expect("serial");
    }

    #[test]
    fn sysfs_association_requires_exact_topology_address_and_udid() {
        let root = TempDir::new().expect("tempdir");
        let selected = "0000811000093062360A401E";
        write_device(root.path(), "1-2.3", selected);
        let associated =
            associate_usb_device(root.path(), &selected_identity(selected)).expect("association");
        assert_eq!(associated.sysfs_name, "1-2.3");
        assert!(!format!("{associated:?}").contains(selected));
        assert!(matches!(
            associate_usb_device(root.path(), &selected_identity("0000811000093062360A401F")),
            Err(Error::IdentityChanged { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn kernel_driver_must_belong_to_associated_usb_topology() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().expect("tempdir");
        let selected_serial = "0000811000093062360A401E";
        write_device(root.path(), "1-2", selected_serial);
        let selected = root.path().join("1-2:6.3");
        fs::create_dir_all(selected.join("net/iphone0")).expect("net");
        fs::write(selected.join("bInterfaceNumber"), "03\n").expect("interface");
        symlink("/sys/bus/usb/drivers/cdc_ncm", selected.join("driver")).expect("driver");
        let other = root.path().join("1-3:6.3");
        fs::create_dir_all(other.join("net/wrong0")).expect("net");
        fs::write(other.join("bInterfaceNumber"), "03\n").expect("interface");
        symlink("/sys/bus/usb/drivers/cdc_ncm", other.join("driver")).expect("driver");
        let association = associate_usb_device(root.path(), &selected_identity(selected_serial))
            .expect("selected");
        let interface = find_kernel_ncm(root.path(), &association, &ncm_descriptors(3, 4))
            .expect("scan")
            .expect("interface");
        assert_eq!(interface.name, "iphone0");
    }

    #[test]
    fn configuration_six_selects_the_carplay_ncm_pair_when_another_pair_exists() {
        let selected = select_ncm_descriptors(vec![ncm_descriptors(5, 6), ncm_descriptors(3, 4)])
            .expect("CarPlay pair");
        assert_eq!(selected.control_interface, 3);
        assert_eq!(selected.data_interface, 4);

        assert!(select_ncm_descriptors(vec![ncm_descriptors(5, 6)]).is_err());
        assert!(
            select_ncm_descriptors(vec![ncm_descriptors(3, 4), ncm_descriptors(3, 4),]).is_err()
        );
    }

    #[test]
    fn ipv6_policy_accepts_only_actual_link_local_scope_records() {
        let records = concat!(
            "fe80000000000000021122fffe334455 02 40 20 80 cpusb0\n",
            "fe80000000000000021122fffe334477 02 40 20 40 cpusb0\n",
            "20010db8000000000000000000000001 02 40 00 80 cpusb0\n",
            "fe80000000000000021122fffe334466 03 40 20 80 other0\n"
        );
        assert_eq!(
            parse_link_local_addresses(records, "cpusb0").expect("addresses"),
            vec![
                "fe80::211:22ff:fe33:4455"
                    .parse::<Ipv6Addr>()
                    .expect("IPv6")
            ]
        );
        assert!(parse_link_local_addresses(records, "bad/name").is_err());
        assert!(
            parse_link_local_addresses(
                "fe80000000000000021122fffe334455 02 40 20 08 cpusb0\n",
                "cpusb0"
            )
            .is_err()
        );
    }

    #[test]
    fn cdc_functional_and_ntb_parameters_are_strictly_parsed() {
        let extra = [
            5, 0x24, 0x06, 2, 3, 13, 0x24, 0x0f, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6, 0x24, 0x1a, 0x00,
            0x01, 0x21,
        ];
        let parsed = parse_cdc_functionals(&extra).expect("functional");
        assert_eq!(parsed.union, Some((2, vec![3])));
        assert_eq!(parsed.mac_index, Some(4));
        assert_eq!(parsed.network_capabilities, Some(0x21));

        let mut parameters = [0_u8; 28];
        parameters[0..2].copy_from_slice(&28_u16.to_le_bytes());
        parameters[2..4].copy_from_slice(&1_u16.to_le_bytes());
        parameters[4..8].copy_from_slice(&16_384_u32.to_le_bytes());
        parameters[16..20].copy_from_slice(&16_384_u32.to_le_bytes());
        parameters[20..22].copy_from_slice(&4_u16.to_le_bytes());
        parameters[24..26].copy_from_slice(&4_u16.to_le_bytes());
        parameters[26..28].copy_from_slice(&8_u16.to_le_bytes());
        let parsed = parse_ntb_parameters(&parameters).expect("parameters");
        assert_eq!(parsed.advertised_input_maximum, 16_384);
        assert_eq!(parsed.negotiated_input_maximum, 16_384);
        assert!(!parsed.supports_ntb32);
        assert_eq!(parsed.output.maximum_datagrams, 8);

        parameters[2..4].copy_from_slice(&3_u16.to_le_bytes());
        parameters[4..8].copy_from_slice(&65_535_u32.to_le_bytes());
        let parsed = parse_ntb_parameters(&parameters).expect("dual-format parameters");
        assert_eq!(parsed.advertised_input_maximum, 65_535);
        assert_eq!(parsed.negotiated_input_maximum, 16_384);
        assert!(parsed.supports_ntb32);

        parameters[4..8].copy_from_slice(&2_047_u32.to_le_bytes());
        assert!(parse_ntb_parameters(&parameters).is_err());
        parameters[4..8].copy_from_slice(&16_384_u32.to_le_bytes());
        parameters[20..22].fill(0);
        assert!(parse_ntb_parameters(&parameters).is_err());
    }

    #[test]
    fn ethernet_filter_admits_ipv6_neighbor_discovery() {
        const ALL_MULTICAST: u16 = 1 << 1;
        const DIRECTED: u16 = 1 << 2;
        const BROADCAST: u16 = 1 << 3;

        assert_eq!(
            PACKET_FILTER_IPV6_LINK_LOCAL,
            ALL_MULTICAST | DIRECTED | BROADCAST
        );
        assert_ne!(PACKET_FILTER_IPV6_LINK_LOCAL & ALL_MULTICAST, 0);
    }

    #[test]
    fn mac_parser_rejects_multicast_zero_and_decorated_values() {
        assert_eq!(
            parse_mac("021122334455").expect("MAC"),
            [0x02, 0x11, 0x22, 0x33, 0x44, 0x55]
        );
        assert!(parse_mac("011122334455").is_err());
        assert!(parse_mac("000000000000").is_err());
        assert!(parse_mac("02:11:22:33:44:55").is_err());
        assert_eq!(
            parse_kernel_mac("02:11:22:33:44:55").expect("kernel MAC"),
            [0x02, 0x11, 0x22, 0x33, 0x44, 0x55]
        );
        assert!(parse_kernel_mac("02-11-22-33-44-55").is_err());
        assert!(parse_kernel_mac("01:11:22:33:44:55").is_err());
    }

    #[derive(Debug)]
    struct FakeUsb {
        reads: VecDeque<Option<Vec<u8>>>,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        write_timeouts: Arc<Mutex<Vec<Duration>>>,
        timeout_next_write: bool,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for FakeUsb {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Release);
        }
    }

    impl NcmUsbTransport for FakeUsb {
        fn input_maximum_size(&self) -> usize {
            16_384
        }

        fn output_parameters(&self) -> NtbParameters {
            NtbParameters::default()
        }

        fn read_ntb(&mut self, bytes: &mut [u8], _timeout: Duration) -> Result<Option<usize>> {
            match self.reads.pop_front().unwrap_or(None) {
                Some(block) => {
                    bytes[..block.len()].copy_from_slice(&block);
                    Ok(Some(block.len()))
                }
                None => Ok(None),
            }
        }

        fn write_ntb(&mut self, bytes: &[u8], timeout: Duration) -> Result<()> {
            self.writes.lock().expect("writes").push(bytes.to_vec());
            self.write_timeouts
                .lock()
                .expect("write timeouts")
                .push(timeout);
            if self.timeout_next_write {
                self.timeout_next_write = false;
                return Err(Error::Usb(rusb::Error::Timeout));
            }
            Ok(())
        }
    }

    #[derive(Debug)]
    struct FakeTap {
        receives: VecDeque<NcmFrame>,
        transmitted: Arc<Mutex<Vec<NcmFrame>>>,
        cancel_after_transmit: CancellationToken,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for FakeTap {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Release);
        }
    }

    impl TapEndpoint for FakeTap {
        fn interface_name(&self) -> &str {
            "fake0"
        }

        fn receive(&mut self, _timeout: Duration) -> io::Result<Option<NcmFrame>> {
            Ok(self.receives.pop_front())
        }

        fn transmit(&mut self, frame: &NcmFrame) -> io::Result<()> {
            self.transmitted
                .lock()
                .expect("transmitted")
                .push(frame.clone());
            self.cancel_after_transmit.cancel();
            Ok(())
        }
    }

    #[test]
    fn bridge_moves_frames_both_directions_and_stops_cleanly() {
        let cancellation = CancellationToken::default();
        let usb_frame = NcmFrame::new(vec![0xaa; 60]).expect("frame");
        let tap_frame = NcmFrame::new(vec![0xbb; 60]).expect("frame");
        let mut encoder = Ntb16Encoder::new(NtbParameters::default()).expect("encoder");
        let incoming = encoder
            .encode(std::slice::from_ref(&usb_frame))
            .expect("NTB");
        let writes = Arc::new(Mutex::new(Vec::new()));
        let write_timeouts = Arc::new(Mutex::new(Vec::new()));
        let transmitted = Arc::new(Mutex::new(Vec::new()));
        let drops = Arc::new(AtomicUsize::new(0));
        let usb = FakeUsb {
            reads: VecDeque::from([None, Some(incoming)]),
            writes: Arc::clone(&writes),
            write_timeouts: Arc::clone(&write_timeouts),
            timeout_next_write: false,
            drops: Arc::clone(&drops),
        };
        let tap = FakeTap {
            receives: VecDeque::from([tap_frame.clone()]),
            transmitted: Arc::clone(&transmitted),
            cancel_after_transmit: cancellation.clone(),
            drops: Arc::clone(&drops),
        };
        run_bridge(
            usb,
            tap,
            &cancellation,
            Duration::from_millis(1),
            Duration::from_secs(2),
        )
        .expect("bridge");
        assert_eq!(*transmitted.lock().expect("transmitted"), vec![usb_frame]);
        let written = writes.lock().expect("writes");
        assert_eq!(
            Ntb16::parse(&written[0]).expect("NTB").frames,
            vec![tap_frame]
        );
        assert_eq!(
            *write_timeouts.lock().expect("write timeouts"),
            vec![Duration::from_secs(2)]
        );
        assert_eq!(drops.load(Ordering::Acquire), 2);
    }

    #[test]
    fn bridge_drops_a_timed_out_ethernet_write_without_dropping_the_link() {
        let cancellation = CancellationToken::default();
        let usb_frame = NcmFrame::new(vec![0xaa; 60]).expect("frame");
        let tap_frame = NcmFrame::new(vec![0xbb; 60]).expect("frame");
        let mut encoder = Ntb16Encoder::new(NtbParameters::default()).expect("encoder");
        let incoming = encoder
            .encode(std::slice::from_ref(&usb_frame))
            .expect("NTB");
        let transmitted = Arc::new(Mutex::new(Vec::new()));
        let drops = Arc::new(AtomicUsize::new(0));
        let usb = FakeUsb {
            reads: VecDeque::from([None, Some(incoming)]),
            writes: Arc::new(Mutex::new(Vec::new())),
            write_timeouts: Arc::new(Mutex::new(Vec::new())),
            timeout_next_write: true,
            drops: Arc::clone(&drops),
        };
        let tap = FakeTap {
            receives: VecDeque::from([tap_frame]),
            transmitted: Arc::clone(&transmitted),
            cancel_after_transmit: cancellation.clone(),
            drops: Arc::clone(&drops),
        };

        run_bridge(
            usb,
            tap,
            &cancellation,
            Duration::from_millis(1),
            Duration::from_secs(2),
        )
        .expect("bridge stays alive after a timed-out Ethernet write");

        assert_eq!(*transmitted.lock().expect("transmitted"), vec![usb_frame]);
        assert_eq!(drops.load(Ordering::Acquire), 2);
    }

    #[test]
    fn malformed_usb_ntb_fails_bridge_without_reaching_tap() {
        let cancellation = CancellationToken::default();
        let transmitted = Arc::new(Mutex::new(Vec::new()));
        let drops = Arc::new(AtomicUsize::new(0));
        let usb = FakeUsb {
            reads: VecDeque::from([Some(vec![1, 2, 3])]),
            writes: Arc::new(Mutex::new(Vec::new())),
            write_timeouts: Arc::new(Mutex::new(Vec::new())),
            timeout_next_write: false,
            drops: Arc::clone(&drops),
        };
        let tap = FakeTap {
            receives: VecDeque::new(),
            transmitted: Arc::clone(&transmitted),
            cancel_after_transmit: cancellation.clone(),
            drops: Arc::clone(&drops),
        };
        assert!(matches!(
            run_bridge(
                usb,
                tap,
                &cancellation,
                Duration::from_millis(1),
                Duration::from_secs(2),
            ),
            Err(Error::InvalidNtb(_))
        ));
        assert!(transmitted.lock().expect("transmitted").is_empty());
        assert_eq!(drops.load(Ordering::Acquire), 2);
    }
}
