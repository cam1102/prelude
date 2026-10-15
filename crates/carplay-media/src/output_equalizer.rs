use super::*;
use crate::{
    EQUALIZER_BANDS, EQUALIZER_FREQUENCIES_HZ, EqualizerAvailability, EqualizerControl,
    db_to_linear,
};

const RAMP_DURATION: Duration = Duration::from_millis(40);
const UPDATE_INTERVAL: Duration = Duration::from_millis(5);

pub(super) struct OutputEqualizer {
    preamp: gst::Element,
    equalizer: gst::Element,
    control: EqualizerControl,
    current: [f64; EQUALIZER_BANDS + 1],
    from: [f64; EQUALIZER_BANDS + 1],
    target: [f64; EQUALIZER_BANDS + 1],
    started: Instant,
    updated: Instant,
}

impl OutputEqualizer {
    pub(super) fn create(control: EqualizerControl) -> Result<Option<Self>> {
        if gst::ElementFactory::find("equalizer-nbands").is_none()
            || gst::ElementFactory::find("volume").is_none()
        {
            control.report_availability(EqualizerAvailability::Unavailable);
            return Ok(None);
        }
        let result = Self::build(control.clone());
        match result {
            Ok(dsp) => {
                control.report_availability(EqualizerAvailability::Available);
                Ok(Some(dsp))
            }
            Err(error) => {
                control.report_availability(EqualizerAvailability::Unavailable);
                Err(error)
            }
        }
    }

    fn build(control: EqualizerControl) -> Result<Self> {
        let preamp = make("volume", "usb-output-preamp")?;
        let equalizer = make("equalizer-nbands", "usb-output-equalizer")?;
        set_checked(&equalizer, "num-bands", EQUALIZER_BANDS as u32)?;
        let proxy = equalizer
            .clone()
            .dynamic_cast::<gst::ChildProxy>()
            .map_err(|_| Error::Backend("equalizer has no band controls".to_owned()))?;
        for (index, frequency) in EQUALIZER_FREQUENCIES_HZ.into_iter().enumerate() {
            let band = proxy
                .child_by_index(index as u32)
                .ok_or_else(|| Error::Backend("equalizer band is missing".to_owned()))?;
            set_enum_checked(&band, "type", "peak")?;
            set_checked(&band, "freq", frequency)?;
            set_checked(
                &band,
                "bandwidth",
                frequency * (2.0_f64.sqrt() - 1.0 / 2.0_f64.sqrt()),
            )?;
        }
        let current = control.settings().effective_gains_db();
        let now = Instant::now();
        let dsp = Self {
            preamp,
            equalizer,
            control,
            current,
            from: current,
            target: current,
            started: now,
            updated: now,
        };
        dsp.apply(current)?;
        Ok(dsp)
    }

    pub(super) fn elements(&self) -> [&gst::Element; 2] {
        [&self.preamp, &self.equalizer]
    }

    fn apply(&self, gains: [f64; EQUALIZER_BANDS + 1]) -> Result<()> {
        set_checked(&self.preamp, "volume", db_to_linear(gains[EQUALIZER_BANDS]))?;
        let proxy = self
            .equalizer
            .clone()
            .dynamic_cast::<gst::ChildProxy>()
            .map_err(|_| Error::Backend("equalizer has no band controls".to_owned()))?;
        for (index, gain) in gains.into_iter().take(EQUALIZER_BANDS).enumerate() {
            let band = proxy
                .child_by_index(index as u32)
                .ok_or_else(|| Error::Backend("equalizer band is missing".to_owned()))?;
            set_checked(&band, "gain", gain)?;
        }
        Ok(())
    }

