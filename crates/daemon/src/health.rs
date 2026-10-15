use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Disabled,
    Starting,
    Ready,
    NotReady,
    Unsupported,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentHealth {
    pub required: bool,
    pub state: HealthState,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverallHealth {
    Disabled,
    Starting,
    Ready,
    NotReady,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HealthRegistry {
    components: BTreeMap<String, ComponentHealth>,
}

impl HealthRegistry {
    pub fn set(
        &mut self,
        component: impl Into<String>,
        required: bool,
        state: HealthState,
        detail: impl Into<String>,
    ) {
        self.components.insert(
            component.into(),
            ComponentHealth {
                required,
                state,
                detail: detail.into(),
            },
        );
    }

    pub fn components(&self) -> &BTreeMap<String, ComponentHealth> {
        &self.components
    }

    pub fn overall(&self) -> OverallHealth {
        let mut required = self
            .components
            .values()
            .filter(|component| component.required);
        if required.clone().next().is_none() {
            return OverallHealth::Disabled;
        }
        if required
            .clone()
            .any(|component| component.state == HealthState::Failed)
        {
            return OverallHealth::Failed;
        }
        if required.clone().any(|component| {
            matches!(
                component.state,
                HealthState::NotReady | HealthState::Unsupported | HealthState::Disabled
            )
        }) {
            return OverallHealth::NotReady;
        }
        if required.any(|component| component.state == HealthState::Starting) {
            return OverallHealth::Starting;
        }
        OverallHealth::Ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_failure_does_not_mask_required_readiness() {
        let mut health = HealthRegistry::default();
        health.set("usb", true, HealthState::Ready, "ok");
        health.set("microphone", false, HealthState::Failed, "missing");
        assert_eq!(health.overall(), OverallHealth::Ready);
    }

    #[test]
    fn unsupported_required_component_is_not_ready() {
        let mut health = HealthRegistry::default();
        health.set("projection", true, HealthState::Unsupported, "no backend");
        assert_eq!(health.overall(), OverallHealth::NotReady);
    }
}
