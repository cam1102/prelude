use platform_superbird::AmbientLightState;
use prelude_control::{ImplementationStatus, ProjectionCapabilities};

use crate::{
    AndroidAutoRuntime, AudioRuntimeSnapshot, DaemonConfig, HealthState, ProjectionDevice,
    ProjectionRuntime, Result, RuntimeProbe, WiredCarkitRuntime, WirelessCarPlayRuntime,
    hands_free::HandsFree,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveProtocol {
    CarPlay,
    AndroidAuto,
    WirelessCarPlay,
}

#[derive(Debug)]
pub struct AutomaticProjectionRuntime {
    carplay: Box<WiredCarkitRuntime>,
    android_auto: Box<AndroidAutoRuntime>,
    wireless_carplay: Option<Box<WirelessCarPlayRuntime>>,
    active: Option<ActiveSession>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveSession {
    protocol: ActiveProtocol,
    wireless: bool,
}

impl ActiveSession {
    const fn for_device(device: &ProjectionDevice) -> Self {
        let protocol = match device {
            ProjectionDevice::CarPlay(_) => ActiveProtocol::CarPlay,
            ProjectionDevice::AndroidAuto(_) | ProjectionDevice::WirelessAndroidAuto(_) => {
                ActiveProtocol::AndroidAuto
            }
            ProjectionDevice::WirelessCarPlay(_) => ActiveProtocol::WirelessCarPlay,
        };
        Self {
            protocol,
            wireless: device.is_wireless(),
        }
    }
}

impl AutomaticProjectionRuntime {
    pub fn from_config(config: &DaemonConfig) -> Result<Self> {
        Self::from_config_with_display(
            config,
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            None,
        )
    }

    pub fn from_config_with_display(
        config: &DaemonConfig,
        #[cfg(all(target_os = "linux", feature = "linux-media"))] display: Option<
            prelude_media::LinuxDisplayHandle,
        >,
    ) -> Result<Self> {
        Self::from_config_with_display_and_ambient(
            config,
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            display,
            AmbientLightState::default(),
        )
    }

    pub fn from_config_with_display_and_ambient(
        config: &DaemonConfig,
        #[cfg(all(target_os = "linux", feature = "linux-media"))] display: Option<
            prelude_media::LinuxDisplayHandle,
        >,
        ambient_light: AmbientLightState,
    ) -> Result<Self> {
        Ok(Self {
            carplay: Box::new(WiredCarkitRuntime::from_config_with_display_and_ambient(
                config,
                #[cfg(all(target_os = "linux", feature = "linux-media"))]
                display.clone(),
                ambient_light.clone(),
            )?),
            android_auto: Box::new(AndroidAutoRuntime::from_config_with_display_and_ambient(
                config,
                #[cfg(all(target_os = "linux", feature = "linux-media"))]
                display,
                ambient_light,
            )?),
            wireless_carplay: None,
            active: None,
        })
    }

    pub fn attach_wireless(&mut self, wireless_carplay: WirelessCarPlayRuntime) {
        self.wireless_carplay = Some(Box::new(wireless_carplay));
    }

    pub(crate) fn attach_hands_free(&mut self, hands_free: HandsFree) {
        self.android_auto.attach_hands_free(hands_free);
    }

    fn runtime(&mut self, protocol: ActiveProtocol) -> Option<&mut dyn ProjectionRuntime> {
        match protocol {
            ActiveProtocol::CarPlay => Some(self.carplay.as_mut()),
            ActiveProtocol::AndroidAuto => Some(self.android_auto.as_mut()),
            ActiveProtocol::WirelessCarPlay => self
                .wireless_carplay
                .as_deref_mut()
                .map(|runtime| runtime as &mut dyn ProjectionRuntime),
        }
    }

    fn active_runtime(&mut self) -> Option<&mut dyn ProjectionRuntime> {
        self.active
            .and_then(|session| self.runtime(session.protocol))
    }

    fn stop_active(&mut self) -> Result<()> {
        let Some(runtime) = self.active_runtime() else {
            return Ok(());
        };
        runtime.stop()?;
        self.active = None;
        Ok(())
    }

    fn hand_off(&mut self, requested: ActiveSession) -> Result<()> {
        if let Some(incoming) = self.runtime(requested.protocol) {
            incoming.begin_handoff();
        }
        let Some(runtime) = self.active_runtime() else {
            return Ok(());
        };
        runtime.stop_for_handoff()?;
        self.active = None;
        Ok(())
    }

    fn idle_probe() -> RuntimeProbe {
        RuntimeProbe {
            state: HealthState::NotReady,
            detail: "automatic projection is waiting for one supported phone".to_owned(),
            transport_state: HealthState::NotReady,
            transport_detail: "no projection transport is active".to_owned(),
            link_state: HealthState::NotReady,
            link_detail: "no projection link is active".to_owned(),
            control_state: HealthState::NotReady,
            control_detail: "no projection control session is active".to_owned(),
            authentication_state: HealthState::NotReady,
            authentication_detail: "no phone authentication session is active".to_owned(),
            session_state: HealthState::NotReady,
            session_detail: "no projection session is active".to_owned(),
            implementation: ImplementationStatus::Unsupported,
            capabilities: ProjectionCapabilities::default(),
        }
    }
}

impl ProjectionRuntime for AutomaticProjectionRuntime {
    fn probe(&mut self) -> RuntimeProbe {
        self.active_runtime()
            .map_or_else(Self::idle_probe, ProjectionRuntime::probe)
    }

    fn set_phone_presence(&mut self, present: bool) {
        if let Some(runtime) = self.active_runtime() {
            runtime.set_phone_presence(present);
        }
    }

    fn start(&mut self, device: &ProjectionDevice) -> Result<()> {
        let requested = ActiveSession::for_device(device);
        match self.active {
            Some(active) if active.wireless && !requested.wireless => self.hand_off(requested)?,
            Some(active) if active != requested => self.stop_active()?,
            _ => {}
        }
        self.runtime(requested.protocol)
            .ok_or(crate::Error::Unsupported(
                "wireless CarPlay is not enabled in this configuration",
            ))?
            .start(device)?;
        self.active = Some(requested);
        Ok(())
    }

    fn audio_status(&mut self) -> AudioRuntimeSnapshot {
        self.active_runtime().map_or_else(
            AudioRuntimeSnapshot::disabled,
            ProjectionRuntime::audio_status,
        )
    }

    fn play_audio_test_tone(&mut self) -> Result<()> {
        self.active_runtime()
            .ok_or(crate::Error::NotReady {
                component: "projection audio",
                state: "no_active_protocol",
            })?
            .play_audio_test_tone()
    }

    fn stop(&mut self) -> Result<()> {
        self.stop_active()
    }

    fn holds_usb_session(&mut self) -> bool {
        self.active.is_some_and(|active| !active.wireless)
            && matches!(
                self.probe().transport_state,
                HealthState::Starting | HealthState::Ready
            )
    }

    fn yields_to(&self, device: &ProjectionDevice) -> bool {
        self.active
            .is_some_and(|active| active.wireless && !device.is_wireless())
    }
}
