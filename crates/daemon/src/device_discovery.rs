use std::time::Duration;

use prelude_android_auto::{AoapCandidate, CancellationToken, RusbBackend};
use prelude_usb_carkit::{DeviceDiscovery, IphoneCandidate, LibUsbDiscovery};

use std::net::TcpStream;

use prelude_wireless::bluez::RfcommConnection;

use crate::{Error, Result, WirelessAndroidAutoPhone, WirelessCarPlayPhone, WirelessPhones};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionDevice {
    CarPlay(IphoneCandidate),
    AndroidAuto(AoapCandidate),
    WirelessAndroidAuto(WirelessAndroidAutoPhone),
    WirelessCarPlay(WirelessCarPlayPhone),
}

impl ProjectionDevice {
    pub const fn protocol_name(&self) -> &'static str {
        match self {
            Self::CarPlay(_) | Self::WirelessCarPlay(_) => "CarPlay",
            Self::AndroidAuto(_) | Self::WirelessAndroidAuto(_) => "Android Auto",
        }
    }

    pub const fn is_wireless(&self) -> bool {
        matches!(
            self,
            Self::WirelessAndroidAuto(_) | Self::WirelessCarPlay(_)
        )
    }

    fn sort_key(&self) -> (u8, u8, u8) {
        match self {
            Self::WirelessCarPlay(_) => (u8::MAX, u8::MAX, 2),
            Self::WirelessAndroidAuto(_) => (u8::MAX, u8::MAX, 3),
            Self::CarPlay(candidate) => (
                candidate.identity.address.bus,
                candidate.identity.address.address,
                0,
            ),
            Self::AndroidAuto(candidate) => {
                (candidate.device.topology.bus, candidate.device.address, 1)
            }
        }
    }
}

pub trait ProjectionDeviceDiscovery: std::fmt::Debug + Send {
    fn scan(&mut self) -> Result<Vec<ProjectionDevice>>;
}

#[derive(Debug)]
struct WirelessDiscovery {
    android_auto: WirelessPhones<TcpStream>,
    carplay: WirelessPhones<RfcommConnection>,
}

#[derive(Debug)]
pub struct CombinedDeviceDiscovery {
    carplay: LibUsbDiscovery,
    android_auto: RusbBackend,
    android_probe_timeout: Duration,
    android_auto_enabled: bool,
    android_vendor_ids: Vec<u16>,
    wireless: Option<WirelessDiscovery>,
}

impl CombinedDeviceDiscovery {
    pub fn new(
        android_probe_timeout: Duration,
        android_auto_enabled: bool,
        android_vendor_ids: Vec<u16>,
    ) -> Self {
        Self {
            carplay: LibUsbDiscovery,
            android_auto: RusbBackend,
            android_probe_timeout,
            android_auto_enabled,
            android_vendor_ids,
            wireless: None,
        }
    }

    #[must_use]
    pub fn with_wireless(
        mut self,
        android_auto: WirelessPhones<TcpStream>,
        carplay: WirelessPhones<RfcommConnection>,
    ) -> Self {
        self.wireless = Some(WirelessDiscovery {
            android_auto,
            carplay,
        });
        self
    }
}

impl ProjectionDeviceDiscovery for CombinedDeviceDiscovery {
    fn scan(&mut self) -> Result<Vec<ProjectionDevice>> {
        let mut devices = self
            .carplay
            .scan()?
            .into_iter()
            .map(ProjectionDevice::CarPlay)
            .collect::<Vec<_>>();
        if self.android_auto_enabled {
            let cancellation = CancellationToken::default();
            devices.extend(
                prelude_android_auto::discover_with_vendor_ids(
                    &mut self.android_auto,
                    self.android_probe_timeout,
                    &cancellation,
                    &self.android_vendor_ids,
                )
                .map_err(|error| {
                    Error::Runtime(format!("Android Open Accessory discovery failed: {error}"))
                })?
                .into_iter()
                .map(ProjectionDevice::AndroidAuto),
            );
        }
        if let Some(wireless) = &self.wireless {
            let wired_present = !devices.is_empty();
            devices.extend(
                wireless
                    .carplay
                    .snapshot()
                    .into_iter()
                    .filter(|phone| !wired_present || phone.is_in_use())
                    .map(ProjectionDevice::WirelessCarPlay),
            );
            devices.extend(
                wireless
                    .android_auto
                    .snapshot()
                    .into_iter()
                    .filter(|phone| !wired_present || phone.is_in_use())
                    .map(ProjectionDevice::WirelessAndroidAuto),
            );
        }
        devices.sort_by_key(ProjectionDevice::sort_key);
        Ok(devices)
    }
}
