use serde::{Deserialize, Serialize};

/// `webrtcdsp` `noise-suppression-level` nicks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NoiseSuppressionLevel {
    Low,
    #[default]
    Moderate,
    High,
    VeryHigh,
}

impl NoiseSuppressionLevel {
    pub const ALL: [Self; 4] = [Self::Low, Self::Moderate, Self::High, Self::VeryHigh];

    pub const fn nick(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Moderate => "moderate",
            Self::High => "high",
            Self::VeryHigh => "very-high",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Moderate => "Moderate",
            Self::High => "High",
            Self::VeryHigh => "Very high",
        }
    }
}

/// Snapshot read when the microphone `webrtcdsp` element is created.
///
/// Knobs are ignored unless `enabled` is set. That flag is the call-echo gate
/// (`Cancel call echo`, phone audio off, USB DAC), not a separate control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EchoTuning {
    pub enabled: bool,
    pub noise_suppression: bool,
    pub noise_suppression_level: NoiseSuppressionLevel,
    pub high_pass_filter: bool,
    pub gain_control: bool,
}

impl EchoTuning {
    pub const DEFAULT: Self = Self {
        enabled: false,
        noise_suppression: true,
        noise_suppression_level: NoiseSuppressionLevel::Moderate,
        high_pass_filter: true,
        gain_control: false,
    };

    /// Level is meaningful only while echo cancellation and noise suppression are on.
    pub const fn noise_level(self) -> Option<NoiseSuppressionLevel> {
        if self.enabled && self.noise_suppression {
            Some(self.noise_suppression_level)
        } else {
            None
        }
    }
}

impl Default for EchoTuning {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_level_applies_only_when_echo_and_suppression_are_on() {
        assert_eq!(
            NoiseSuppressionLevel::ALL.map(NoiseSuppressionLevel::nick),
            ["low", "moderate", "high", "very-high"]
        );
        assert_eq!(NoiseSuppressionLevel::VeryHigh.label(), "Very high");

        let defaults = EchoTuning::default();
        assert!(!defaults.enabled);
        assert!(defaults.noise_suppression);
        assert!(defaults.high_pass_filter);
        assert!(!defaults.gain_control);
        assert!(defaults.noise_level().is_none());

        let armed = EchoTuning {
            enabled: true,
            ..defaults
        };
        assert_eq!(
            armed.noise_level().map(NoiseSuppressionLevel::nick),
            Some("moderate")
        );

        let quiet = EchoTuning {
            enabled: true,
            noise_suppression: false,
            noise_suppression_level: NoiseSuppressionLevel::High,
            ..defaults
        };
        assert!(quiet.noise_level().is_none());
    }
}
