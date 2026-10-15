use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub const EQUALIZER_BANDS: usize = 10;
pub const EQUALIZER_FREQUENCIES_HZ: [f64; EQUALIZER_BANDS] = [
    31.25, 62.5, 125.0, 250.0, 500.0, 1_000.0, 2_000.0, 4_000.0, 8_000.0, 16_000.0,
];
pub const MIN_BAND_HALF_DB: i8 = -24;
pub const MAX_BAND_HALF_DB: i8 = 24;
pub const MIN_PREAMP_HALF_DB: i8 = -48;
pub const MAX_PREAMP_HALF_DB: i8 = 24;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EqualizerSettings {
    pub enabled: bool,
    pub bands_half_db: [i8; EQUALIZER_BANDS],
    pub preamp_half_db: i8,
}

impl EqualizerSettings {
    pub fn validate(self) -> Result<Self> {
        if !self
            .bands_half_db
            .iter()
            .all(|gain| (MIN_BAND_HALF_DB..=MAX_BAND_HALF_DB).contains(gain))
            || !(MIN_PREAMP_HALF_DB..=MAX_PREAMP_HALF_DB).contains(&self.preamp_half_db)
        {
            return Err(Error::InvalidAudio(
                "equalizer gain is outside its supported range".to_owned(),
            ));
        }
        Ok(self)
    }

    pub fn effective_gains_db(self) -> [f64; EQUALIZER_BANDS + 1] {
        let mut gains = [0.0; EQUALIZER_BANDS + 1];
        if self.enabled {
            for (out, gain) in gains.iter_mut().zip(self.bands_half_db) {
                *out = half_db_to_db(gain);
            }
            gains[EQUALIZER_BANDS] = half_db_to_db(self.preamp_half_db);
        }
        gains
    }
}

pub fn half_db_to_db(gain: i8) -> f64 {
    f64::from(gain) / 2.0
}
pub fn db_to_linear(gain: f64) -> f64 {
    10.0_f64.powf(gain / 20.0)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum EqualizerAvailability {
    #[default]
    Pending,
    Available,
    Unavailable,
}

#[derive(Debug, Default)]
struct State {
    settings: EqualizerSettings,
    availability: EqualizerAvailability,
}

#[derive(Debug, Default, Clone)]
pub struct EqualizerControl(Arc<Mutex<State>>);

impl PartialEq for EqualizerControl {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.settings() == other.settings()
    }
}
impl Eq for EqualizerControl {}

impl EqualizerControl {
    pub fn settings(&self) -> EqualizerSettings {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .settings
    }

    pub fn set(&self, settings: EqualizerSettings) -> Result<()> {
        let settings = settings.validate()?;
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .settings = settings;
        Ok(())
    }

    pub fn availability(&self) -> EqualizerAvailability {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .availability
    }

    #[cfg(all(target_os = "linux", feature = "linux-backend"))]
    pub(crate) fn try_settings(&self) -> Option<EqualizerSettings> {
        self.0.try_lock().ok().map(|state| state.settings)
    }

    #[cfg(all(target_os = "linux", feature = "linux-backend"))]
    pub(crate) fn report_availability(&self, availability: EqualizerAvailability) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .availability = availability;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_and_disabled_curve_are_preserved() {
        for gain in [MIN_BAND_HALF_DB, MAX_BAND_HALF_DB] {
            let settings = EqualizerSettings {
                bands_half_db: [gain; 10],
                preamp_half_db: MIN_PREAMP_HALF_DB,
                ..Default::default()
            };
            assert!(settings.validate().is_ok());
            assert_eq!(settings.effective_gains_db(), [0.0; 11]);
            assert_eq!(
                EqualizerSettings {
                    enabled: true,
                    ..settings
                }
                .effective_gains_db()[0],
                half_db_to_db(gain)
            );
        }
        assert!(
            EqualizerSettings {
                bands_half_db: [25; 10],
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            EqualizerSettings {
                preamp_half_db: -49,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!((db_to_linear(-6.0) - 0.501187).abs() < 0.000001);
    }
    #[test]
    fn latest_state_survives_clones_and_invalid_updates() {
        let original = EqualizerControl::default();
        let session = original.clone();
        let settings = EqualizerSettings {
            enabled: true,
            preamp_half_db: -12,
            ..Default::default()
        };
        for gain in -24..=24 {
            original
                .set(EqualizerSettings {
                    bands_half_db: [gain; 10],
                    ..settings
                })
                .expect("valid");
        }
        assert_eq!(session.settings().bands_half_db, [24; 10]);
        assert!(
            session
                .set(EqualizerSettings {
                    preamp_half_db: 25,
                    ..settings
                })
                .is_err()
        );
        assert_eq!(original.settings().preamp_half_db, -12);
        assert_eq!(original.clone().settings(), session.settings());
    }
}
