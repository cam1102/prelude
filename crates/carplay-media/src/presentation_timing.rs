use crate::{Error, Result};

pub const MAXIMUM_AUDIO_OUTPUT_DELAY_MS: u32 = 1_000;
const NANOSECONDS_PER_MILLISECOND: u64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioOutputDelaySource {
    Uncompensated,
    ConfiguredCalibration,
    TransportReported,
    Measured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioOutputDelay {
    source: AudioOutputDelaySource,
    milliseconds: u32,
}

impl AudioOutputDelay {
    pub const fn uncompensated() -> Self {
        Self {
            source: AudioOutputDelaySource::Uncompensated,
            milliseconds: 0,
        }
    }

    pub fn configured_calibration(milliseconds: u32) -> Result<Self> {
        Self::new(AudioOutputDelaySource::ConfiguredCalibration, milliseconds)
    }

    pub fn transport_reported(milliseconds: u32) -> Result<Self> {
        Self::new(AudioOutputDelaySource::TransportReported, milliseconds)
    }

    pub fn measured(milliseconds: u32) -> Result<Self> {
        Self::new(AudioOutputDelaySource::Measured, milliseconds)
    }

    fn new(source: AudioOutputDelaySource, milliseconds: u32) -> Result<Self> {
        if milliseconds > MAXIMUM_AUDIO_OUTPUT_DELAY_MS {
            return Err(Error::InvalidAudio(format!(
                "audio output delay exceeds the {MAXIMUM_AUDIO_OUTPUT_DELAY_MS} ms bound"
            )));
        }
        Ok(Self {
            source,
            milliseconds,
        })
    }

    pub const fn source(self) -> AudioOutputDelaySource {
        self.source
    }

    pub const fn milliseconds(self) -> u32 {
        self.milliseconds
    }

    pub const fn nanoseconds(self) -> u64 {
        self.milliseconds as u64 * NANOSECONDS_PER_MILLISECOND
    }
}

impl Default for AudioOutputDelay {
    fn default() -> Self {
        Self::uncompensated()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(any(test, all(target_os = "linux", feature = "linux-backend")))]
pub(crate) struct VideoPresentationSchedule {
    pub(crate) deadline_ns: u64,
    pub(crate) wait_ns: u64,
    pub(crate) lateness_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(any(test, all(target_os = "linux", feature = "linux-backend")))]
pub(crate) struct VideoPresentationClock {
    output_delay: AudioOutputDelay,
    origin: Option<(u64, u64)>,
    previous_pts_ns: Option<u64>,
    previous_clock_ns: Option<u64>,
}

#[cfg(any(test, all(target_os = "linux", feature = "linux-backend")))]
impl VideoPresentationClock {
    pub(crate) const fn new(output_delay: AudioOutputDelay) -> Self {
        Self {
            output_delay,
            origin: None,
            previous_pts_ns: None,
            previous_clock_ns: None,
        }
    }

    pub(crate) fn schedule(
        &mut self,
        presentation_time_ns: u64,
        clock_time_ns: u64,
    ) -> Result<VideoPresentationSchedule> {
        if self
            .previous_pts_ns
            .is_some_and(|previous| presentation_time_ns < previous)
        {
            return Err(Error::InvalidVideo(
                "decoded video presentation time moved backwards".to_owned(),
            ));
        }
        if self
            .previous_clock_ns
            .is_some_and(|previous| clock_time_ns < previous)
        {
            return Err(Error::Backend(
                "GStreamer system clock moved backwards".to_owned(),
            ));
        }
        if self.output_delay.milliseconds() == 0 {
            self.previous_pts_ns = Some(presentation_time_ns);
            self.previous_clock_ns = Some(clock_time_ns);
            return Ok(VideoPresentationSchedule {
                deadline_ns: clock_time_ns,
                wait_ns: 0,
                lateness_ns: 0,
            });
        }
        let (origin_pts_ns, origin_clock_ns) = *self
            .origin
            .get_or_insert((presentation_time_ns, clock_time_ns));
        let media_offset_ns = presentation_time_ns
            .checked_sub(origin_pts_ns)
            .ok_or_else(|| {
                Error::InvalidVideo("decoded video presentation time moved backwards".to_owned())
            })?;
        let deadline_ns = origin_clock_ns
            .checked_add(self.output_delay.nanoseconds())
            .and_then(|deadline| deadline.checked_add(media_offset_ns))
            .ok_or_else(|| Error::Backend("video presentation deadline overflowed".to_owned()))?;
        self.previous_pts_ns = Some(presentation_time_ns);
        self.previous_clock_ns = Some(clock_time_ns);
        Ok(VideoPresentationSchedule {
            deadline_ns,
            wait_ns: deadline_ns.saturating_sub(clock_time_ns),
            lateness_ns: clock_time_ns.saturating_sub(deadline_ns),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_delay_is_bounded_and_preserves_its_evidence_source() {
        assert_eq!(AudioOutputDelay::uncompensated().milliseconds(), 0);
        assert_eq!(
            AudioOutputDelay::configured_calibration(275)
                .expect("calibration")
                .source(),
            AudioOutputDelaySource::ConfiguredCalibration
        );
        assert_eq!(
            AudioOutputDelay::transport_reported(175)
                .expect("transport report")
                .source(),
            AudioOutputDelaySource::TransportReported
        );
        assert_eq!(
            AudioOutputDelay::measured(180)
                .expect("measurement")
                .source(),
            AudioOutputDelaySource::Measured
        );
        assert!(
            AudioOutputDelay::configured_calibration(MAXIMUM_AUDIO_OUTPUT_DELAY_MS + 1).is_err()
        );
        assert!(AudioOutputDelay::measured(MAXIMUM_AUDIO_OUTPUT_DELAY_MS + 1).is_err());
        assert!(AudioOutputDelay::transport_reported(MAXIMUM_AUDIO_OUTPUT_DELAY_MS + 1).is_err());
    }

    #[test]
    fn presentation_clock_maps_pts_and_calibration_to_one_monotonic_domain() {
        let mut clock = VideoPresentationClock::new(
            AudioOutputDelay::configured_calibration(250).expect("calibration"),
        );
        let first = clock
            .schedule(1_000_000_000, 5_000_000_000)
            .expect("first schedule");
        assert_eq!(first.deadline_ns, 5_250_000_000);
        assert_eq!(first.wait_ns, 250_000_000);
        assert_eq!(first.lateness_ns, 0);

        let second = clock
            .schedule(1_033_000_000, 5_100_000_000)
            .expect("second schedule");
        assert_eq!(second.deadline_ns, 5_283_000_000);
        assert_eq!(second.wait_ns, 183_000_000);
        assert_eq!(second.lateness_ns, 0);

        let late = clock
            .schedule(1_066_000_000, 5_400_000_000)
            .expect("late schedule");
        assert_eq!(late.deadline_ns, 5_316_000_000);
        assert_eq!(late.wait_ns, 0);
        assert_eq!(late.lateness_ns, 84_000_000);
    }

    #[test]
    fn uncompensated_clock_preserves_immediate_usb_presentation() {
        let mut clock = VideoPresentationClock::new(AudioOutputDelay::uncompensated());
        let first = clock.schedule(10, 5_000).expect("first schedule");
        let second = clock.schedule(43, 5_001).expect("second schedule");
        assert_eq!(first.deadline_ns, 5_000);
        assert_eq!(first.wait_ns, 0);
        assert_eq!(second.deadline_ns, 5_001);
        assert_eq!(second.wait_ns, 0);
    }

    #[test]
    fn presentation_clock_rejects_regression_and_overflow() {
        let mut pts_regression = VideoPresentationClock::new(AudioOutputDelay::uncompensated());
        pts_regression.schedule(20, 100).expect("first");
        assert!(pts_regression.schedule(19, 101).is_err());

        let mut clock_regression = VideoPresentationClock::new(AudioOutputDelay::uncompensated());
        clock_regression.schedule(20, 100).expect("first");
        assert!(clock_regression.schedule(21, 99).is_err());

        let mut overflow = VideoPresentationClock::new(
            AudioOutputDelay::configured_calibration(1).expect("calibration"),
        );
        assert!(overflow.schedule(0, u64::MAX).is_err());
    }
}
