use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use prelude_media::LinuxDisplayHandle;
use prelude_wireless::bluez::PairingPasskeys;

use crate::{
    DisplayPlatform, DisplayStatus, load_last_display_platform,
    wireless_runtime::SharedBluetoothControl,
};

const POLL: Duration = Duration::from_millis(250);
const CODE_TIMEOUT: Duration = Duration::from_secs(30);
const PAIRED_HANDOFF: Duration = Duration::from_secs(15);

struct Shown {
    peer: String,
    deadline: Instant,
    paired: bool,
}

pub(crate) fn show_pairing_codes(
    display: &LinuxDisplayHandle,
    passkeys: &PairingPasskeys,
    control: &SharedBluetoothControl,
    state_dir: &Path,
    stop: &AtomicBool,
) {
    let mut shown: Option<Shown> = None;
    while !stop.load(Ordering::Relaxed) {
        if let Some((peer, passkey)) = passkeys.take_unshown() {
            if display.projection_active() {
                tracing::debug!(
                    peer,
                    "a session owns the screen; not showing the pairing code"
                );
            } else if show(display, DisplayStatus::ConfirmPairing(passkey)) {
                tracing::info!(peer, "showing the Bluetooth pairing code");
                shown = Some(Shown {
                    peer,
                    deadline: Instant::now() + CODE_TIMEOUT,
                    paired: false,
                });
            }
        }
        shown = shown.and_then(|current| advance(display, control, state_dir, current));
        thread::sleep(POLL);
    }
}

fn advance(
    display: &LinuxDisplayHandle,
    control: &SharedBluetoothControl,
    state_dir: &Path,
    shown: Shown,
) -> Option<Shown> {
    if display.projection_active() {
        return None;
    }
    if !shown.paired && is_paired(control, &shown.peer) {
        show(display, DisplayStatus::Connecting(None));
        return Some(Shown {
            deadline: Instant::now() + PAIRED_HANDOFF,
            paired: true,
            ..shown
        });
    }
    if Instant::now() < shown.deadline {
        return Some(shown);
    }
    let platform = load_last_display_platform(state_dir).unwrap_or(DisplayPlatform::CarPlay);
    if let Ok(overlay) = crate::status_screen::render(platform, DisplayStatus::WaitingForPhone)
        && !display.projection_active()
        && let Err(error) = display.set_waiting_overlay(overlay)
    {
        tracing::warn!(%error, "restoring the waiting screen after pairing failed");
    }
    None
}

fn is_paired(control: &SharedBluetoothControl, peer: &str) -> bool {
    control
        .read()
        .ok()
        .and_then(|control| control.clone())
        .and_then(|control| control.is_paired(peer))
        .unwrap_or(false)
}

fn show(display: &LinuxDisplayHandle, status: DisplayStatus) -> bool {
    let shown =
        crate::status_screen::render(DisplayPlatform::CarPlay, status).and_then(|overlay| {
            display
                .set_waiting_overlay(overlay)
                .map_err(|error| error.to_string())
        });
    if let Err(error) = &shown {
        tracing::warn!(%error, "showing the Bluetooth pairing screen failed");
    }
    shown.is_ok()
}
