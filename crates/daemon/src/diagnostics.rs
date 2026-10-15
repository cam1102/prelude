use std::{collections::VecDeque, time::SystemTime};

use serde::{Deserialize, Serialize};

use crate::{HealthRegistry, OverallHealth, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticEvent {
    pub timestamp_millis: u128,
    pub component: String,
    pub message: String,
}

impl DiagnosticEvent {
    pub fn now(component: impl Into<String>, message: impl Into<String>) -> Self {
        let timestamp_millis = SystemTime::UNIX_EPOCH
            .elapsed()
            .map_or(0, |elapsed| elapsed.as_millis());
        Self {
            timestamp_millis,
            component: component.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticSnapshot {
    pub overall: OverallHealth,
    pub health: HealthRegistry,
    pub events: Vec<DiagnosticEvent>,
}

#[derive(Debug)]
pub struct Diagnostics {
    maximum_events: usize,
    events: VecDeque<DiagnosticEvent>,
}

impl Diagnostics {
    pub fn new(maximum_events: usize) -> Self {
        Self {
            maximum_events,
            events: VecDeque::with_capacity(maximum_events),
        }
    }

    pub fn record(&mut self, event: DiagnosticEvent) {
        if self.maximum_events == 0 {
            return;
        }
        while self.events.len() >= self.maximum_events {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    pub fn snapshot(&self, health: &HealthRegistry) -> DiagnosticSnapshot {
        DiagnosticSnapshot {
            overall: health.overall(),
            health: health.clone(),
            events: self.events.iter().cloned().collect(),
        }
    }

    pub fn snapshot_json(&self, health: &HealthRegistry) -> Result<String> {
        Ok(serde_json::to_string_pretty(&self.snapshot(health))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_ring_discards_oldest_event() {
        let mut diagnostics = Diagnostics::new(2);
        diagnostics.record(DiagnosticEvent::now("a", "one"));
        diagnostics.record(DiagnosticEvent::now("b", "two"));
        diagnostics.record(DiagnosticEvent::now("c", "three"));
        let snapshot = diagnostics.snapshot(&HealthRegistry::default());
        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(snapshot.events[0].component, "b");
    }
}