    pub(super) fn tick(&mut self) -> Result<()> {
        let now = Instant::now();
        if now.duration_since(self.updated) < UPDATE_INTERVAL {
            return Ok(());
        }
        self.updated = now;
        if let Some(settings) = self.control.try_settings() {
            let target = settings.effective_gains_db();
            if target != self.target {
                self.from = self.current;
                self.target = target;
                self.started = now;
            }
        }
        if self.current == self.target {
            return Ok(());
        }
        let fraction =
            (now.duration_since(self.started).as_secs_f64() / RAMP_DURATION.as_secs_f64()).min(1.0);
        if fraction >= 1.0 {
            self.current = self.target;
        } else {
            for index in 0..self.current.len() {
                self.current[index] =
                    self.from[index] + (self.target[index] - self.from[index]) * fraction;
            }
        }
        if let Err(error) = self.apply(self.current) {
            self.control
                .report_availability(EqualizerAvailability::Unavailable);
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EqualizerSettings;

    fn require_dsp() -> bool {
        if gst::init().is_ok()
            && ["appsrc", "appsink", "volume", "equalizer-nbands"]
                .into_iter()
                .all(|factory| gst::ElementFactory::find(factory).is_some())
        {
            return true;
        }
        assert!(
            std::env::var_os("PRELUDE_REQUIRE_HOST_AUDIO_TEST").is_none(),
            "required equalizer DSP elements unavailable"
        );
        eprintln!("SKIP equalizer integration: GStreamer DSP elements unavailable");
        false
    }

    fn processed_rms(settings: EqualizerSettings, frequency: f64) -> f64 {
        let control = EqualizerControl::default();
        control.set(settings).expect("settings");
        let dsp = OutputEqualizer::create(control)
            .expect("DSP")
            .expect("DSP plugins");
        let pipeline = gst::Pipeline::new();
        let source = gst_app::AppSrc::builder()
            .caps(
                &gst::Caps::builder("audio/x-raw")
                    .field("format", "F32LE")
                    .field("layout", "interleaved")
                    .field("rate", 48_000_i32)
                    .field("channels", 2_i32)
                    .build(),
            )
            .format(gst::Format::Time)
            .build();
        let sink = gst_app::AppSink::builder().sync(false).build();
        let elements = [
            source.upcast_ref::<gst::Element>(),
            &dsp.preamp,
            &dsp.equalizer,
            sink.upcast_ref::<gst::Element>(),
        ];
        pipeline.add_many(elements).expect("add");
        gst::Element::link_many(elements).expect("link");
        pipeline.set_state(gst::State::Playing).expect("playing");
        let mut samples = Vec::with_capacity(48_000 * 8);
        for index in 0..48_000 {
            let sample = (0.05
                * (std::f64::consts::TAU * frequency * f64::from(index) / 48_000.0).sin())
                as f32;
            samples.extend_from_slice(&sample.to_le_bytes());
            samples.extend_from_slice(&sample.to_le_bytes());
        }
        let mut buffer = gst::Buffer::from_mut_slice(samples);
        buffer
            .get_mut()
            .expect("writable")
            .set_pts(gst::ClockTime::ZERO);
        source.push_buffer(buffer).expect("push");
        source.end_of_stream().expect("EOS");
        let mut output = Vec::new();
        while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(2)) {
            let buffer = sample.buffer().expect("output buffer");
            let map = buffer.map_readable().expect("map");
            output.extend(
                map.as_slice()
                    .chunks_exact(4)
                    .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
            );
        }
        pipeline.set_state(gst::State::Null).expect("stop");
        assert_eq!(output.len(), 96_000, "all output samples");
        let steady = &output[48_000..];
        (steady
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum::<f64>()
            / steady.len() as f64)
            .sqrt()
    }

    #[test]
    fn actual_dsp_flat_center_gain_preamp_and_bypass() {
        if !require_dsp() {
            return;
        }
        let flat = processed_rms(
            EqualizerSettings {
                enabled: true,
                ..Default::default()
            },
            1_000.0,
        );
        assert!((flat - 0.05 / 2.0_f64.sqrt()).abs() < 0.00001);
        let mut boosted = EqualizerSettings {
            enabled: true,
            ..Default::default()
        };
        boosted.bands_half_db[5] = 12;
        let amplified = processed_rms(boosted, 1_000.0);
        assert!((amplified / flat - db_to_linear(6.0)).abs() < 0.02);
        boosted.preamp_half_db = -12;
        assert!((processed_rms(boosted, 1_000.0) / flat - 1.0).abs() < 0.02);
        boosted.enabled = false;
        assert!((processed_rms(boosted, 1_000.0) - flat).abs() < 0.00001);
        let preamp = EqualizerSettings {
            enabled: true,
            preamp_half_db: -24,
            ..Default::default()
        };
        assert!((processed_rms(preamp, 1_000.0) / flat - db_to_linear(-12.0)).abs() < 0.00001);
    }

    #[test]
    fn float_mixer_preserves_headroom_before_preamp_attenuation() {
        if !require_dsp() {
            return;
        }
        let plan = AudioMixerPlan::new(&[(AudioStreamId::Main, AudioFormatId::new(0x8000))])
            .expect("plan");
        let control = EqualizerControl::default();
        control
            .set(EqualizerSettings {
                enabled: true,
                preamp_half_db: -12,
                ..Default::default()
            })
            .expect("settings");
        let dsp = OutputEqualizer::create(control)
            .expect("DSP")
            .expect("plugins");
        let pipeline = gst::Pipeline::new();
        let mix_caps = floating_mix_caps(&plan).expect("mix caps");
        let mixer = make("audiomixer", "headroom-mixer").expect("mixer");
        let filter = make("capsfilter", "headroom-caps").expect("filter");
        set_checked(&filter, "caps", mix_caps.clone()).expect("float caps");
        let sources: Vec<_> = (0..2)
            .map(|_| {
                gst_app::AppSrc::builder()
                    .caps(&mix_caps)
                    .format(gst::Format::Time)
                    .build()
            })
            .collect();
        let sink = gst_app::AppSink::builder().sync(false).build();
        let output = [
            &mixer,
            &filter,
            &dsp.preamp,
            &dsp.equalizer,
            sink.upcast_ref::<gst::Element>(),
        ];
        pipeline.add_many(output).expect("add output");
        gst::Element::link_many(output).expect("link output");
        for source in &sources {
            pipeline.add(source).expect("source");
            source.link(&mixer).expect("mixer input");
        }
        pipeline.set_state(gst::State::Playing).expect("play");
        for source in &sources {
            let mut data = Vec::with_capacity(48_000 * 8);
            for index in 0..48_000 {
                let sample = (0.75
                    * (std::f64::consts::TAU * 1_000.0 * f64::from(index) / 48_000.0).sin())
                    as f32;
                data.extend_from_slice(&sample.to_le_bytes());
                data.extend_from_slice(&sample.to_le_bytes());
            }
            let mut buffer = gst::Buffer::from_mut_slice(data);
            let writable = buffer.get_mut().expect("writable");
            writable.set_pts(gst::ClockTime::ZERO);
            writable.set_duration(gst::ClockTime::SECOND);
            source.push_buffer(buffer).expect("push");
            source.end_of_stream().expect("EOS");
        }
        let mut output_samples = Vec::new();
        while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(2)) {
            let map = sample
                .buffer()
                .expect("buffer")
                .map_readable()
                .expect("map");
            output_samples.extend(
                map.as_slice()
                    .chunks_exact(4)
                    .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
            );
        }
        pipeline.set_state(gst::State::Null).expect("stop");
        assert_eq!(output_samples.len(), 96_000);
        let rms = (output_samples
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum::<f64>()
            / output_samples.len() as f64)
            .sqrt();
        let expected = 1.5 * db_to_linear(-6.0) / 2.0_f64.sqrt();
        assert!(
            (rms - expected).abs() < 0.0001,
            "float sum must survive until preamp: {rms} vs {expected}"
        );
    }

