use std::collections::BTreeSet;

use bytes::{BufMut, Bytes, BytesMut};
use thiserror::Error;

use super::{CsmError, CsmFrame, CsmParameter, auth, encode_parameters};
use crate::csm::{
    carplay::{
        CARPLAY_AVAILABILITY, CARPLAY_START_SESSION, DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION,
    },
    wifi::{
        ACCESSORY_WIFI_CONFIGURATION_INFORMATION, REQUEST_ACCESSORY_WIFI_CONFIGURATION_INFORMATION,
    },
};

pub const START_IDENTIFICATION: u16 = 0x1d00;
pub const IDENTIFICATION_INFORMATION: u16 = 0x1d01;
pub const IDENTIFICATION_ACCEPTED: u16 = 0x1d02;
pub const IDENTIFICATION_REJECTED: u16 = 0x1d03;
pub const CANCEL_IDENTIFICATION: u16 = 0x1d05;
pub const IDENTIFICATION_INFORMATION_UPDATE: u16 = 0x1d06;

const WIRED_CARPLAY_USB_HOST_COMPONENT_ID: u16 = 0;
const WIRED_CARPLAY_USB_HOST_COMPONENT_NAME: &str = "USBHostTransport";
const WIRELESS_BLUETOOTH_COMPONENT_ID: u16 = 0;
const WIRELESS_BLUETOOTH_COMPONENT_NAME: &str = "BluetoothTransport";
const WIRELESS_CARPLAY_COMPONENT_ID: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PowerProvidingCapability {
    None = 0,
    Reserved = 1,
    Advanced = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MatchAction {
    NoAction = 0,
    OptionalAction = 1,
    NoAlert = 2,
    NoCommunicationProtocol = 3,
    NoActionNoCommunication = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum UsbDeviceAudioSampleRate {
    Hz8000 = 0,
    Hz11025 = 1,
    Hz12000 = 2,
    Hz16000 = 3,
    Hz22050 = 4,
    Hz24000 = 5,
    Hz32000 = 6,
    Hz44100 = 7,
    Hz48000 = 8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EngineType {
    Gasoline = 0,
    Diesel = 1,
    Electric = 2,
    CompressedNaturalGas = 3,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalAccessoryProtocol {
    pub id: u8,
    pub name: String,
    pub match_action: MatchAction,
    pub native_transport_component_id: Option<u16>,
    pub supports_carplay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDeviceTransportComponent {
    pub id: u16,
    pub name: String,
    pub supports_iap2_connection: bool,
    pub audio_sample_rates: Vec<UsbDeviceAudioSampleRate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbHostTransportComponent {
    pub id: u16,
    pub name: String,
    pub supports_iap2_connection: bool,
    pub carplay_interface_number: Option<u8>,
    pub supports_carplay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BluetoothTransportComponent {
    pub id: u16,
    pub name: String,
    pub supports_iap2_connection: bool,
    pub mac_address: [u8; 6],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VehicleInformationComponent {
    pub id: u16,
    pub name: String,
    pub engine_type: EngineType,
    pub display_name: Option<String>,
    pub maps_display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VehicleStatusComponent {
    pub id: u16,
    pub name: String,
    pub range: bool,
    pub outside_temperature: bool,
    pub range_warning: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationInformationComponent {
    pub id: u16,
    pub name: String,
    pub gps_fix_data: bool,
    pub recommended_minimum_gps_transit_data: bool,
    pub vehicle_speed_data: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WirelessCarPlayTransportComponent {
    pub id: u16,
    pub name: String,
    pub supports_iap2_connection: bool,
    pub supports_carplay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteGuidanceDisplayComponent {
    pub id: u16,
    pub name: String,
    pub maximum_current_road_name_length: Option<u16>,
    pub maximum_destination_name_length: Option<u16>,
    pub maximum_after_maneuver_road_name_length: Option<u16>,
    pub maximum_maneuver_description_length: Option<u16>,
    pub maximum_guidance_maneuver_storage_capacity: Option<u16>,
    pub maximum_lane_guidance_description_length: Option<u16>,
    pub maximum_lane_guidance_storage_capacity: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentificationInformation {
    pub name: String,
    pub model_identifier: String,
    pub manufacturer: String,
    pub serial_number: String,
    pub firmware_version: String,
    pub hardware_version: String,
    pub additional_messages_sent_by_accessory: Vec<u16>,
    pub additional_messages_received_from_device: Vec<u16>,
    pub power_providing_capability: PowerProvidingCapability,
    pub maximum_current_drawn_from_device_ma: u16,
    pub external_accessory_protocols: Vec<ExternalAccessoryProtocol>,
    pub app_match_team_id: Option<String>,
    pub current_language: String,
    pub supported_languages: Vec<String>,
    pub usb_device_transport: Option<UsbDeviceTransportComponent>,
    pub usb_host_transport: Option<UsbHostTransportComponent>,
    pub bluetooth_transport: Option<BluetoothTransportComponent>,
    pub vehicle_information: Option<VehicleInformationComponent>,
    pub vehicle_status: Option<VehicleStatusComponent>,
    pub location_information: Option<LocationInformationComponent>,
    pub wireless_carplay_transport: Option<WirelessCarPlayTransportComponent>,
    pub route_guidance_displays: Vec<RouteGuidanceDisplayComponent>,
}

impl IdentificationInformation {
    #[allow(clippy::too_many_arguments)]
    pub fn wired_carplay(
        name: impl Into<String>,
        model_identifier: impl Into<String>,
        manufacturer: impl Into<String>,
        serial_number: impl Into<String>,
        firmware_version: impl Into<String>,
        hardware_version: impl Into<String>,
        carplay_interface_number: u8,
    ) -> Self {
        Self {
            name: name.into(),
            model_identifier: model_identifier.into(),
            manufacturer: manufacturer.into(),
            serial_number: serial_number.into(),
            firmware_version: firmware_version.into(),
            hardware_version: hardware_version.into(),
            additional_messages_sent_by_accessory: vec![CARPLAY_START_SESSION],
            additional_messages_received_from_device: vec![
                CARPLAY_AVAILABILITY,
                DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION,
            ],
            power_providing_capability: PowerProvidingCapability::None,
            maximum_current_drawn_from_device_ma: 0,
            external_accessory_protocols: Vec::new(),
            app_match_team_id: None,
            current_language: "en".into(),
            supported_languages: vec!["en".into()],
            usb_device_transport: None,
            usb_host_transport: Some(UsbHostTransportComponent {
                id: WIRED_CARPLAY_USB_HOST_COMPONENT_ID,
                name: WIRED_CARPLAY_USB_HOST_COMPONENT_NAME.into(),
                supports_iap2_connection: true,
                carplay_interface_number: Some(carplay_interface_number),
                supports_carplay: true,
            }),
            bluetooth_transport: None,
            vehicle_information: None,
            vehicle_status: None,
            location_information: None,
            wireless_carplay_transport: None,
            route_guidance_displays: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn wireless_carplay(
        name: impl Into<String>,
        model_identifier: impl Into<String>,
        manufacturer: impl Into<String>,
        serial_number: impl Into<String>,
        firmware_version: impl Into<String>,
        hardware_version: impl Into<String>,
        bluetooth_mac_address: [u8; 6],
        access_point_ssid: impl Into<String>,
    ) -> Self {
        let mut identification = Self::wired_carplay(
            name,
            model_identifier,
            manufacturer,
            serial_number,
            firmware_version,
            hardware_version,
            0,
        );
        identification.usb_host_transport = None;
        identification
            .additional_messages_sent_by_accessory
            .push(ACCESSORY_WIFI_CONFIGURATION_INFORMATION);
        identification
            .additional_messages_received_from_device
            .push(REQUEST_ACCESSORY_WIFI_CONFIGURATION_INFORMATION);
        identification.bluetooth_transport = Some(BluetoothTransportComponent {
            id: WIRELESS_BLUETOOTH_COMPONENT_ID,
            name: WIRELESS_BLUETOOTH_COMPONENT_NAME.into(),
            supports_iap2_connection: true,
            mac_address: bluetooth_mac_address,
        });
        identification.wireless_carplay_transport = Some(WirelessCarPlayTransportComponent {
            id: WIRELESS_CARPLAY_COMPONENT_ID,
            name: access_point_ssid.into(),
            supports_iap2_connection: true,
            supports_carplay: true,
        });
        identification
    }

    pub fn validate(&self) -> Result<(), IdentificationError> {
        for (field, value) in [
            ("name", self.name.as_str()),
            ("model_identifier", self.model_identifier.as_str()),
            ("manufacturer", self.manufacturer.as_str()),
            ("serial_number", self.serial_number.as_str()),
            ("firmware_version", self.firmware_version.as_str()),
            ("hardware_version", self.hardware_version.as_str()),
            ("current_language", self.current_language.as_str()),
        ] {
            require_nonempty(field, value)?;
        }
        if self.supported_languages.is_empty()
            || !self
                .supported_languages
                .iter()
                .any(|language| language == &self.current_language)
        {
            return Err(IdentificationError::CurrentLanguageNotSupported);
        }
        for language in &self.supported_languages {
            require_nonempty("supported_language", language)?;
        }

        let mut transport_ids = BTreeSet::new();
        if let Some(component) = &self.usb_device_transport {
            validate_component(component.id, &component.name, &mut transport_ids)?;
            let rates: BTreeSet<_> = component.audio_sample_rates.iter().copied().collect();
            if !component.audio_sample_rates.is_empty()
                && ![
                    UsbDeviceAudioSampleRate::Hz32000,
                    UsbDeviceAudioSampleRate::Hz44100,
                    UsbDeviceAudioSampleRate::Hz48000,
                ]
                .into_iter()
                .all(|required| rates.contains(&required))
            {
                return Err(IdentificationError::MissingRequiredUsbAudioRate);
            }
        }
        if let Some(component) = &self.usb_host_transport {
            validate_component(component.id, &component.name, &mut transport_ids)?;
            if component.supports_carplay && component.carplay_interface_number.is_none() {
                return Err(IdentificationError::MissingCarPlayInterfaceNumber);
            }
        }
        if let Some(component) = &self.bluetooth_transport {
            validate_component(component.id, &component.name, &mut transport_ids)?;
        }
        if let Some(component) = &self.wireless_carplay_transport {
            validate_component(component.id, &component.name, &mut transport_ids)?;
        }
        let supports_iap2 = self
            .usb_device_transport
            .as_ref()
            .is_some_and(|component| component.supports_iap2_connection)
            || self
                .usb_host_transport
                .as_ref()
                .is_some_and(|component| component.supports_iap2_connection)
            || self
                .bluetooth_transport
                .as_ref()
                .is_some_and(|component| component.supports_iap2_connection)
            || self
                .wireless_carplay_transport
                .as_ref()
                .is_some_and(|component| component.supports_iap2_connection);
        if !supports_iap2 {
            return Err(IdentificationError::NoIap2Transport);
        }

        let mut protocol_ids = BTreeSet::new();
        for protocol in &self.external_accessory_protocols {
            require_nonempty("external_accessory_protocol.name", &protocol.name)?;
            if !protocol_ids.insert(protocol.id) {
                return Err(IdentificationError::DuplicateProtocolId(protocol.id));
            }
            if let Some(id) = protocol.native_transport_component_id {
                if !transport_ids.contains(&id) {
                    return Err(IdentificationError::UnknownNativeTransport(id));
                }
            }
        }
        Ok(())
    }

    pub fn to_frame(&self) -> Result<CsmFrame, IdentificationError> {
        self.validate()?;
        let mut parameters = vec![
            string_parameter(0, &self.name)?,
            string_parameter(1, &self.model_identifier)?,
            string_parameter(2, &self.manufacturer)?,
            string_parameter(3, &self.serial_number)?,
            string_parameter(4, &self.firmware_version)?,
            string_parameter(5, &self.hardware_version)?,
            CsmParameter::new(6, message_list(self.messages_sent_by_accessory())?),
            CsmParameter::new(7, message_list(self.messages_received_from_device())?),
            CsmParameter::new(
                8,
                Bytes::copy_from_slice(&[self.power_providing_capability as u8]),
            ),
            CsmParameter::new(
                9,
                Bytes::copy_from_slice(&self.maximum_current_drawn_from_device_ma.to_be_bytes()),
            ),
        ];

        for protocol in &self.external_accessory_protocols {
            let mut group = vec![
                CsmParameter::new(0, Bytes::copy_from_slice(&[protocol.id])),
                string_parameter(1, &protocol.name)?,
                CsmParameter::new(2, Bytes::copy_from_slice(&[protocol.match_action as u8])),
            ];
            if let Some(id) = protocol.native_transport_component_id {
                group.push(u16_parameter(3, id));
            }
            if protocol.supports_carplay {
                group.push(CsmParameter::presence(4));
            }
            parameters.push(group_parameter(10, &group)?);
        }
        if let Some(team_id) = &self.app_match_team_id {
            parameters.push(string_parameter(11, team_id)?);
        }
        parameters.push(string_parameter(12, &self.current_language)?);
        for language in &self.supported_languages {
            parameters.push(string_parameter(13, language)?);
        }

        if let Some(component) = &self.usb_device_transport {
            let mut group = transport_group(
                component.id,
                &component.name,
                component.supports_iap2_connection,
            )?;
            for rate in &component.audio_sample_rates {
                group.push(CsmParameter::new(3, Bytes::copy_from_slice(&[*rate as u8])));
            }
            parameters.push(group_parameter(15, &group)?);
        }
        if let Some(component) = &self.usb_host_transport {
            let mut group = transport_group(
                component.id,
                &component.name,
                component.supports_iap2_connection,
            )?;
            if let Some(interface) = component.carplay_interface_number {
                group.push(CsmParameter::new(3, Bytes::copy_from_slice(&[interface])));
            }
            if component.supports_carplay {
                group.push(CsmParameter::presence(4));
            }
            parameters.push(group_parameter(16, &group)?);
        }
        if let Some(component) = &self.bluetooth_transport {
            let mut group = transport_group(
                component.id,
                &component.name,
                component.supports_iap2_connection,
            )?;
            group.push(CsmParameter::new(
                3,
                Bytes::copy_from_slice(&component.mac_address),
            ));
            parameters.push(group_parameter(17, &group)?);
        }
        if let Some(component) = &self.vehicle_information {
            let mut group = vec![
                u16_parameter(0, component.id),
                string_parameter(1, &component.name)?,
                CsmParameter::new(2, Bytes::copy_from_slice(&[component.engine_type as u8])),
            ];
            push_optional_string(&mut group, 6, component.display_name.as_deref())?;
            push_optional_string(&mut group, 8, component.maps_display_name.as_deref())?;
            parameters.push(group_parameter(20, &group)?);
        }
        if let Some(component) = &self.vehicle_status {
            let mut group = vec![
                u16_parameter(0, component.id),
                string_parameter(1, &component.name)?,
            ];
            push_presence(&mut group, 3, component.range);
            push_presence(&mut group, 4, component.outside_temperature);
            push_presence(&mut group, 6, component.range_warning);
            parameters.push(group_parameter(21, &group)?);
        }
        if let Some(component) = &self.location_information {
            let mut group = vec![
                u16_parameter(0, component.id),
                string_parameter(1, &component.name)?,
            ];
            push_presence(&mut group, 17, component.gps_fix_data);
            push_presence(
                &mut group,
                18,
                component.recommended_minimum_gps_transit_data,
            );
            push_presence(&mut group, 20, component.vehicle_speed_data);
            parameters.push(group_parameter(22, &group)?);
        }
        if let Some(component) = &self.wireless_carplay_transport {
            let mut group = transport_group(
                component.id,
                &component.name,
                component.supports_iap2_connection,
            )?;
            if component.supports_carplay {
                group.push(CsmParameter::presence(4));
            }
            parameters.push(group_parameter(24, &group)?);
        }
        for component in &self.route_guidance_displays {
            let mut group = vec![
                u16_parameter(0, component.id),
                string_parameter(1, &component.name)?,
            ];
            for (id, value) in [
                (2, component.maximum_current_road_name_length),
                (3, component.maximum_destination_name_length),
                (4, component.maximum_after_maneuver_road_name_length),
                (5, component.maximum_maneuver_description_length),
                (6, component.maximum_guidance_maneuver_storage_capacity),
                (7, component.maximum_lane_guidance_description_length),
                (8, component.maximum_lane_guidance_storage_capacity),
            ] {
                if let Some(value) = value {
                    group.push(u16_parameter(id, value));
                }
            }
            parameters.push(group_parameter(30, &group)?);
        }

        Ok(CsmFrame {
            message_id: IDENTIFICATION_INFORMATION,
            parameters,
        })
    }

    pub fn messages_sent_by_accessory(&self) -> Vec<u16> {
        merge_message_lists(&[
            auth::SENT_BY_ACCESSORY_CONTROL_V2,
            &self.additional_messages_sent_by_accessory,
        ])
    }

    pub fn messages_received_from_device(&self) -> Vec<u16> {
        merge_message_lists(&[
            auth::RECEIVED_BY_ACCESSORY_CONTROL_V2,
            &self.additional_messages_received_from_device,
        ])
    }
}

fn merge_message_lists(lists: &[&[u16]]) -> Vec<u16> {
    lists
        .iter()
        .flat_map(|list| list.iter().copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn message_list(messages: Vec<u16>) -> Result<Bytes, IdentificationError> {
    let mut output = BytesMut::with_capacity(messages.len().saturating_mul(2));
    for message in messages {
        output.put_u16(message);
    }
    Ok(output.freeze())
}

fn string_parameter(id: u16, value: &str) -> Result<CsmParameter, IdentificationError> {
    require_nonempty("encoded_string", value)?;
    if value.as_bytes().contains(&0) {
        return Err(IdentificationError::EmbeddedNul(id));
    }
    let mut payload = BytesMut::with_capacity(value.len() + 1);
    payload.extend_from_slice(value.as_bytes());
    payload.put_u8(0);
    Ok(CsmParameter::new(id, payload.freeze()))
}

fn u16_parameter(id: u16, value: u16) -> CsmParameter {
    CsmParameter::new(id, Bytes::copy_from_slice(&value.to_be_bytes()))
}

fn group_parameter(id: u16, group: &[CsmParameter]) -> Result<CsmParameter, IdentificationError> {
    Ok(CsmParameter::new(id, encode_parameters(group)?))
}

fn transport_group(
    id: u16,
    name: &str,
    supports_iap2_connection: bool,
) -> Result<Vec<CsmParameter>, IdentificationError> {
    let mut group = vec![u16_parameter(0, id), string_parameter(1, name)?];
    push_presence(&mut group, 2, supports_iap2_connection);
    Ok(group)
}

fn push_presence(group: &mut Vec<CsmParameter>, id: u16, present: bool) {
    if present {
        group.push(CsmParameter::presence(id));
    }
}

fn push_optional_string(
    group: &mut Vec<CsmParameter>,
    id: u16,
    value: Option<&str>,
) -> Result<(), IdentificationError> {
    if let Some(value) = value {
        group.push(string_parameter(id, value)?);
    }
    Ok(())
}

fn validate_component(
    id: u16,
    name: &str,
    ids: &mut BTreeSet<u16>,
) -> Result<(), IdentificationError> {
    require_nonempty("transport_component.name", name)?;
    if !ids.insert(id) {
        return Err(IdentificationError::DuplicateTransportId(id));
    }
    Ok(())
}

fn require_nonempty(field: &'static str, value: &str) -> Result<(), IdentificationError> {
    if value.trim().is_empty() {
        return Err(IdentificationError::EmptyString(field));
    }
    Ok(())
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum IdentificationError {
    #[error("identification field {0} must not be empty")]
    EmptyString(&'static str),
    #[error("identification string parameter {0} contains an embedded NUL")]
    EmbeddedNul(u16),
    #[error("current language is absent from supported languages")]
    CurrentLanguageNotSupported,
    #[error("no declared transport supports the active iAP2 connection")]
    NoIap2Transport,
    #[error("duplicate transport component id {0}")]
    DuplicateTransportId(u16),
    #[error("duplicate external accessory protocol id {0}")]
    DuplicateProtocolId(u8),
    #[error("external accessory protocol refers to unknown transport id {0}")]
    UnknownNativeTransport(u16),
    #[error("USB Device Mode audio omits one of 32kHz, 44.1kHz, or 48kHz")]
    MissingRequiredUsbAudioRate,
    #[error("CarPlay-capable USB transport has no CarPlay interface number")]
    MissingCarPlayInterfaceNumber,
    #[error("CSM encoding failed: {0}")]
    Csm(#[from] CsmError),
}
