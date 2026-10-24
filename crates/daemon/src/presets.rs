use std::sync::{Arc, Mutex};

use platform_superbird::LocalPreset;
use serde::{Deserialize, Serialize};

pub const PRESET_COUNT: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetAction {
    None,
    PreviousTrack,
    PlayPause,
    NextTrack,
    Mute,
    Home,
    Back,
    VoiceAssistant,
    AnswerCall,
    EndCall,
}

impl PresetAction {
    pub const ALL: [Self; 10] = [
        Self::PreviousTrack,
        Self::PlayPause,
        Self::NextTrack,
        Self::Mute,
        Self::Home,
        Self::Back,
        Self::VoiceAssistant,
        Self::AnswerCall,
        Self::EndCall,
        Self::None,
    ];

    pub const DEFAULTS: [Self; PRESET_COUNT] = [
        Self::PreviousTrack,
        Self::PlayPause,
        Self::NextTrack,
        Self::Mute,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::PreviousTrack => "Previous track",
            Self::PlayPause => "Play/Pause",
            Self::NextTrack => "Next track",
            Self::Mute => "Mute",
            Self::Home => "Home",
            Self::Back => "Back",
            Self::VoiceAssistant => "Voice assistant",
            Self::AnswerCall => "Answer call",
            Self::EndCall => "End call",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PresetBindings(Arc<Mutex<[PresetAction; PRESET_COUNT]>>);

impl Default for PresetBindings {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(PresetAction::DEFAULTS)))
    }
}

impl PartialEq for PresetBindings {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.actions() == other.actions()
    }
}

impl Eq for PresetBindings {}

impl PresetBindings {
    pub fn actions(&self) -> [PresetAction; PRESET_COUNT] {
        *self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn set(&self, actions: [PresetAction; PRESET_COUNT]) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = actions;
    }

    pub fn action(&self, preset: LocalPreset) -> PresetAction {
        usize::from(preset.number())
            .checked_sub(1)
            .and_then(|index| self.actions().get(index).copied())
            .unwrap_or(PresetAction::None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_legacy_fixed_layout_and_updates_are_shared() {
        let bindings = PresetBindings::default();
        let shared = bindings.clone();
        assert_eq!(
            bindings.action(LocalPreset::ONE),
            PresetAction::PreviousTrack
        );
        assert_eq!(bindings.action(LocalPreset::FOUR), PresetAction::Mute);
        bindings.set([
            PresetAction::Home,
            PresetAction::VoiceAssistant,
            PresetAction::AnswerCall,
            PresetAction::None,
        ]);
        assert_eq!(shared.action(LocalPreset::ONE), PresetAction::Home);
        assert_eq!(shared.action(LocalPreset::FOUR), PresetAction::None);
    }
}