    #[test]
    fn missing_dsp_plugins_report_unavailable_without_breaking_usb_audio() {
        const CHILD: &str = "PRELUDE_TEST_MISSING_EQUALIZER_CHILD";
        if std::env::var_os(CHILD).is_some() {
            gst::init().expect("GStreamer");
            assert!(gst::ElementFactory::find("equalizer-nbands").is_none());
            let control = EqualizerControl::default();
            let settings = EqualizerSettings {
                enabled: true,
                bands_half_db: [12; 10],
                preamp_half_db: -12,
            };
            control.set(settings).expect("save curve");
            let mut config = LinuxAudioBackendConfig::with_negotiated_route(
                AudioOutputRoute::TestAlsa("null".to_owned()),
                vec![AudioStreamConfig::new(
                    AudioStreamId::Main,
                    AudioFormatId::new(0x8000),
                )],
            );
            config.equalizer = control.clone();
            let backend = LinuxAudioBackend::start(config)
                .expect("ordinary audio must start without optional EQ");
            assert_eq!(control.availability(), EqualizerAvailability::Unavailable);
            assert_eq!(control.settings(), settings);
            assert_eq!(
                backend
                    .telemetry()
                    .expect("telemetry")
                    .pipeline_start_events,
                1
            );
            backend.shutdown().expect("shutdown");
            return;
        }
        if !require_dsp() {
            return;
        }

        let factory = gst::ElementFactory::find("equalizer-nbands").expect("factory");
        let filename = factory
            .plugin()
            .expect("plugin")
            .filename()
            .expect("filename");
        let directory = tempfile::TempDir::new().expect("temporary plugins");
        for entry in fs::read_dir(filename.parent().expect("plugin directory")).expect("plugins") {
            let entry = entry.expect("entry");
            if entry.file_name() != filename.file_name().expect("equalizer filename") {
                std::os::unix::fs::symlink(entry.path(), directory.path().join(entry.file_name()))
                    .expect("isolated plugin link");
            }
        }
        let status = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "linux_backend::output_equalizer::tests::missing_dsp_plugins_report_unavailable_without_breaking_usb_audio", "--nocapture"])
            .env(CHILD, "1")
            .env("GST_PLUGIN_SYSTEM_PATH_1_0", directory.path())
            .env("GST_PLUGIN_PATH_1_0", "")
            .env("GST_REGISTRY_1_0", directory.path().join("registry.bin"))
            .status().expect("missing-plugin subprocess");
        assert!(status.success(), "optional plugin fallback process failed");
    }

    #[test]
    fn updates_ramp_on_same_elements_and_disabled_preserves_curve() {
        if !require_dsp() {
            return;
        }
        let control = EqualizerControl::default();
        let mut dsp = OutputEqualizer::create(control.clone())
            .expect("DSP")
            .expect("plugins");
        let element = dsp.equalizer.clone();
        let settings = EqualizerSettings {
            enabled: true,
            bands_half_db: [12; 10],
            preamp_half_db: -24,
        };

        for gain in -24..=24 {
            control
                .set(EqualizerSettings {
                    bands_half_db: [gain; 10],
                    ..settings
                })
                .expect("settings");
        }
        control.set(settings).expect("latest");
        thread::sleep(UPDATE_INTERVAL);
        dsp.tick().expect("begin ramp");
        assert_ne!(dsp.current, settings.effective_gains_db());
        thread::sleep(RAMP_DURATION / 2);
        dsp.tick().expect("mid-ramp");
        assert!(dsp.current[0] > 0.0 && dsp.current[0] < 6.0);
        thread::sleep(RAMP_DURATION);
        dsp.tick().expect("settle");
        assert_eq!(dsp.current, settings.effective_gains_db());
        control
            .set(EqualizerSettings {
                enabled: false,
                ..settings
            })
            .expect("disable");
        thread::sleep(UPDATE_INTERVAL);
        dsp.tick().expect("begin bypass");
        thread::sleep(RAMP_DURATION);
        dsp.tick().expect("bypass");
        assert_eq!(dsp.current, [0.0; 11]);
        assert_eq!(control.settings().bands_half_db, [12; 10]);
        assert_eq!(dsp.equalizer, element);

        control.set(settings).expect("enable again");
        thread::sleep(UPDATE_INTERVAL);
        dsp.tick().expect("start");
        thread::sleep(RAMP_DURATION / 3);
        dsp.tick().expect("fractional state");
        control
            .set(EqualizerSettings {
                bands_half_db: [-17; 10],
                ..settings
            })
            .expect("retarget");
        thread::sleep(UPDATE_INTERVAL);
        dsp.tick().expect("retarget ramp");
        thread::sleep(RAMP_DURATION);
        dsp.tick().expect("settle exactly");
        assert_eq!(dsp.current, dsp.target);
        assert_eq!(dsp.current[0], -8.5);
        dsp.tick().expect("idle tick");
        assert_eq!(dsp.current, dsp.target);
    }
}
