use serde::{Deserialize, Serialize};

use crate::{Error, ProjectionAuthorization, ProjectionState, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayGeometry {
    pub width: u16,
    pub height: u16,
    pub maximum_contacts: u8,
}

impl DisplayGeometry {
    pub fn validate(self) -> Result<Self> {
        if self.width == 0 || self.height == 0 || self.maximum_contacts == 0 {
            return Err(Error::InvalidInput(
                "display dimensions and contact count must be non-zero".to_owned(),
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TouchContact {
    pub identifier: u8,
    pub phase: TouchPhase,
    pub x: u16,
    pub y: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderDirection {
    Clockwise,
    CounterClockwise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Home,
    Back,
    Select,
    Siri,
    PlayPause,
    Next,
    Previous,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputEvent {
    Touch(Vec<TouchContact>),
    Encoder {
        direction: EncoderDirection,
        detents: u8,
    },
    Button {
        button: Button,
        pressed: bool,
    },
}

pub trait InputSink: std::fmt::Debug + Send {
    fn send(&mut self, event: &InputEvent) -> std::result::Result<(), String>;
}

#[derive(Debug)]
pub struct InputRouter<S> {
    geometry: DisplayGeometry,
    sink: S,
}

impl<S: InputSink> InputRouter<S> {
    pub fn new(geometry: DisplayGeometry, sink: S) -> Result<Self> {
        Ok(Self {
            geometry: geometry.validate()?,
            sink,
        })
    }

    pub fn route(
        &mut self,
        event: &InputEvent,
        state: ProjectionState,
        authorization: &ProjectionAuthorization,
    ) -> Result<()> {
        if state != ProjectionState::Active {
            return Err(Error::NotReady {
                component: "projection input",
                state: state.as_str(),
            });
        }
        if !authorization.capabilities().input {
            return Err(Error::Unsupported("projection input was not negotiated"));
        }
        self.validate_event(event)?;
        self.sink.send(event).map_err(Error::Sink)
    }

    fn validate_event(&self, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::Touch(contacts) => {
                if contacts.is_empty()
                    || contacts.len() > usize::from(self.geometry.maximum_contacts)
                {
                    return Err(Error::InvalidInput(
                        "touch contact count is outside display capabilities".to_owned(),
                    ));
                }
                let mut seen = [false; 256];
                for contact in contacts {
                    if contact.x >= self.geometry.width || contact.y >= self.geometry.height {
                        return Err(Error::InvalidInput(
                            "touch coordinate is outside the display".to_owned(),
                        ));
                    }
                    let index = usize::from(contact.identifier);
                    if seen[index] {
                        return Err(Error::InvalidInput(
                            "duplicate touch contact identifier".to_owned(),
                        ));
                    }
                    seen[index] = true;
                }
            }
            InputEvent::Encoder { detents: 0, .. } => {
                return Err(Error::InvalidInput(
                    "encoder event has zero detents".to_owned(),
                ));
            }
            InputEvent::Encoder { .. } | InputEvent::Button { .. } => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use prelude_net::{PairVerification, SecurityContext};

    use super::*;
    use crate::{ImplementationStatus, ProjectionCapabilities, ProjectionGate, ProjectionPolicy};

    #[derive(Debug, Default)]
    struct RecordingSink(Vec<InputEvent>);

    impl InputSink for RecordingSink {
        fn send(&mut self, event: &InputEvent) -> std::result::Result<(), String> {
            self.0.push(event.clone());
            Ok(())
        }
    }

    fn authorization() -> ProjectionAuthorization {
        let mut security = SecurityContext::default();
        security
            .record_pair_verification(PairVerification::new("phone", [2; 32]).expect("pairing"))
            .expect("pairing");
        security.record_mfi_proof().expect("proof");
        security.record_peer_acceptance().expect("acceptance");
        ProjectionGate
            .authorize(
                &security,
                ImplementationStatus::ExperimentalPrivateProtocol,
                ProjectionCapabilities {
                    control: true,
                    input: true,
                    ..ProjectionCapabilities::default()
                },
                ProjectionPolicy {
                    allow_experimental_private_protocol: true,
                    ..ProjectionPolicy::default()
                },
            )
            .expect("authorization")
    }

    #[test]
    fn out_of_bounds_touch_is_rejected() {
        let mut router = InputRouter::new(
            DisplayGeometry {
                width: 800,
                height: 480,
                maximum_contacts: 5,
            },
            RecordingSink::default(),
        )
        .expect("router");
        let result = router.route(
            &InputEvent::Touch(vec![TouchContact {
                identifier: 1,
                phase: TouchPhase::Down,
                x: 800,
                y: 10,
            }]),
            ProjectionState::Active,
            &authorization(),
        );
        assert!(matches!(result, Err(Error::InvalidInput(_))));
    }

    #[test]
    fn inactive_projection_rejects_input() {
        let mut router = InputRouter::new(
            DisplayGeometry {
                width: 800,
                height: 480,
                maximum_contacts: 5,
            },
            RecordingSink::default(),
        )
        .expect("router");
        assert!(matches!(
            router.route(
                &InputEvent::Button {
                    button: Button::Siri,
                    pressed: true,
                },
                ProjectionState::Authorized,
                &authorization(),
            ),
            Err(Error::NotReady { .. })
        ));
    }
}
