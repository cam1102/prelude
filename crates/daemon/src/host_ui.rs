use std::{
    fs,
    os::unix::fs::FileTypeExt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use fontdue::{Font, FontSettings};
use platform_superbird::{
    AmbientLightControl, KnobInputButton, LinuxInputBackend, LinuxInputConfig, PlatformInputEvent,
    PlatformInputEventKind, RotaryDirection, TouchInputPhase,
};
use prelude_media::LinuxDisplayHandle;
use prelude_wireless::access_point::{Band, ChannelWidth};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::{KnownPhone, RuntimeSettings, WirelessPhoneDirectory};

mod equalizer_ui;
use equalizer_ui::EqualizerUi;

const DISPLAY_WIDTH: usize = 800;
const DISPLAY_HEIGHT: usize = 480;
const MAXIMUM_FONT_BYTES: u64 = 8 * 1024 * 1024;
const SETTINGS_POLL_INTERVAL: Duration = Duration::from_millis(20);
const INPUT_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const PHONES_REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const ROW_X: i32 = 32;
const ROW_WIDTH: i32 = 736;
const ROW_INSET: i32 = 24;
const ROW_GAP: i32 = 12;
const CONTENT_X: i32 = ROW_X + ROW_INSET;
const CONTENT_RIGHT: i32 = ROW_X + ROW_WIDTH - ROW_INSET;
const TITLE_Y: i32 = 44;
const TITLE_SIZE: f32 = 40.0;
const LABEL_SIZE: f32 = 32.0;
const VALUE_SIZE: f32 = 28.0;
const CAPTION_SIZE: f32 = 16.0;
const ROW_TEXT_CENTER: i32 = TOGGLE_ROW_HEIGHT / 2;
const SWITCH_WIDTH: i32 = 80;
const SWITCH_HEIGHT: i32 = 44;
const SWITCH_X: i32 = CONTENT_RIGHT - SWITCH_WIDTH;
const CONTROL_GAP: i32 = 24;
const TOGGLE_ROW_HEIGHT: i32 = 72;
const DPI_ROW_HEIGHT: i32 = 120;
const RESTORE_ROW_HEIGHT: i32 = TOGGLE_ROW_HEIGHT;
const PHONES_ROW_HEIGHT: i32 = TOGGLE_ROW_HEIGHT;
const FIRST_ROW_Y: i32 = 80;
const VIEWPORT_TOP: i32 = FIRST_ROW_Y;
const DRAG_THRESHOLD: i32 = 12;
const PHONES_ROW: usize = 0;
const PHONES_ROW_Y: i32 = FIRST_ROW_Y;
const AUTO_BRIGHTNESS_ROW: usize = 1;
const AUTO_BRIGHTNESS_ROW_Y: i32 = PHONES_ROW_Y + PHONES_ROW_HEIGHT + ROW_GAP;
const AUTO_NIGHT_MODE_ROW: usize = 2;
const AUTO_NIGHT_MODE_ROW_Y: i32 = AUTO_BRIGHTNESS_ROW_Y + TOGGLE_ROW_HEIGHT + ROW_GAP;
const PHONE_AUDIO_ROW: usize = 3;
const PHONE_AUDIO_ROW_Y: i32 = AUTO_NIGHT_MODE_ROW_Y + TOGGLE_ROW_HEIGHT + ROW_GAP;
const EQUALIZER_ROW: usize = 4;
const EQUALIZER_ROW_Y: i32 = PHONE_AUDIO_ROW_Y + TOGGLE_ROW_HEIGHT + ROW_GAP;
const DPI_ROW: usize = 5;
const DPI_ROW_Y: i32 = EQUALIZER_ROW_Y + TOGGLE_ROW_HEIGHT + ROW_GAP;
const RESTORE_DPI_ROW: usize = 6;
const RESTORE_DPI_ROW_Y: i32 = DPI_ROW_Y + DPI_ROW_HEIGHT + ROW_GAP;
const ADVANCED_ROW: usize = 7;
const ADVANCED_ROW_Y: i32 = RESTORE_DPI_ROW_Y + RESTORE_ROW_HEIGHT + ROW_GAP;
const ADVANCED_ROW_HEIGHT: i32 = TOGGLE_ROW_HEIGHT;
const SLIDER_X: i32 = ROW_X + ROW_INSET;
const SLIDER_WIDTH: i32 = ROW_WIDTH - ROW_INSET * 2;
const LIST_ROW_HEIGHT: i32 = TOGGLE_ROW_HEIGHT;
const CONNECT_ROW: usize = 0;
const FORGET_ROW: usize = 1;
const PHONE_ACTION_ROWS: usize = 2;
const WIFI_BAND_ROW: usize = 0;
const WIFI_CHANNEL_ROW: usize = 1;
const WIFI_WIDTH_ROW: usize = 2;
const WIFI_ROWS: usize = 3;
const BANDS: [Band; 2] = [Band::Ghz2_4, Band::Ghz5];
const MIN_DPI: u32 = prelude_android_auto_protocol::MIN_VIDEO_DENSITY_DPI;
const MAX_DPI: u32 = prelude_android_auto_protocol::MAX_VIDEO_DENSITY_DPI;
const DPI_STEP: u32 = 10;
const GREEN: [u8; 3] = [30, 215, 96];
const RED: [u8; 3] = [254, 59, 48];
const WHITE: [u8; 3] = [255, 255, 255];
const SECONDARY: [u8; 3] = [178, 178, 178];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsCommitted {
    pub settings: RuntimeSettings,
    pub restart_required: bool,
    pub radio_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostUiEvent {
    SettingsCommitted(SettingsCommitted),
    ConnectPhone(String),
    DisconnectPhone(String),
    ForgetPhone(String),
}

#[derive(Debug)]
pub struct HostUi {
    events: Receiver<HostUiEvent>,
    stop: Arc<AtomicBool>,
    wake: Arc<Notify>,
    worker: Option<JoinHandle<()>>,
}

impl HostUi {
    pub fn start(
        display: LinuxDisplayHandle,
        state_dir: PathBuf,
        settings: RuntimeSettings,
        equalizer_control: prelude_media::EqualizerControl,
        ambient_light: Option<AmbientLightControl>,
        phones: Option<WirelessPhoneDirectory>,
    ) -> Result<Self, String> {
        let (events_tx, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Notify::new());
        let worker_stop = Arc::clone(&stop);
        let worker_wake = Arc::clone(&wake);
        let worker = thread::Builder::new()
            .name("prelude-host-ui".to_owned())
            .spawn(move || {
                let ui = SettingsUi::new(
                    display,
                    state_dir,
                    settings,
                    equalizer_control,
                    ambient_light,
                    phones,
                    events_tx,
                    worker_wake,
                );
                run_host_ui(ui, &worker_stop);
            })
            .map_err(|error| format!("starting the host settings UI failed: {error}"))?;
        Ok(Self {
            events,
            stop,
            wake,
            worker: Some(worker),
        })
    }

    pub fn take_events(&self) -> Vec<HostUiEvent> {
        self.events.try_iter().collect()
    }

    pub fn wake(&self) -> Arc<Notify> {
        Arc::clone(&self.wake)
    }
}

impl Drop for HostUi {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_host_ui(mut ui: SettingsUi, stop: &AtomicBool) {
    let mut input = None;
    while !stop.load(Ordering::Acquire) {
        if input.is_none() {
            match LinuxInputBackend::discover(LinuxInputConfig::default()) {
                Ok(backend) => {
                    info!("host settings UI acquired Superbird input");
                    input = Some(backend);
                }
                Err(error) => {
                    debug!(%error, "host settings UI input is not ready");
                    wait_for_stop(stop, INPUT_RETRY_INTERVAL);
                    continue;
                }
            }
        }

        let Some(backend) = input.as_mut() else {
            continue;
        };
        match backend.poll() {
            Ok(batch) => {
                for event in batch.events {
                    ui.handle(event);
                }
                if batch.terminal.is_some() {
                    ui.close_without_commit();
                    input = None;
                }
            }
            Err(error) => {
                warn!(%error, "host settings UI input stopped; retrying");
                input = None;
                wait_for_stop(stop, INPUT_RETRY_INTERVAL);
            }
        }
        ui.tick();
    }
    ui.close_without_commit();
}

fn wait_for_stop(stop: &AtomicBool, duration: Duration) {
    let start = std::time::Instant::now();
    while !stop.load(Ordering::Acquire) && start.elapsed() < duration {
        thread::sleep(SETTINGS_POLL_INTERVAL.min(duration.saturating_sub(start.elapsed())));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Settings,
    Phones,
    Phone,
    Advanced,
    Equalizer,
}

struct SettingsUi {
    equalizer_control: prelude_media::EqualizerControl,
    equalizer_ui: EqualizerUi,
    display: LinuxDisplayHandle,
    state_dir: PathBuf,
    settings: RuntimeSettings,
    staged_dpi: u32,
    staged_dpi_customized: bool,
    staged_phone_audio: bool,
    staged_radio: Option<(u8, ChannelWidth)>,
    page: Page,
    selected: usize,
    dpi_editing: bool,
    touch_target: Option<usize>,
    dpi_dragging: bool,
    menu_open: bool,
    error_message: Option<&'static str>,
    ambient_light: Option<AmbientLightControl>,
    directory: Option<WirelessPhoneDirectory>,
    phones: Vec<KnownPhone>,
    active_phone: Option<String>,
    phone_focus: usize,
    opened_phone: Option<String>,
    phone_action: usize,
    wifi_focus: usize,
    wifi_editing: bool,
    list_touch: Option<usize>,
    title_touched: bool,
    confirming_forget: bool,
    pending_connect: Option<String>,
    phones_refreshed: Instant,
    scroll: i32,
    touch_start: Option<(i32, i32, i32)>,
    touch_scrolling: bool,
    events: Sender<HostUiEvent>,
    wake: Arc<Notify>,
    fonts: FontSet,
}

impl SettingsUi {
    #[allow(clippy::too_many_arguments)]
    fn new(
        display: LinuxDisplayHandle,
        state_dir: PathBuf,
        settings: RuntimeSettings,
        equalizer_control: prelude_media::EqualizerControl,
        ambient_light: Option<AmbientLightControl>,
        directory: Option<WirelessPhoneDirectory>,
        events: Sender<HostUiEvent>,
        wake: Arc<Notify>,
    ) -> Self {
        if let Some(control) = ambient_light.as_ref() {
            control.set_automatic_brightness(settings.auto_brightness);
            control.set_automatic_night_mode(settings.auto_night_mode);
        }
        Self {
            equalizer_control,
            equalizer_ui: EqualizerUi::new(settings.equalizer),
            display,
            state_dir,
            settings,
            staged_dpi: settings.reported_dpi,
            staged_dpi_customized: settings.dpi_customized,
            staged_phone_audio: settings.phone_audio,
            staged_radio: settings.wifi_radio(),
            page: Page::Settings,
            selected: 0,
            dpi_editing: false,
            touch_target: None,
            dpi_dragging: false,
            menu_open: false,
            error_message: None,
            ambient_light,
            directory,
            phones: Vec::new(),
            active_phone: None,
            phone_focus: 0,
            opened_phone: None,
            phone_action: CONNECT_ROW,
            wifi_focus: WIFI_BAND_ROW,
            wifi_editing: false,
            list_touch: None,
            title_touched: false,
            confirming_forget: false,
            pending_connect: None,
            phones_refreshed: Instant::now(),
            scroll: 0,
            touch_start: None,
            touch_scrolling: false,
            events,
            wake,
            fonts: FontSet::load(),
        }
    }

    fn handle(&mut self, event: PlatformInputEvent) {
        match event.kind {
            PlatformInputEventKind::SettingsButton => {
                if self.menu_open {
                    self.close();
                } else {
                    self.open();
                }
            }
            PlatformInputEventKind::KnobButton { button, pressed }
                if self.menu_open && !pressed =>
            {
                match self.page {
                    Page::Equalizer => self.handle_equalizer_button(button),
                    Page::Settings => self.handle_knob_release(button),
                    Page::Phones | Page::Phone | Page::Advanced => {
                        self.handle_list_knob_release(button);
                    }
                }
            }
            PlatformInputEventKind::Rotary { direction, steps } if self.menu_open => {
                match self.page {
                    Page::Equalizer => {
                        self.equalizer_ui.rotary(direction, steps);
                        self.apply_equalizer();
                    }
                    Page::Settings => self.handle_rotary(direction, steps),
                    Page::Phones | Page::Phone | Page::Advanced => {
                        self.handle_list_rotary(direction, steps);
                    }
                }
            }
            PlatformInputEventKind::Touch {
                contact_id: 0,
                phase,
                x,
                y,
            } if self.menu_open => match self.page {
                Page::Equalizer => self.handle_equalizer_touch(phase, x, y),
                Page::Settings => self.handle_touch(phase, x, y),
                Page::Phones | Page::Phone | Page::Advanced => self.handle_list_touch(phase, x, y),
            },
            PlatformInputEventKind::Reset { .. } if self.menu_open => {
                self.equalizer_ui.cancel_interaction();
                self.save_equalizer();
                self.touch_target = None;
                self.dpi_dragging = false;
                self.dpi_editing = false;
                self.wifi_editing = false;
                self.list_touch = None;
                self.title_touched = false;
                self.touch_start = None;
                self.touch_scrolling = false;
            }
            _ => {}
        }
    }

    fn tick(&mut self) {
        if self.menu_open && self.phones_refreshed.elapsed() >= PHONES_REFRESH_INTERVAL {
            let phones = self.phones.clone();
            let active = self.active_phone.clone();
            self.refresh_phones();
            if self.page == Page::Equalizer || phones != self.phones || active != self.active_phone
            {
                self.redraw();
            }
        }
    }

    fn refresh_phones(&mut self) {
        self.phones_refreshed = Instant::now();
        let Some(directory) = self.directory.as_ref() else {
            return;
        };
        self.phones = directory.phones();
        self.active_phone = directory.active();
        if self.pending_connect.is_some() && self.pending_connect == self.active_phone {
            self.pending_connect = None;
        }
        self.phone_focus = self.phone_focus.min(self.phones.len().saturating_sub(1));
        if self.page == Page::Phone && self.opened().is_none() {
            self.show_phones(self.phone_focus);
        }
        self.scroll = self.scroll.min(self.maximum_scroll());
    }

    fn opened(&self) -> Option<&KnownPhone> {
        let address = self.opened_phone.as_deref()?;
        self.phones.iter().find(|phone| phone.address == address)
    }

    fn list_len(&self) -> usize {
        match self.page {
            Page::Equalizer => 0,
            Page::Settings => SETTINGS_ROWS.len(),
            Page::Phones => self.phones.len(),
            Page::Phone => PHONE_ACTION_ROWS,
            Page::Advanced if self.wifi_available() => WIFI_ROWS,
            Page::Advanced => 0,
        }
    }

    fn wifi_available(&self) -> bool {
        self.directory.is_some() && self.staged_radio.is_some()
    }

    fn list_focus(&self) -> usize {
        match self.page {
            Page::Equalizer => 0,
            Page::Settings => self.selected,
            Page::Phones => self.phone_focus,
            Page::Phone => self.phone_action,
            Page::Advanced => self.wifi_focus,
        }
    }

    fn row_stops(&self) -> Vec<i32> {
        match self.page {
            Page::Equalizer => Vec::new(),
            Page::Settings => SETTINGS_ROWS
                .iter()
                .map(|&(top, _)| top - FIRST_ROW_Y)
                .collect(),
            Page::Phones | Page::Phone | Page::Advanced => (0..self.list_len())
                .map(|index| list_row_y(index) - FIRST_ROW_Y)
                .collect(),
        }
    }

    fn maximum_scroll(&self) -> i32 {
        let content_bottom = match self.page {
            Page::Equalizer => DISPLAY_HEIGHT as i32,
            Page::Settings => ADVANCED_ROW_Y + ADVANCED_ROW_HEIGHT + ROW_GAP,
            Page::Phones | Page::Phone | Page::Advanced => list_row_y(self.list_len()),
        };
        stop_at_or_after(&self.row_stops(), content_bottom - DISPLAY_HEIGHT as i32)
    }

    fn scroll_into_view(&mut self, top: i32, height: i32) {
        let hidden_below = top + height + ROW_GAP - DISPLAY_HEIGHT as i32;
        let needed = stop_at_or_after(&self.row_stops(), hidden_below);
        self.scroll = self
            .scroll
            .max(needed)
            .min(top - FIRST_ROW_Y)
            .clamp(0, self.maximum_scroll());
    }

    fn content_y(&self, y: i32) -> Option<i32> {
        (y >= VIEWPORT_TOP).then_some(y + self.scroll)
    }

    fn begin_touch(&mut self, x: i32, y: i32) {
        self.touch_start = Some((x, y, self.scroll));
        self.touch_scrolling = false;
    }

    fn drag_scroll(&mut self, x: i32, y: i32) -> bool {
        let Some((start_x, start_y, start_scroll)) = self.touch_start else {
            return false;
        };
        let (dx, dy) = (x - start_x, y - start_y);
        if !self.touch_scrolling
            && dy.abs() >= DRAG_THRESHOLD
            && dy.abs() >= dx.abs()
            && self.maximum_scroll() > 0
        {
            self.touch_scrolling = true;
        }
        if self.touch_scrolling {
            self.scroll = (start_scroll - dy).clamp(0, self.maximum_scroll());
            self.redraw();
        }
        self.touch_scrolling
    }

    fn end_touch(&mut self) -> bool {
        self.touch_start = None;
        let scrolled = std::mem::take(&mut self.touch_scrolling);
        if scrolled {
            self.scroll = nearest_stop(&self.row_stops(), self.scroll).min(self.maximum_scroll());
            self.redraw();
        }
        scrolled
    }

    fn open(&mut self) {
        if let Err(error) = self.display.set_host_ui_active(true) {
            warn!(%error, "could not mark the host settings UI active");
            return;
        }
        self.menu_open = true;
        self.page = Page::Settings;
        self.selected = 0;
        self.scroll = 0;
        self.staged_dpi = self.settings.reported_dpi;
        self.staged_dpi_customized = self.settings.dpi_customized;
        self.staged_phone_audio = self.settings.phone_audio;
        self.staged_radio = self.settings.wifi_radio();
        self.dpi_editing = false;
        self.wifi_editing = false;
        self.error_message = None;
        self.refresh_phones();
        self.wake.notify_one();
        if let Err(error) = self.display.set_overlay(self.render()) {
            warn!(%error, "could not render the host settings UI");
            let _ = self.display.set_host_ui_active(false);
            self.menu_open = false;
            self.wake.notify_one();
        }
    }

    fn close(&mut self) {
        let mut next = self.settings;
        next.equalizer = self.equalizer_ui.settings;
        next.reported_dpi = self.staged_dpi;
        next.dpi_customized = self.staged_dpi_customized;
        next.phone_audio = self.staged_phone_audio;
        if let Some((channel, width)) = self.staged_radio {
            next.wifi_channel = Some(channel);
            next.wifi_channel_width_mhz = Some(width.mhz());
        }
        if let Err(error) = next.persist(&self.state_dir) {
            warn!(%error, "could not persist host settings");
            self.error_message = Some("Unable to save settings");
            self.redraw();
            return;
        }
        let restart_required = next.reported_dpi != self.settings.reported_dpi
            || next.dpi_customized != self.settings.dpi_customized
            || next.phone_audio != self.settings.phone_audio;
        let radio_changed = next.wifi_radio() != self.settings.wifi_radio();
        self.settings = next;
        self.menu_open = false;
        self.leave_subpage();
        self.touch_target = None;
        self.dpi_dragging = false;
        self.dpi_editing = false;
        if let Err(error) = self.display.clear_overlay() {
            warn!(%error, "could not clear the host settings UI");
        }
        if let Err(error) = self.display.set_host_ui_active(false) {
            warn!(%error, "could not mark the host settings UI inactive");
        }
        if restart_required || radio_changed {
            self.send(HostUiEvent::SettingsCommitted(SettingsCommitted {
                settings: next,
                restart_required,
                radio_changed,
            }));
        }
        self.wake.notify_one();
    }

    fn close_without_commit(&mut self) {
        if self.menu_open {
            let _ = self.save_equalizer();
            if let Err(error) = self.display.clear_overlay() {
                warn!(%error, "could not clear the host settings UI during shutdown");
            }
            if let Err(error) = self.display.set_host_ui_active(false) {
                warn!(%error, "could not release the host settings UI during shutdown");
            }
            self.menu_open = false;
            self.dpi_editing = false;
            self.leave_subpage();
            self.wake.notify_one();
        }
    }

    fn send(&self, event: HostUiEvent) {
        if let Err(error) = self.events.send(event) {
            warn!(%error, "could not notify the daemon about a settings change");
        }
        self.wake.notify_one();
    }

    fn handle_knob_release(&mut self, button: KnobInputButton) {
        match button {
            KnobInputButton::Back => self.close(),
            KnobInputButton::Select => match self.selected {
                AUTO_BRIGHTNESS_ROW => self.toggle_auto_brightness(),
                AUTO_NIGHT_MODE_ROW => self.toggle_auto_night_mode(),
                PHONE_AUDIO_ROW => self.toggle_phone_audio(),
                EQUALIZER_ROW => self.enter_equalizer_page(),
                DPI_ROW => {
                    self.dpi_editing = !self.dpi_editing;
                    self.redraw();
                }
                RESTORE_DPI_ROW => self.restore_original_dpi(),
                PHONES_ROW => self.enter_phones_page(),
                ADVANCED_ROW => self.enter_advanced_page(),
                _ => unreachable!("settings selection is bounded"),
            },
        }
    }

    fn handle_rotary(&mut self, direction: RotaryDirection, steps: u8) {
        if self.selected == DPI_ROW && self.dpi_editing {
            let delta = u32::from(steps).saturating_mul(DPI_STEP);
            self.staged_dpi = match direction {
                RotaryDirection::Clockwise => self.staged_dpi.saturating_add(delta),
                RotaryDirection::CounterClockwise => self.staged_dpi.saturating_sub(delta),
            }
            .clamp(MIN_DPI, MAX_DPI);
            self.staged_dpi_customized = true;
        } else {
            self.selected = selection_after_rotary(self.selected, direction, steps, ADVANCED_ROW);
            self.dpi_editing = false;
            let (top, height) = SETTINGS_ROWS[self.selected];
            self.scroll_into_view(top, height);
        }
        self.redraw();
    }

    fn handle_touch(&mut self, phase: TouchInputPhase, x: u16, y: u16) {
        let x = i32::from(x);
        let y = i32::from(y);
        match phase {
            TouchInputPhase::Down => {
                self.begin_touch(x, y);
                self.touch_target = self.content_y(y).and_then(settings_row_at);
                self.dpi_dragging = false;
                if let Some(selection) = self.touch_target {
                    self.selected = selection;
                }
                self.dpi_editing = false;
                self.redraw();
            }
            TouchInputPhase::Move if self.dpi_dragging => self.stage_dpi_from_x(x),
            TouchInputPhase::Move => {
                if self.drag_scroll(x, y) {
                    self.touch_target = None;
                } else if self.touch_target == Some(DPI_ROW)
                    && self
                        .touch_start
                        .is_some_and(|(start_x, _, _)| (x - start_x).abs() >= DRAG_THRESHOLD)
                {
                    self.dpi_dragging = true;
                    self.stage_dpi_from_x(x);
                }
            }
            TouchInputPhase::Up => {
                let scrolled = self.end_touch();
                let released_on_target = !scrolled
                    && self.touch_target.is_some()
                    && self.touch_target == self.content_y(y).and_then(settings_row_at);
                match self.touch_target {
                    Some(DPI_ROW) if self.dpi_dragging => self.redraw(),
                    Some(DPI_ROW) if released_on_target => self.stage_dpi_from_x(x),
                    Some(AUTO_BRIGHTNESS_ROW) if released_on_target => {
                        self.toggle_auto_brightness();
                    }
                    Some(AUTO_NIGHT_MODE_ROW) if released_on_target => {
                        self.toggle_auto_night_mode();
                    }
                    Some(PHONE_AUDIO_ROW) if released_on_target => self.toggle_phone_audio(),
                    Some(EQUALIZER_ROW) if released_on_target => self.enter_equalizer_page(),
                    Some(RESTORE_DPI_ROW) if released_on_target => self.restore_original_dpi(),
                    Some(PHONES_ROW) if released_on_target => self.enter_phones_page(),
                    Some(ADVANCED_ROW) if released_on_target => self.enter_advanced_page(),
                    _ => {}
                }
                self.touch_target = None;
                self.dpi_dragging = false;
            }
        }
    }

    fn stage_dpi_from_x(&mut self, x: i32) {
        self.staged_dpi = dpi_from_x(x);
        self.staged_dpi_customized = true;
        self.redraw();
    }

    fn enter_phones_page(&mut self) {
        self.refresh_phones();
        self.show_phones(0);
    }

    fn show_phones(&mut self, focus: usize) {
        self.page = Page::Phones;
        self.opened_phone = None;
        self.confirming_forget = false;
        self.phone_focus = focus.min(self.phones.len().saturating_sub(1));
        self.scroll = 0;
        self.scroll_into_view(list_row_y(self.phone_focus), LIST_ROW_HEIGHT);
        self.redraw();
    }

    fn open_phone(&mut self, index: usize) {
        let Some(phone) = self.phones.get(index) else {
            return;
        };
        self.opened_phone = Some(phone.address.clone());
        self.phone_focus = index;
        self.page = Page::Phone;
        self.phone_action = CONNECT_ROW;
        self.confirming_forget = false;
        self.scroll = 0;
        self.redraw();
    }

    fn enter_equalizer_page(&mut self) {
        self.equalizer_ui.cancel_interaction();
        self.page = Page::Equalizer;
        self.scroll = 0;
        self.redraw();
    }

    fn apply_equalizer(&mut self) {
        if let Err(error) = self.equalizer_control.set(self.equalizer_ui.settings) {
            warn!(%error, "could not apply equalizer");
            self.error_message = Some("Unable to apply equalizer");
        } else {
            self.error_message = None;
        }
        self.redraw();
    }

    fn save_equalizer(&mut self) -> bool {
        if !self.equalizer_ui.dirty {
            return true;
        }
        match self.equalizer_ui.persist(&self.state_dir, self.settings) {
            Ok(next) => self.settings = next,
            Err(error) => {
                warn!(%error, "could not save equalizer");
                self.error_message = Some("Unable to save settings");
                self.redraw();
                return false;
            }
        }
        self.error_message = None;
        true
    }

    fn handle_equalizer_button(&mut self, button: KnobInputButton) {
        let leave = self.equalizer_ui.button(button);
        self.apply_equalizer();
        if self.save_equalizer() && leave {
            self.go_back();
        }
    }

    fn handle_equalizer_touch(&mut self, phase: TouchInputPhase, x: u16, y: u16) {
        let leave = self.equalizer_ui.touch(phase, x, y);
        self.apply_equalizer();
        if phase == TouchInputPhase::Up && self.save_equalizer() && leave {
            self.go_back();
        }
    }

    fn enter_advanced_page(&mut self) {
        self.page = Page::Advanced;
        self.wifi_focus = WIFI_BAND_ROW;
        self.wifi_editing = false;
        self.scroll = 0;
        self.redraw();
    }

    fn leave_subpage(&mut self) {
        self.equalizer_ui.cancel_interaction();
        self.page = Page::Settings;
        self.opened_phone = None;
        let (top, height) = SETTINGS_ROWS[self.selected];
        self.scroll_into_view(top, height);
        self.list_touch = None;
        self.title_touched = false;
        self.confirming_forget = false;
        self.wifi_editing = false;
    }

    fn go_back(&mut self) {
        match self.page {
            Page::Phone => self.show_phones(self.phone_focus),
            _ => {
                self.leave_subpage();
                self.redraw();
            }
        }
    }

    fn handle_list_knob_release(&mut self, button: KnobInputButton) {
        match button {
            KnobInputButton::Back if self.wifi_editing => {
                self.wifi_editing = false;
                self.redraw();
            }
            KnobInputButton::Back => self.go_back(),
            KnobInputButton::Select if self.page == Page::Advanced => {
                self.wifi_editing = !self.wifi_editing && self.list_len() > 0;
                self.redraw();
            }
            KnobInputButton::Select => self.choose_list_row(self.list_focus()),
        }
    }

    fn handle_list_rotary(&mut self, direction: RotaryDirection, steps: u8) {
        if self.wifi_editing {
            let delta = match direction {
                RotaryDirection::Clockwise => isize::from(steps),
                RotaryDirection::CounterClockwise => -isize::from(steps),
            };
            self.step_staged_radio(delta, false);
            return;
        }
        let Some(last) = self.list_len().checked_sub(1) else {
            return;
        };
        let focus = selection_after_rotary(self.list_focus(), direction, steps, last);
        self.focus_list_row(focus);
        self.scroll_into_view(list_row_y(focus), LIST_ROW_HEIGHT);
        self.redraw();
    }

    fn focus_list_row(&mut self, row: usize) {
        match self.page {
            Page::Equalizer => {}
            Page::Settings => self.selected = row,
            Page::Phones => self.phone_focus = row,
            Page::Phone => {
                if row != self.phone_action {
                    self.confirming_forget = false;
                }
                self.phone_action = row;
            }
            Page::Advanced => {
                if row != self.wifi_focus {
                    self.wifi_editing = false;
                }
                self.wifi_focus = row;
            }
        }
    }

    fn handle_list_touch(&mut self, phase: TouchInputPhase, x: u16, y: u16) {
        let x = i32::from(x);
        let y = i32::from(y);
        match phase {
            TouchInputPhase::Down => {
                self.begin_touch(x, y);
                self.title_touched = y < VIEWPORT_TOP;
                self.list_touch = self
                    .content_y(y)
                    .and_then(|y| list_row_at(y, self.list_len()));
                if let Some(row) = self.list_touch {
                    self.focus_list_row(row);
                    self.redraw();
                }
            }
            TouchInputPhase::Move => {
                if self.drag_scroll(x, y) {
                    self.list_touch = None;
                }
            }
            TouchInputPhase::Up => {
                let scrolled = self.end_touch();
                let released_row = self
                    .content_y(y)
                    .and_then(|y| list_row_at(y, self.list_len()));
                if self.title_touched && y < VIEWPORT_TOP {
                    self.go_back();
                } else if let Some(row) = self.list_touch.filter(|_| !scrolled)
                    && released_row == Some(row)
                {
                    self.choose_list_row(row);
                }
                self.list_touch = None;
                self.title_touched = false;
            }
        }
    }

    fn choose_list_row(&mut self, row: usize) {
        match self.page {
            Page::Settings | Page::Equalizer => {}
            Page::Phones => self.open_phone(row),
            Page::Phone if row == CONNECT_ROW => self.connect_opened_phone(),
            Page::Phone => self.forget_opened_phone(),
            Page::Advanced => self.step_staged_radio(1, true),
        }
    }

    fn step_staged_radio(&mut self, delta: isize, wrap: bool) {
        if let Some(radio) = self.staged_radio {
            self.staged_radio = Some(step_radio(radio, self.wifi_focus, delta, wrap));
            self.error_message = None;
            self.redraw();
        }
    }

    fn connect_opened_phone(&mut self) {
        let Some(phone) = self.opened().cloned() else {
            return;
        };
        self.confirming_forget = false;
        if self.is_active(&phone) {
            info!(peer = phone.address, "disconnecting a phone from settings");
            self.active_phone = None;
            self.send(HostUiEvent::DisconnectPhone(phone.address));
        } else {
            info!(
                peer = phone.address,
                "connecting to a phone chosen in settings"
            );
            self.pending_connect = Some(phone.address.clone());
            self.send(HostUiEvent::ConnectPhone(phone.address));
        }
        self.redraw();
    }

    fn forget_opened_phone(&mut self) {
        let Some(phone) = self.opened().cloned() else {
            return;
        };
        if !self.confirming_forget {
            self.confirming_forget = true;
            self.redraw();
            return;
        }
        info!(peer = phone.address, "forgetting a phone from settings");
        if self.pending_connect.as_deref() == Some(phone.address.as_str()) {
            self.pending_connect = None;
        }
        self.phones.retain(|known| known.address != phone.address);
        self.send(HostUiEvent::ForgetPhone(phone.address));
        self.show_phones(self.phone_focus);
    }

    fn restore_original_dpi(&mut self) {
        self.staged_dpi = self.settings.original_dpi;
        self.staged_dpi_customized = false;
        self.dpi_editing = false;
        self.error_message = None;
        self.redraw();
    }

    fn toggle_auto_brightness(&mut self) {
        let mut next = self.settings;
        next.auto_brightness = !next.auto_brightness;
        self.apply_immediate_setting(next, true);
    }

    fn toggle_auto_night_mode(&mut self) {
        let mut next = self.settings;
        next.auto_night_mode = !next.auto_night_mode;
        self.apply_immediate_setting(next, false);
    }

    fn toggle_phone_audio(&mut self) {
        self.staged_phone_audio = !self.staged_phone_audio;
        self.error_message = None;
        self.redraw();
    }

    fn apply_immediate_setting(&mut self, next: RuntimeSettings, brightness: bool) {
        if let Err(error) = next.persist(&self.state_dir) {
            warn!(%error, "could not persist host settings");
            self.error_message = Some("Unable to save settings");
            self.redraw();
            return;
        }
        if let Some(control) = self.ambient_light.as_ref() {
            if brightness {
                control.set_automatic_brightness(next.auto_brightness);
            } else {
                control.set_automatic_night_mode(next.auto_night_mode);
            }
        }
        self.settings = next;
        self.error_message = None;
        self.redraw();
    }

    fn redraw(&self) {
        if self.menu_open
            && let Err(error) = self.display.set_overlay(self.render())
        {
            warn!(%error, "could not update the host settings UI");
        }
    }

    fn render(&self) -> Vec<u8> {
        let mut canvas = Canvas::new();
        canvas.scroll(self.scroll, VIEWPORT_TOP);
        match self.page {
            Page::Equalizer => self.equalizer_ui.render(
                &mut canvas,
                &self.fonts,
                self.staged_phone_audio,
                self.equalizer_control.availability(),
                self.error_message,
            ),
            Page::Settings => self.render_settings_rows(&mut canvas),
            Page::Phones => self.render_phones(&mut canvas),
            Page::Phone => self.render_phone_actions(&mut canvas),
            Page::Advanced => self.render_advanced(&mut canvas),
        }
        canvas.scroll(0, 0);
        self.render_scrollbar(&mut canvas);
        let title = match (self.page, self.opened()) {
            (Page::Phone, Some(phone)) => self.fonts.fit(
                &format!("‹ {}", phone.name),
                TITLE_SIZE,
                true,
                CONTENT_RIGHT - CONTENT_X,
            ),
            (Page::Equalizer, _) => "‹ Equalizer".to_owned(),
            (Page::Settings, _) => "Settings".to_owned(),
            (Page::Advanced, _) => "‹ Advanced".to_owned(),
            _ => "‹ Phones".to_owned(),
        };
        canvas.text(
            &self.fonts,
            &title,
            CONTENT_X,
            TITLE_Y,
            TITLE_SIZE,
            WHITE,
            true,
        );
        if self.page == Page::Equalizer {
            self.equalizer_ui.render_header(&mut canvas, &self.fonts);
        } else if let Some(message) = self.error_message {
            canvas.text_right(
                &self.fonts,
                message,
                CONTENT_RIGHT,
                TITLE_Y,
                CAPTION_SIZE,
                RED,
                false,
            );
        }
        canvas.to_nv12()
    }

    fn render_scrollbar(&self, canvas: &mut Canvas) {
        let maximum = self.maximum_scroll();
        if maximum == 0 {
            return;
        }
        let track = DISPLAY_HEIGHT as i32 - VIEWPORT_TOP - ROW_GAP * 2;
        let thumb = track * track / (track + maximum);
        let thumb_y = VIEWPORT_TOP + ROW_GAP + (track - thumb) * self.scroll / maximum;
        canvas.rounded_rect(ROW_X + ROW_WIDTH + 12, thumb_y, 4, thumb, 2, WHITE, 90);
    }

    fn render_settings_rows(&self, canvas: &mut Canvas) {
        self.render_phones_row(canvas);
        self.render_toggle_row(
            canvas,
            AUTO_BRIGHTNESS_ROW_Y,
            "Auto brightness",
            self.settings.auto_brightness,
            AUTO_BRIGHTNESS_ROW,
        );
        self.render_toggle_row(
            canvas,
            AUTO_NIGHT_MODE_ROW_Y,
            "Auto night mode",
            self.settings.auto_night_mode,
            AUTO_NIGHT_MODE_ROW,
        );
        self.render_toggle_row(
            canvas,
            PHONE_AUDIO_ROW_Y,
            "Phone audio",
            self.staged_phone_audio,
            PHONE_AUDIO_ROW,
        );
        self.render_row(
            canvas,
            EQUALIZER_ROW_Y,
            TOGGLE_ROW_HEIGHT,
            "Equalizer",
            EQUALIZER_ROW,
        );
        canvas.text_right(
            &self.fonts,
            if self.equalizer_ui.settings.enabled {
                "On  ›"
            } else {
                "Off  ›"
            },
            CONTENT_RIGHT,
            EQUALIZER_ROW_Y + ROW_TEXT_CENTER,
            VALUE_SIZE,
            SECONDARY,
            false,
        );
        self.render_dpi_row(canvas);
        self.render_reset_row(canvas);
        self.render_row(
            canvas,
            ADVANCED_ROW_Y,
            ADVANCED_ROW_HEIGHT,
            "Advanced",
            ADVANCED_ROW,
        );
        canvas.text_right(
            &self.fonts,
            "›",
            CONTENT_RIGHT,
            ADVANCED_ROW_Y + ROW_TEXT_CENTER,
            VALUE_SIZE,
            if self.selected == ADVANCED_ROW {
                GREEN
            } else {
                SECONDARY
            },
            true,
        );
    }

    fn render_row(&self, canvas: &mut Canvas, y: i32, height: i32, label: &str, index: usize) {
        canvas.rounded_rect(
            ROW_X,
            y,
            ROW_WIDTH,
            height,
            14,
            WHITE,
            if self.selected == index { 28 } else { 18 },
        );
        canvas.text(
            &self.fonts,
            label,
            CONTENT_X,
            y + ROW_TEXT_CENTER,
            LABEL_SIZE,
            WHITE,
            true,
        );
    }

    fn render_toggle_row(
        &self,
        canvas: &mut Canvas,
        y: i32,
        label: &str,
        enabled: bool,
        index: usize,
    ) {
        self.render_row(canvas, y, TOGGLE_ROW_HEIGHT, label, index);
        canvas.text_right(
            &self.fonts,
            if enabled { "On" } else { "Off" },
            SWITCH_X - CONTROL_GAP,
            y + ROW_TEXT_CENTER,
            VALUE_SIZE,
            if enabled { GREEN } else { SECONDARY },
            true,
        );
        canvas.rounded_rect(
            SWITCH_X,
            y + (TOGGLE_ROW_HEIGHT - SWITCH_HEIGHT) / 2,
            SWITCH_WIDTH,
            SWITCH_HEIGHT,
            SWITCH_HEIGHT / 2,
            WHITE,
            if enabled { 82 } else { 28 },
        );
        let thumb_x = if enabled {
            CONTENT_RIGHT - SWITCH_HEIGHT / 2
        } else {
            SWITCH_X + SWITCH_HEIGHT / 2
        };
        canvas.circle(thumb_x, y + ROW_TEXT_CENTER, 18, WHITE, 255);
    }

    fn render_dpi_row(&self, canvas: &mut Canvas) {
        let y = DPI_ROW_Y;
        self.render_row(canvas, y, DPI_ROW_HEIGHT, "Reported DPI", DPI_ROW);
        canvas.text_right(
            &self.fonts,
            &self.staged_dpi.to_string(),
            CONTENT_RIGHT,
            y + ROW_TEXT_CENTER,
            VALUE_SIZE,
            if self.selected == DPI_ROW {
                GREEN
            } else {
                SECONDARY
            },
            true,
        );
        let track_y = y + 76;
        canvas.rounded_rect(SLIDER_X, track_y, SLIDER_WIDTH, 8, 4, WHITE, 45);
        let filled_width = ((self.staged_dpi - MIN_DPI) as i64 * i64::from(SLIDER_WIDTH)
            / i64::from(MAX_DPI - MIN_DPI)) as i32;
        canvas.rounded_rect(SLIDER_X, track_y, filled_width.max(8), 8, 4, WHITE, 220);
        canvas.circle(
            SLIDER_X + filled_width,
            track_y + 4,
            12,
            if self.selected == DPI_ROW {
                GREEN
            } else {
                WHITE
            },
            255,
        );
        canvas.text(
            &self.fonts,
            &MIN_DPI.to_string(),
            CONTENT_X,
            y + 104,
            CAPTION_SIZE,
            SECONDARY,
            false,
        );
        canvas.text_right(
            &self.fonts,
            &MAX_DPI.to_string(),
            CONTENT_RIGHT,
            y + 104,
            CAPTION_SIZE,
            SECONDARY,
            false,
        );
    }

    fn render_reset_row(&self, canvas: &mut Canvas) {
        let y = RESTORE_DPI_ROW_Y;
        self.render_row(
            canvas,
            y,
            RESTORE_ROW_HEIGHT,
            "Restore original DPI",
            RESTORE_DPI_ROW,
        );
        canvas.text_right(
            &self.fonts,
            &self.settings.original_dpi.to_string(),
            CONTENT_RIGHT,
            y + ROW_TEXT_CENTER,
            VALUE_SIZE,
            if self.selected == RESTORE_DPI_ROW {
                GREEN
            } else {
                SECONDARY
            },
            true,
        );
    }

    fn render_phones_row(&self, canvas: &mut Canvas) {
        let y = PHONES_ROW_Y;
        self.render_row(canvas, y, PHONES_ROW_HEIGHT, "Phones", PHONES_ROW);
        let summary = match self.phones.len() {
            _ if self.directory.is_none() => "Off".to_owned(),
            0 => "None ›".to_owned(),
            count => format!("{count} saved ›"),
        };
        canvas.text_right(
            &self.fonts,
            &summary,
            CONTENT_RIGHT,
            y + ROW_TEXT_CENTER,
            VALUE_SIZE,
            if self.selected == PHONES_ROW {
                GREEN
            } else {
                SECONDARY
            },
            true,
        );
    }

    fn render_message(&self, canvas: &mut Canvas, headline: &str, caption: &str) {
        canvas.text(
            &self.fonts,
            headline,
            CONTENT_X,
            FIRST_ROW_Y + ROW_TEXT_CENTER,
            LABEL_SIZE,
            WHITE,
            true,
        );
        canvas.text(
            &self.fonts,
            caption,
            CONTENT_X,
            FIRST_ROW_Y + ROW_TEXT_CENTER + 40,
            CAPTION_SIZE,
            SECONDARY,
            false,
        );
    }

    fn render_advanced(&self, canvas: &mut Canvas) {
        let Some((channel, width)) = self.staged_radio.filter(|_| self.wifi_available()) else {
            self.render_message(
                canvas,
                "Wireless is off",
                "Enable wireless projection to change Wi-Fi settings",
            );
            return;
        };
        for (row, label, value) in [
            (
                WIFI_BAND_ROW,
                "Wi-Fi band",
                band_name(Band::of_channel(channel)).to_owned(),
            ),
            (WIFI_CHANNEL_ROW, "Channel", channel.to_string()),
            (
                WIFI_WIDTH_ROW,
                "Channel width",
                format!("{} MHz", width.mhz()),
            ),
        ] {
            let focused = self.wifi_focus == row;
            let value = if focused && self.wifi_editing {
                format!("‹ {value} ›")
            } else {
                value
            };
            self.render_list_row(
                canvas,
                row,
                label,
                WHITE,
                &value,
                if focused { GREEN } else { SECONDARY },
            );
        }
        canvas.text(
            &self.fonts,
            "Wi-Fi restarts after settings close",
            CONTENT_X,
            list_row_y(WIFI_ROWS) + ROW_TEXT_CENTER / 2,
            CAPTION_SIZE,
            SECONDARY,
            false,
        );
    }

    fn render_phones(&self, canvas: &mut Canvas) {
        if self.phones.is_empty() {
            if self.directory.is_some() {
                self.render_message(
                    canvas,
                    "No saved phones",
                    "Phones you connect wirelessly show up here",
                );
            } else {
                self.render_message(
                    canvas,
                    "Wireless is off",
                    "Enable wireless projection to manage phones",
                );
            }
            return;
        }
        for (index, phone) in self.phones.iter().enumerate() {
            let selected = self.phone_focus == index;
            let (value, color) = if self.is_active(phone) {
                ("Connected ›", GREEN)
            } else if self.is_pending(phone) {
                ("Connecting ›", SECONDARY)
            } else {
                ("›", SECONDARY)
            };
            self.render_list_row(
                canvas,
                index,
                &phone.name,
                WHITE,
                value,
                if selected { GREEN } else { color },
            );
        }
    }

    fn render_phone_actions(&self, canvas: &mut Canvas) {
        let Some(phone) = self.opened() else {
            return;
        };
        let (status, color) = if self.is_active(phone) {
            ("Connected", GREEN)
        } else if self.is_pending(phone) {
            ("Connecting", SECONDARY)
        } else {
            ("", SECONDARY)
        };
        self.render_list_row(
            canvas,
            CONNECT_ROW,
            if self.is_active(phone) {
                "Disconnect"
            } else {
                "Connect"
            },
            WHITE,
            status,
            if self.phone_action == CONNECT_ROW && color != GREEN {
                GREEN
            } else {
                color
            },
        );
        let (label, color) = if self.confirming_forget {
            ("Press again to forget", RED)
        } else {
            ("Forget", WHITE)
        };
        self.render_list_row(canvas, FORGET_ROW, label, color, "", SECONDARY);
    }

    fn is_active(&self, phone: &KnownPhone) -> bool {
        self.active_phone.as_deref() == Some(phone.address.as_str())
    }

    fn is_pending(&self, phone: &KnownPhone) -> bool {
        self.pending_connect.as_deref() == Some(phone.address.as_str())
    }

    fn render_list_row(
        &self,
        canvas: &mut Canvas,
        index: usize,
        label: &str,
        label_color: [u8; 3],
        value: &str,
        value_color: [u8; 3],
    ) {
        let y = list_row_y(index);
        canvas.rounded_rect(
            ROW_X,
            y,
            ROW_WIDTH,
            LIST_ROW_HEIGHT,
            14,
            WHITE,
            if self.list_focus() == index { 28 } else { 18 },
        );
        let value_width = self.fonts.text_width(value, VALUE_SIZE, true);
        let label = self.fonts.fit(
            label,
            LABEL_SIZE,
            true,
            CONTENT_RIGHT - value_width - CONTROL_GAP - CONTENT_X,
        );
        canvas.text(
            &self.fonts,
            &label,
            CONTENT_X,
            y + ROW_TEXT_CENTER,
            LABEL_SIZE,
            label_color,
            true,
        );
        canvas.text_right(
            &self.fonts,
            value,
            CONTENT_RIGHT,
            y + ROW_TEXT_CENTER,
            VALUE_SIZE,
            value_color,
            true,
        );
    }
}

const fn band_name(band: Band) -> &'static str {
    match band {
        Band::Ghz2_4 => "2.4 GHz",
        Band::Ghz5 => "5 GHz",
    }
}

fn step_option<T: Copy + PartialEq>(options: &[T], current: T, delta: isize, wrap: bool) -> T {
    let last = options.len().saturating_sub(1) as isize;
    let index = options
        .iter()
        .position(|&option| option == current)
        .unwrap_or(0) as isize;
    let next = if wrap {
        (index + delta).rem_euclid(last + 1)
    } else {
        (index + delta).clamp(0, last)
    };
    options.get(next as usize).copied().unwrap_or(current)
}

fn step_radio(
    (channel, width): (u8, ChannelWidth),
    row: usize,
    delta: isize,
    wrap: bool,
) -> (u8, ChannelWidth) {
    let band = Band::of_channel(channel);
    let channel = match row {
        WIFI_BAND_ROW => match step_option(&BANDS, band, delta, wrap) {
            next if next == band => channel,
            next => next.default_channel(),
        },
        WIFI_CHANNEL_ROW => step_option(band.channels(), channel, delta, wrap),
        _ => channel,
    };
    let width = match row {
        WIFI_WIDTH_ROW => step_option(ChannelWidth::supported_on(channel), width, delta, wrap),
        _ => width,
    };
    (channel, width.min(ChannelWidth::widest_for(channel)))
}

const SETTINGS_ROWS: [(i32, i32); 8] = [
    (PHONES_ROW_Y, PHONES_ROW_HEIGHT),
    (AUTO_BRIGHTNESS_ROW_Y, TOGGLE_ROW_HEIGHT),
    (AUTO_NIGHT_MODE_ROW_Y, TOGGLE_ROW_HEIGHT),
    (PHONE_AUDIO_ROW_Y, TOGGLE_ROW_HEIGHT),
    (EQUALIZER_ROW_Y, TOGGLE_ROW_HEIGHT),
    (DPI_ROW_Y, DPI_ROW_HEIGHT),
    (RESTORE_DPI_ROW_Y, RESTORE_ROW_HEIGHT),
    (ADVANCED_ROW_Y, ADVANCED_ROW_HEIGHT),
];

fn stop_at_or_after(stops: &[i32], offset: i32) -> i32 {
    if offset <= 0 {
        return 0;
    }
    stops
        .iter()
        .copied()
        .find(|&stop| stop >= offset)
        .or_else(|| stops.last().copied())
        .unwrap_or(0)
}

fn nearest_stop(stops: &[i32], offset: i32) -> i32 {
    stops
        .iter()
        .copied()
        .min_by_key(|&stop| (stop - offset).abs())
        .unwrap_or(0)
}

fn settings_row_at(y: i32) -> Option<usize> {
    SETTINGS_ROWS
        .iter()
        .position(|&(top, height)| (top..top + height).contains(&y))
}

const fn list_row_y(index: usize) -> i32 {
    FIRST_ROW_Y + index as i32 * (LIST_ROW_HEIGHT + ROW_GAP)
}

fn list_row_at(y: i32, rows: usize) -> Option<usize> {
    (0..rows).find(|&index| (list_row_y(index)..list_row_y(index) + LIST_ROW_HEIGHT).contains(&y))
}

fn selection_after_rotary(
    selected: usize,
    direction: RotaryDirection,
    steps: u8,
    last: usize,
) -> usize {
    let delta = usize::from(steps);
    match direction {
        RotaryDirection::Clockwise => selected.saturating_add(delta).min(last),
        RotaryDirection::CounterClockwise => selected.saturating_sub(delta),
    }
}

fn dpi_from_x(x: i32) -> u32 {
    let position = (x - SLIDER_X).clamp(0, SLIDER_WIDTH);
    let raw = MIN_DPI
        + ((u32::try_from(position).unwrap_or(0) * (MAX_DPI - MIN_DPI))
            / u32::try_from(SLIDER_WIDTH).unwrap_or(1));
    ((raw + DPI_STEP / 2) / DPI_STEP * DPI_STEP).clamp(MIN_DPI, MAX_DPI)
}

struct FontSet {
    regular: Option<Font>,
    semibold: Option<Font>,
}

impl FontSet {
    fn face(&self, semibold: bool) -> Option<&Font> {
        if semibold {
            self.semibold.as_ref().or(self.regular.as_ref())
        } else {
            self.regular.as_ref()
        }
    }

    fn text_width(&self, text: &str, size: f32, semibold: bool) -> i32 {
        if let Some(font) = self.face(semibold) {
            text.chars()
                .map(|character| font.metrics(character, size).advance_width)
                .sum::<f32>()
                .round() as i32
        } else {
            let scale = (size / 8.0).round().max(1.0) as i32;
            (text.chars().count() as i32 * 6 - 1).max(0) * scale
        }
    }

    fn fit(&self, text: &str, size: f32, semibold: bool, maximum_width: i32) -> String {
        if self.text_width(text, size, semibold) <= maximum_width {
            return text.to_owned();
        }
        let mut fitted: String = text.to_owned();
        while !fitted.is_empty() {
            fitted.pop();
            let candidate = format!("{}…", fitted.trim_end());
            if self.text_width(&candidate, size, semibold) <= maximum_width {
                return candidate;
            }
        }
        "…".to_owned()
    }

    fn load() -> Self {
        let directories = [
            "/usr/share/fonts/prelude",
            "/usr/share/fonts/nocturne",
            "/usr/share/fonts/truetype/inter",
            "/usr/share/fonts/truetype/Roboto",
            "/usr/share/fonts/truetype/roboto",
            "/usr/share/fonts/Roboto",
            "/usr/share/fonts/ttf",
            "/usr/share/fonts",
        ];
        let regular = load_font(&directories, &["Inter-Regular.ttf", "Roboto-Regular.ttf"]);
        let semibold = load_font(
            &directories,
            &["Inter-SemiBold.ttf", "Inter-Bold.ttf", "Roboto-Bold.ttf"],
        );
        if regular.is_none() {
            warn!("no Inter or Roboto font was found; using the built-in settings glyphs");
        }
        Self { regular, semibold }
    }
}

fn load_font(directories: &[&str], names: &[&str]) -> Option<Font> {
    directories
        .iter()
        .flat_map(|directory| names.iter().map(move |name| (*directory, *name)))
        .find_map(|(directory, name)| {
            let path = PathBuf::from(directory).join(name);
            let metadata = fs::symlink_metadata(&path).ok()?;
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || metadata.file_type().is_block_device()
                || metadata.len() == 0
                || metadata.len() > MAXIMUM_FONT_BYTES
            {
                return None;
            }
            let bytes = fs::read(path).ok()?;
            Font::from_bytes(bytes, FontSettings::default()).ok()
        })
}

struct Canvas {
    rgb: Vec<u8>,
    offset_y: i32,
    clip_top: i32,
}

impl Canvas {
    fn new() -> Self {
        Self {
            rgb: vec![0; DISPLAY_WIDTH * DISPLAY_HEIGHT * 3],
            offset_y: 0,
            clip_top: 0,
        }
    }

    fn scroll(&mut self, offset_y: i32, clip_top: i32) {
        self.offset_y = offset_y;
        self.clip_top = clip_top;
    }

    fn visible_rows(&self) -> std::ops::Range<i32> {
        self.offset_y + self.clip_top..self.offset_y + DISPLAY_HEIGHT as i32
    }

    #[allow(clippy::too_many_arguments)]
    fn rounded_rect(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        radius: i32,
        color: [u8; 3],
        alpha: u8,
    ) {
        let rows = self.visible_rows();
        for py in y.max(rows.start)..(y + height).min(rows.end) {
            for px in x.max(0)..(x + width).min(DISPLAY_WIDTH as i32) {
                let corner_x = if px < x + radius {
                    x + radius
                } else if px >= x + width - radius {
                    x + width - radius - 1
                } else {
                    px
                };
                let corner_y = if py < y + radius {
                    y + radius
                } else if py >= y + height - radius {
                    y + height - radius - 1
                } else {
                    py
                };
                let inside = (px - corner_x).pow(2) + (py - corner_y).pow(2) <= radius.pow(2);
                if inside {
                    self.blend(px, py, color, alpha);
                }
            }
        }
    }

    fn circle(&mut self, center_x: i32, center_y: i32, radius: i32, color: [u8; 3], alpha: u8) {
        let rows = self.visible_rows();
        for y in (center_y - radius).max(rows.start)..(center_y + radius).min(rows.end) {
            for x in (center_x - radius).max(0)..(center_x + radius).min(DISPLAY_WIDTH as i32) {
                if (x - center_x).pow(2) + (y - center_y).pow(2) <= radius.pow(2) {
                    self.blend(x, y, color, alpha);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn text(
        &mut self,
        fonts: &FontSet,
        text: &str,
        x: i32,
        center_y: i32,
        size: f32,
        color: [u8; 3],
        semibold: bool,
    ) {
        let font = fonts.face(semibold);
        if let Some(font) = font {
            self.font_text(font, text, x, center_y, size, color);
        } else {
            self.bitmap_text(text, x, center_y, size, color);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn text_right(
        &mut self,
        fonts: &FontSet,
        text: &str,
        right: i32,
        center_y: i32,
        size: f32,
        color: [u8; 3],
        semibold: bool,
    ) {
        let width = fonts.text_width(text, size, semibold);
        self.text(fonts, text, right - width, center_y, size, color, semibold);
    }

    fn font_text(
        &mut self,
        font: &Font,
        text: &str,
        x: i32,
        center_y: i32,
        size: f32,
        color: [u8; 3],
    ) {
        let mut cursor_x = x as f32;
        let baseline = font
            .horizontal_line_metrics(size)
            .map_or(size / 2.0, |metrics| {
                (metrics.ascent + metrics.descent) / 2.0
            })
            .round() as i32
            + center_y;
        for character in text.chars() {
            let (metrics, bitmap) = font.rasterize(character, size);
            let glyph_x = cursor_x.round() as i32 + metrics.xmin;
            let glyph_y = baseline - metrics.height as i32 - metrics.ymin;
            for row in 0..metrics.height {
                for column in 0..metrics.width {
                    let alpha = bitmap[row * metrics.width + column];
                    if alpha != 0 {
                        self.blend(glyph_x + column as i32, glyph_y + row as i32, color, alpha);
                    }
                }
            }
            cursor_x += metrics.advance_width;
        }
    }

    fn bitmap_text(&mut self, text: &str, x: i32, center_y: i32, size: f32, color: [u8; 3]) {
        let scale = (size / 8.0).round().max(1.0) as i32;
        let mut cursor_x = x;
        let y = center_y - 7 * scale / 2;
        for character in text.chars() {
            if let Some(glyph) = bitmap_glyph(character) {
                for (row, bits) in glyph.iter().enumerate() {
                    for column in 0..5 {
                        if bits & (1 << (4 - column)) != 0 {
                            self.rounded_rect(
                                cursor_x + column * scale,
                                y + row as i32 * scale,
                                scale,
                                scale,
                                0,
                                color,
                                255,
                            );
                        }
                    }
                }
            }
            cursor_x += 6 * scale;
        }
    }

    fn blend(&mut self, x: i32, y: i32, color: [u8; 3], alpha: u8) {
        let y = y - self.offset_y;
        if !(0..DISPLAY_WIDTH as i32).contains(&x)
            || !(self.clip_top..DISPLAY_HEIGHT as i32).contains(&y)
        {
            return;
        }
        let offset = (y as usize * DISPLAY_WIDTH + x as usize) * 3;
        let alpha = u16::from(alpha);
        let inverse = 255_u16.saturating_sub(alpha);
        for (channel, &foreground) in color.iter().enumerate() {
            self.rgb[offset + channel] = ((u16::from(self.rgb[offset + channel]) * inverse
                + u16::from(foreground) * alpha
                + 127)
                / 255) as u8;
        }
    }

    fn to_nv12(&self) -> Vec<u8> {
        let mut frame = vec![0_u8; DISPLAY_WIDTH * DISPLAY_HEIGHT * 3 / 2];
        let luma_size = DISPLAY_WIDTH * DISPLAY_HEIGHT;
        for y in 0..DISPLAY_HEIGHT {
            for x in 0..DISPLAY_WIDTH {
                let offset = (y * DISPLAY_WIDTH + x) * 3;
                frame[y * DISPLAY_WIDTH + x] =
                    rgb_to_yuv(self.rgb[offset], self.rgb[offset + 1], self.rgb[offset + 2]).0;
            }
        }
        for y in (0..DISPLAY_HEIGHT).step_by(2) {
            for x in (0..DISPLAY_WIDTH).step_by(2) {
                let mut u = 0_u16;
                let mut v = 0_u16;
                for sample_y in y..y + 2 {
                    for sample_x in x..x + 2 {
                        let offset = (sample_y * DISPLAY_WIDTH + sample_x) * 3;
                        let (_, sample_u, sample_v) = rgb_to_yuv(
                            self.rgb[offset],
                            self.rgb[offset + 1],
                            self.rgb[offset + 2],
                        );
                        u += u16::from(sample_u);
                        v += u16::from(sample_v);
                    }
                }
                let uv_offset = luma_size + (y / 2) * DISPLAY_WIDTH + x;
                frame[uv_offset] = ((u + 2) / 4) as u8;
                frame[uv_offset + 1] = ((v + 2) / 4) as u8;
            }
        }
        frame
    }
}

fn rgb_to_yuv(red: u8, green: u8, blue: u8) -> (u8, u8, u8) {
    let red = i32::from(red);
    let green = i32::from(green);
    let blue = i32::from(blue);
    let clamp = |value: i32| value.clamp(0, 255) as u8;
    (
        clamp(((47 * red + 157 * green + 16 * blue + 128) >> 8) + 16),
        clamp(((-26 * red - 86 * green + 112 * blue + 128) >> 8) + 128),
        clamp(((112 * red - 102 * green - 10 * blue + 128) >> 8) + 128),
    )
}

fn bitmap_glyph(character: char) -> Option<[u8; 7]> {
    let character = character.to_ascii_uppercase();
    Some(match character {
        'A' => [0x0e, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11],
        'B' => [0x1e, 0x11, 0x11, 0x1e, 0x11, 0x11, 0x1e],
        'C' => [0x0f, 0x10, 0x10, 0x10, 0x10, 0x10, 0x0f],
        'D' => [0x1e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1e],
        'E' => [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x1f],
        'F' => [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x10],
        'G' => [0x0f, 0x10, 0x10, 0x17, 0x11, 0x11, 0x0f],
        'H' => [0x11, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11],
        'I' => [0x1f, 0x04, 0x04, 0x04, 0x04, 0x04, 0x1f],
        'J' => [0x01, 0x01, 0x01, 0x01, 0x11, 0x11, 0x0e],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1f],
        'M' => [0x11, 0x1b, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e],
        'P' => [0x1e, 0x11, 0x11, 0x1e, 0x10, 0x10, 0x10],
        'Q' => [0x0e, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0d],
        'R' => [0x1e, 0x11, 0x11, 0x1e, 0x14, 0x12, 0x11],
        'S' => [0x0f, 0x10, 0x10, 0x0e, 0x01, 0x01, 0x1e],
        'T' => [0x1f, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0a, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x1b, 0x11],
        'X' => [0x11, 0x11, 0x0a, 0x04, 0x0a, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0a, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1f, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1f],
        '0' => [0x0e, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0e],
        '1' => [0x04, 0x0c, 0x04, 0x04, 0x04, 0x04, 0x0e],
        '2' => [0x0e, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1f],
        '3' => [0x1e, 0x01, 0x01, 0x0e, 0x01, 0x01, 0x1e],
        '4' => [0x02, 0x06, 0x0a, 0x12, 0x1f, 0x02, 0x02],
        '5' => [0x1f, 0x10, 0x10, 0x1e, 0x01, 0x01, 0x1e],
        '6' => [0x0e, 0x10, 0x10, 0x1e, 0x11, 0x11, 0x0e],
        '7' => [0x1f, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0e, 0x11, 0x11, 0x0e, 0x11, 0x11, 0x0e],
        '9' => [0x0e, 0x11, 0x11, 0x0f, 0x01, 0x01, 0x0e],
        ' ' => [0; 7],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x0c],
        ':' => [0x00, 0x0c, 0x0c, 0x00, 0x0c, 0x0c, 0x00],
        '-' | '−' => [0x00, 0x00, 0x00, 0x1f, 0x00, 0x00, 0x00],
        '/' => [0x01, 0x02, 0x02, 0x04, 0x08, 0x08, 0x10],
        '+' => [0x00, 0x04, 0x04, 0x1f, 0x04, 0x04, 0x00],
        '·' => [0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00],
        '•' => [0x00, 0x04, 0x0e, 0x1f, 0x0e, 0x04, 0x00],
        '‹' => [0x00, 0x02, 0x04, 0x08, 0x04, 0x02, 0x00],
        '›' => [0x00, 0x08, 0x04, 0x02, 0x04, 0x08, 0x00],
        '…' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x15],
        '\'' | '’' => [0x04, 0x04, 0x08, 0x00, 0x00, 0x00, 0x00],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpi_slider_clamps_and_snaps_to_ten() {
        assert_eq!(dpi_from_x(0), MIN_DPI);
        assert_eq!(dpi_from_x(SLIDER_X + SLIDER_WIDTH), MAX_DPI);
        assert_eq!(dpi_from_x(SLIDER_X + SLIDER_WIDTH / 2), 290);
    }

    #[test]
    fn settings_rows_have_consistent_spacing_and_fit_on_screen() {
        for (index, pair) in SETTINGS_ROWS.windows(2).enumerate() {
            let [(top, height), (next, _)] = pair else {
                unreachable!("windows(2) yields pairs");
            };
            assert_eq!(next - (top + height), ROW_GAP, "gap after row {index}");
        }
        assert_eq!(SETTINGS_ROWS[PHONES_ROW].0, FIRST_ROW_Y);
        assert_eq!(RESTORE_ROW_HEIGHT, TOGGLE_ROW_HEIGHT);
        assert_eq!(SLIDER_X + SLIDER_WIDTH, ROW_X + ROW_WIDTH - ROW_INSET);
        assert_eq!(PHONES_ROW_HEIGHT, TOGGLE_ROW_HEIGHT);
        assert_eq!(LIST_ROW_HEIGHT, TOGGLE_ROW_HEIGHT);
        assert_eq!(
            list_row_y(1) - list_row_y(0),
            AUTO_BRIGHTNESS_ROW_Y - PHONES_ROW_Y
        );
    }

    #[test]
    fn touch_rows_map_to_their_settings() {
        assert_eq!(settings_row_at(FIRST_ROW_Y), Some(PHONES_ROW));
        assert_eq!(
            settings_row_at(AUTO_BRIGHTNESS_ROW_Y),
            Some(AUTO_BRIGHTNESS_ROW)
        );
        assert_eq!(settings_row_at(PHONE_AUDIO_ROW_Y), Some(PHONE_AUDIO_ROW));
        assert_eq!(
            settings_row_at(DPI_ROW_Y + DPI_ROW_HEIGHT - 1),
            Some(DPI_ROW)
        );
        assert_eq!(settings_row_at(FIRST_ROW_Y + TOGGLE_ROW_HEIGHT), None);
        assert_eq!(settings_row_at(VIEWPORT_TOP - 1), None);
    }

    #[test]
    fn rotary_navigation_runs_from_phones_to_advanced_and_stops() {
        assert_eq!(SETTINGS_ROWS.len() - 1, ADVANCED_ROW);
        assert_eq!(
            selection_after_rotary(PHONES_ROW, RotaryDirection::Clockwise, 1, ADVANCED_ROW),
            AUTO_BRIGHTNESS_ROW
        );
        assert_eq!(
            selection_after_rotary(DPI_ROW, RotaryDirection::Clockwise, 5, ADVANCED_ROW),
            ADVANCED_ROW
        );
        assert_eq!(
            selection_after_rotary(
                AUTO_BRIGHTNESS_ROW,
                RotaryDirection::CounterClockwise,
                3,
                ADVANCED_ROW
            ),
            PHONES_ROW
        );
    }

    #[test]
    fn wifi_band_switch_resets_the_channel_and_caps_the_width() {
        assert_eq!(
            step_radio((36, ChannelWidth::Mhz20), WIFI_BAND_ROW, 1, true),
            (6, ChannelWidth::Mhz20)
        );
        assert_eq!(
            step_radio((36, ChannelWidth::Mhz80), WIFI_BAND_ROW, -1, false),
            (6, ChannelWidth::Mhz40)
        );
        assert_eq!(
            step_radio((36, ChannelWidth::Mhz20), WIFI_BAND_ROW, 1, false),
            (36, ChannelWidth::Mhz20)
        );
    }

    #[test]
    fn wifi_channels_stay_in_band() {
        assert_eq!(
            step_radio((161, ChannelWidth::Mhz80), WIFI_CHANNEL_ROW, 1, false),
            (165, ChannelWidth::Mhz20)
        );
        assert_eq!(
            step_radio((165, ChannelWidth::Mhz20), WIFI_CHANNEL_ROW, 1, false),
            (165, ChannelWidth::Mhz20)
        );
        assert_eq!(
            step_radio((165, ChannelWidth::Mhz20), WIFI_CHANNEL_ROW, 1, true),
            (36, ChannelWidth::Mhz20)
        );
        assert_eq!(
            step_radio((48, ChannelWidth::Mhz20), WIFI_CHANNEL_ROW, 1, false),
            (149, ChannelWidth::Mhz20)
        );
        assert_eq!(
            step_radio((1, ChannelWidth::Mhz20), WIFI_CHANNEL_ROW, 20, false),
            (13, ChannelWidth::Mhz20)
        );
    }

    #[test]
    fn wifi_width_only_offers_what_the_channel_supports() {
        assert_eq!(
            step_radio((36, ChannelWidth::Mhz20), WIFI_WIDTH_ROW, 2, false),
            (36, ChannelWidth::Mhz80)
        );
        assert_eq!(
            step_radio((6, ChannelWidth::Mhz40), WIFI_WIDTH_ROW, 1, true),
            (6, ChannelWidth::Mhz20)
        );
        assert_eq!(
            step_radio((165, ChannelWidth::Mhz20), WIFI_WIDTH_ROW, 1, false),
            (165, ChannelWidth::Mhz20)
        );
    }

    #[test]
    fn phone_rows_use_the_settings_row_geometry() {
        assert_eq!(list_row_at(FIRST_ROW_Y, 2), Some(CONNECT_ROW));
        assert_eq!(
            list_row_at(list_row_y(1) + LIST_ROW_HEIGHT - 1, 2),
            Some(FORGET_ROW)
        );
        assert_eq!(list_row_at(FIRST_ROW_Y + LIST_ROW_HEIGHT, 2), None);
        assert_eq!(list_row_at(list_row_y(2), 2), None);
        assert_eq!(
            list_row_at(list_row_y(3), crate::MAXIMUM_KNOWN_PHONES),
            Some(3)
        );
    }

    #[test]
    fn scrolled_canvas_draws_rows_below_the_fixed_title() {
        let pixel = |canvas: &Canvas, y: usize| canvas.rgb[y * DISPLAY_WIDTH * 3];
        let mut canvas = Canvas::new();
        canvas.scroll(100, VIEWPORT_TOP);
        canvas.rounded_rect(0, 0, 10, 600, 0, WHITE, 255);
        assert_eq!(pixel(&canvas, VIEWPORT_TOP as usize - 1), 0);
        assert_eq!(pixel(&canvas, VIEWPORT_TOP as usize), 255);
        assert_eq!(pixel(&canvas, DISPLAY_HEIGHT - 1), 255);

        let mut canvas = Canvas::new();
        canvas.scroll(100, VIEWPORT_TOP);
        canvas.rounded_rect(0, 500, 10, 20, 0, WHITE, 255);
        assert_eq!(pixel(&canvas, 400), 255);
        assert_eq!(pixel(&canvas, 399), 0);
    }

    #[test]
    fn scrolling_rests_on_row_edges_so_the_title_gap_never_changes() {
        let stops: Vec<i32> = SETTINGS_ROWS
            .iter()
            .map(|&(top, _)| top - FIRST_ROW_Y)
            .collect();
        let overflow = ADVANCED_ROW_Y + ADVANCED_ROW_HEIGHT + ROW_GAP - DISPLAY_HEIGHT as i32;
        let maximum = stop_at_or_after(&stops, overflow);
        assert!(maximum >= overflow);
        assert_eq!(
            SETTINGS_ROWS[EQUALIZER_ROW].0 - maximum,
            FIRST_ROW_Y,
            "at the bottom the first visible row sits where Phones sits at the top"
        );
        assert_eq!(stop_at_or_after(&stops, -5), 0);
        assert_eq!(nearest_stop(&stops, 30), 0);
        assert_eq!(nearest_stop(&stops, 50), stops[1]);
    }

    #[test]
    fn long_names_are_truncated_to_fit() {
        let fonts = FontSet {
            regular: None,
            semibold: None,
        };
        assert_eq!(fonts.fit("Pixel", LABEL_SIZE, true, 400), "Pixel");
        let fitted = fonts.fit("A very long phone name indeed", LABEL_SIZE, true, 200);
        assert!(fitted.ends_with('…'));
        assert!(fonts.text_width(&fitted, LABEL_SIZE, true) <= 200);
    }

    #[test]
    fn rendered_settings_frame_is_nv12_for_the_car_thing_surface() {
        let canvas = Canvas::new();
        assert_eq!(
            canvas.to_nv12().len(),
            DISPLAY_WIDTH * DISPLAY_HEIGHT * 3 / 2
        );
    }

    #[test]
    fn nv12_conversion_uses_limited_range_neutral_black_and_white() {
        assert_eq!(rgb_to_yuv(0, 0, 0), (16, 128, 128));
        assert_eq!(rgb_to_yuv(255, 255, 255), (235, 128, 128));
    }

    #[test]
    fn built_in_font_covers_settings_labels() {
        for character in
            "Settings Auto brightness night mode Phone audio Reported DPI Restore original On Off \
             ‹ Phones › saved None Connect Disconnect Connected Connecting Forget Confirm \
             No saved phones you connect wirelessly show up here Wireless is off Enable projection \
             to manage Unable save settings Advanced Wi-Fi band Channel width 2.4 GHz 5 MHz \
             change restarts after close Equalizer Reset flat Preamp +12.0 dB · Editing \
             All USB output Music calls navigation assistants Boosts can clip Lower add headroom \
             unavailable Update device image use bypasses 31.25 62.5 1k 2k 4k 8k 16k \
             −24 EQ off curve saved USB audio only Turn choose Press adjust finish Release reduce"
                .chars()
        {
            assert!(
                bitmap_glyph(character).is_some(),
                "missing glyph {character:?}"
            );
        }
    }
}
