//! Far-end reference shared by the playback probe and the microphone DSP.

use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

use gstreamer as gst;

use crate::{EchoTuning, Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EchoReference {
    pub(crate) probe_name: String,
    pub(crate) base_time: gst::ClockTime,
}

static ACTIVE: Mutex<Option<EchoReference>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static TUNING: Mutex<EchoTuning> = Mutex::new(EchoTuning::DEFAULT);

fn tuning_slot() -> std::sync::MutexGuard<'static, EchoTuning> {
    TUNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn set_tuning(tuning: EchoTuning) {
    *tuning_slot() = tuning;
}

pub(crate) fn tuning() -> EchoTuning {
    *tuning_slot()
}

pub(crate) fn enabled() -> bool {
    tuning().enabled
}

pub(crate) fn next_probe_name() -> String {
    format!(
        "prelude-echo-probe-{}",
        GENERATION.fetch_add(1, Ordering::Relaxed)
    )
}

fn lock() -> Result<std::sync::MutexGuard<'static, Option<EchoReference>>> {
    ACTIVE
        .lock()
        .map_err(|_| Error::Backend("echo reference lock is poisoned".to_owned()))
}

pub(crate) fn publish(reference: EchoReference) -> Result<()> {
    *lock()? = Some(reference);
    Ok(())
}

pub(crate) fn retract(name: &str) -> Result<()> {
    let mut active = lock()?;
    if active
        .as_ref()
        .is_some_and(|reference| reference.probe_name == name)
    {
        *active = None;
    }
    Ok(())
}

pub(crate) fn current() -> Result<Option<EchoReference>> {
    Ok(lock()?.clone())
}

pub(crate) fn available() -> bool {
    gst::ElementFactory::find("webrtcdsp").is_some()
        && gst::ElementFactory::find("webrtcechoprobe").is_some()
}

pub(crate) fn current_for_capture() -> Result<Option<EchoReference>> {
    if !enabled() {
        return Ok(None);
    }
    Ok(current()?.filter(|_| available()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_retract_generation_and_missing_plugin() {
        let saved = current().expect("read echo reference");
        let first = next_probe_name();
        let second = next_probe_name();
        assert_ne!(first, second);

        publish(EchoReference {
            probe_name: first.clone(),
            base_time: gst::ClockTime::from_nseconds(12),
        })
        .expect("publish");
        let active = current().expect("read published").expect("published");
        assert_eq!(active.probe_name, first);
        assert_eq!(active.base_time, gst::ClockTime::from_nseconds(12));
        retract("other-probe").expect("ignore a different probe");
        assert_eq!(
            current().expect("read kept").expect("kept").probe_name,
            first
        );
        retract(&first).expect("retract");
        assert_ne!(
            current()
                .expect("read retracted")
                .as_ref()
                .map(|reference| reference.probe_name.as_str()),
            Some(first.as_str())
        );
        if current().expect("read empty").is_none() {
            assert!(current_for_capture().expect("no reference").is_none());
        }

        publish(EchoReference {
            probe_name: second.clone(),
            base_time: gst::ClockTime::from_nseconds(1),
        })
        .expect("publish while plugin may be missing");
        set_tuning(EchoTuning {
            enabled: false,
            ..EchoTuning::DEFAULT
        });
        assert!(current_for_capture().expect("setting off").is_none());
        set_tuning(EchoTuning {
            enabled: true,
            ..EchoTuning::DEFAULT
        });
        let _ = gst::init();
        let selected = current_for_capture().expect("capture reference");
        if available() {
            assert_eq!(selected.expect("plugin present").probe_name, second);
        } else {
            assert!(selected.is_none());
        }
        set_tuning(EchoTuning::DEFAULT);
        retract(&second).expect("cleanup");
        if let Some(saved) = saved {
            publish(saved).expect("restore echo reference");
        }
    }
}
