use super::*;
use prelude_media::{
    EqualizerSettings, MAX_BAND_HALF_DB, MAX_PREAMP_HALF_DB, MIN_BAND_HALF_DB, MIN_PREAMP_HALF_DB,
    half_db_to_db,
};

const BAND_LEFT: i32 = 84;
const BAND_WIDTH: i32 = 66;
const TRACK_TOP: i32 = 136;
const TRACK_BOTTOM: i32 = 310;
const BAND_LABEL_Y: i32 = 337;
const PREAMP_LEFT: i32 = 218;
const PREAMP_RIGHT: i32 = 632;
const PREAMP_Y: i32 = 417;
const THUMB_HIT_RADIUS: i32 = 22;
pub(super) const HEADER_LEFT: i32 = 424;
const RESET: usize = 0;
const TOGGLE: usize = 1;
const FIRST_BAND: usize = 2;
const PREAMP: usize = FIRST_BAND + 10;
const LABELS: [&str; 10] = [
    "31", "63", "125", "250", "500", "1k", "2k", "4k", "8k", "16k",
];

pub(super) struct EqualizerUi {
    pub(super) settings: EqualizerSettings,
    focus: usize,
    editing: bool,
    touched: Option<usize>,
    drag_offset: Option<i32>,
    title_touched: bool,
    pub(super) dirty: bool,
}

impl EqualizerUi {
    pub(super) fn persist(
        &mut self,
        state_dir: &std::path::Path,
        current: RuntimeSettings,
    ) -> Result<RuntimeSettings, crate::settings::SettingsError> {
        if !self.dirty {
            return Ok(current);
        }
        let next = RuntimeSettings {
            equalizer: self.settings,
            ..current
        };
        next.persist(state_dir)?;
        self.dirty = false;
        Ok(next)
    }

    pub(super) fn focus_toggle(&mut self) {
        self.focus = TOGGLE;
    }

    pub(super) fn cancel_interaction(&mut self) {
        self.touched = None;
        self.drag_offset = None;
        self.title_touched = false;
        self.editing = false;
    }

    pub(super) fn new(settings: EqualizerSettings) -> Self {
        Self {
            settings,
            focus: TOGGLE,
            editing: false,
            touched: None,
            drag_offset: None,
            title_touched: false,
            dirty: false,
        }
    }

    pub(super) fn button(&mut self, button: KnobInputButton) -> bool {
        match button {
            KnobInputButton::Back if self.editing => self.editing = false,
            KnobInputButton::Back => return true,
            KnobInputButton::Select => match self.focus {
                TOGGLE => {
                    self.settings.enabled = !self.settings.enabled;
                    self.dirty = true;
                }
                RESET => {
                    self.settings.bands_half_db = [0; 10];
                    self.settings.preamp_half_db = 0;
                    self.dirty = true;
                }
                _ => self.editing = !self.editing,
            },
        }
        false
    }

    pub(super) fn rotary(&mut self, direction: RotaryDirection, steps: u8) {
        if self.editing {
            let delta = if direction == RotaryDirection::Clockwise {
                i16::from(steps)
            } else {
                -i16::from(steps)
            };
            if self.focus == PREAMP {
                self.settings.preamp_half_db = (i16::from(self.settings.preamp_half_db) + delta)
                    .clamp(i16::from(MIN_PREAMP_HALF_DB), i16::from(MAX_PREAMP_HALF_DB))
                    as i8;
            } else if let Some(band) = band(self.focus) {
                let gain = &mut self.settings.bands_half_db[band];
                *gain = (i16::from(*gain) + delta)
                    .clamp(i16::from(MIN_BAND_HALF_DB), i16::from(MAX_BAND_HALF_DB))
                    as i8;
            }
            self.dirty = true;
        } else {
            self.focus = selection_after_rotary(self.focus, direction, steps, PREAMP);
        }
    }

    pub(super) fn touch(&mut self, phase: TouchInputPhase, x: u16, y: u16) -> bool {
        let (x, y) = (i32::from(x), i32::from(y));
        match phase {
            TouchInputPhase::Down => {
                self.touched = hit(x, y);
                self.title_touched = self.touched.is_none() && back_hit(x, y);
                self.drag_offset = None;
                self.editing = false;
                if let Some(focus) = self.touched {
                    self.focus = focus;
                    if let Some(band) = band(focus)
                        && (TRACK_TOP - 18..=TRACK_BOTTOM + 18).contains(&y)
                    {
                        let thumb_y = band_y(self.settings.bands_half_db[band]);
                        self.drag_offset = Some(if (y - thumb_y).abs() <= THUMB_HIT_RADIUS {
                            y - thumb_y
                        } else {
                            0
                        });
                    } else if focus == PREAMP
                        && (PREAMP_LEFT - 18..=PREAMP_RIGHT + 18).contains(&x)
                        && (y - PREAMP_Y).abs() <= 18
                    {
                        let thumb_x = preamp_x(self.settings.preamp_half_db);
                        self.drag_offset = Some(if (x - thumb_x).abs() <= THUMB_HIT_RADIUS {
                            x - thumb_x
                        } else {
                            0
                        });
                    }
                    self.drag(x, y);
                }
            }
            TouchInputPhase::Move => {
                if self.touched.is_some() {
                    self.drag(x, y);
                }
            }
            TouchInputPhase::Up => {
                let touched = self.touched.take();
                let title_touched = std::mem::take(&mut self.title_touched);
                if let Some(focus) = touched {
                    if (FIRST_BAND..=PREAMP).contains(&focus) {
                        self.drag(x, y);
                    }
                    self.drag_offset = None;
                    if (focus == TOGGLE || focus == RESET) && hit(x, y) == Some(focus) {
                        self.button(KnobInputButton::Select);
                    }
                } else if title_touched && back_hit(x, y) {
                    return true;
                }
            }
        }
        false
    }

    fn drag(&mut self, x: i32, y: i32) {
        let Some(offset) = self.drag_offset else {
            return;
        };
        if let Some(band) = band(self.focus) {
            let gain = band_gain(y - offset);
            let current = &mut self.settings.bands_half_db[band];
            self.dirty |= *current != gain;
            *current = gain;
        } else if self.focus == PREAMP {
            let gain = preamp_gain(x - offset);
            self.dirty |= self.settings.preamp_half_db != gain;
            self.settings.preamp_half_db = gain;
        }
    }

    fn adjusting(&self, focus: usize) -> bool {
        self.focus == focus && (self.editing || self.drag_offset.is_some())
    }

    pub(super) fn render_header(&self, canvas: &mut Canvas, fonts: &FontSet) {
        let reset_focused = self.focus == RESET;
        canvas.rounded_rect(
            448,
            20,
            112,
            48,
            24,
            if reset_focused { FOCUS } else { GROUP },
            255,
        );
        if reset_focused {
            canvas.ring(442, 14, 124, 60, 30, 2.5, WHITE);
        }
        canvas.text_center(fonts, "Reset", 504, TITLE_Y, 23.0, WHITE, false);
        let x = CONTENT_RIGHT - SWITCH_WIDTH;
        canvas.switch(x, TITLE_Y, self.settings.enabled);
        if self.focus == TOGGLE {
            canvas.ring(
                x - 6,
                TITLE_Y - SWITCH_HEIGHT / 2 - 6,
                SWITCH_WIDTH + 12,
                SWITCH_HEIGHT + 12,
                SWITCH_HEIGHT / 2 + 6,
                2.5,
                WHITE,
            );
        }
    }

    fn focus_frame(&self, canvas: &mut Canvas, focus: usize, bounds: (i32, i32, i32, i32)) {
        if self.focus == focus {
            let (x, y, width, height) = bounds;
            let radius = (height / 2).min(14);
            canvas.rounded_rect(x, y, width, height, radius, FOCUS, 255);
            if self.adjusting(focus) {
                canvas.ring(x, y, width, height, radius, 2.0, GREEN);
            }
        }
    }

    pub(super) fn render(
        &self,
        canvas: &mut Canvas,
        fonts: &FontSet,
        phone_audio: bool,
        availability: prelude_media::EqualizerAvailability,
        error_message: Option<&str>,
    ) {
        let scope = if phone_audio {
            "USB audio only · Phone audio bypasses EQ"
        } else if !self.settings.enabled {
            "All USB audio · EQ off, curve saved"
        } else {
            "All USB audio"
        };
        canvas.text(fonts, scope, CONTENT_X, 96, CAPTION_SIZE, SECONDARY, false);

        let zero_y = band_y(0);
        for (label, y) in [("+12", TRACK_TOP), ("0", zero_y), ("−12", TRACK_BOTTOM)] {
            canvas.text_right(fonts, label, 70, y, 18.0, SECONDARY, false);
        }
        if let Some(band) = band(self.focus) {
            let x = BAND_LEFT + band as i32 * BAND_WIDTH + BAND_WIDTH / 2;
            self.focus_frame(canvas, self.focus, (x - 28, 116, 56, 239));
        }
        canvas.rounded_rect(BAND_LEFT, zero_y, BAND_WIDTH * 10, 1, 0, WHITE, 55);
        for (index, gain) in self.settings.bands_half_db.iter().enumerate() {
            let focus = index + FIRST_BAND;
            let x = BAND_LEFT + index as i32 * BAND_WIDTH + BAND_WIDTH / 2;
            let adjusting = self.adjusting(focus);
            let focused = self.focus == focus;
            canvas.rounded_rect(x - 3, TRACK_TOP, 6, TRACK_BOTTOM - TRACK_TOP, 3, WHITE, 55);
            let y = band_y(*gain);
            let color = if adjusting {
                GREEN
            } else if focused {
                WHITE
            } else {
                SECONDARY
            };
            canvas.rounded_rect(
                x - 3,
                y.min(zero_y),
                6,
                (y - zero_y).abs().max(1),
                3,
                color,
                255,
            );
            canvas.circle(x, y, if focused { 13 } else { 10 }, color, 255);
            let label = LABELS[index];
            canvas.text(
                fonts,
                label,
                x - fonts.text_width(label, 21.0, focused) / 2,
                BAND_LABEL_Y,
                21.0,
                if focused { GREEN } else { SECONDARY },
                focused,
            );
        }
        let value = match (band(self.focus), self.focus) {
            (Some(band), _) => format!(
                "{} Hz  ·  {}",
                prelude_media::EQUALIZER_FREQUENCIES_HZ[band],
                gain_label(self.settings.bands_half_db[band]),
            ),
            (None, PREAMP) => format!("Preamp  ·  {}", gain_label(self.settings.preamp_half_db)),
            _ => "Select a band to adjust".to_owned(),
        };
        canvas.text(fonts, &value, CONTENT_X, 372, 26.0, WHITE, false);
        let hint = if self.editing {
            "Turn to adjust · Press to finish"
        } else if self.drag_offset.is_some() {
            "Release to save"
        } else {
            "Turn to choose · Press to adjust"
        };
        let hint = fonts.fit(
            hint,
            CAPTION_SIZE,
            false,
            CONTENT_RIGHT - CONTENT_X - fonts.text_width(&value, 26.0, false) - 24,
        );
        canvas.text_right(
            fonts,
            &hint,
            CONTENT_RIGHT,
            372,
            CAPTION_SIZE,
            SECONDARY,
            false,
        );

        canvas.rounded_rect(CONTENT_X, 392, CONTENT_RIGHT - CONTENT_X, 1, 0, WHITE, 35);
        self.focus_frame(canvas, PREAMP, (GROUP_X + 4, 398, GROUP_WIDTH - 8, 58));
        canvas.text(fonts, "Preamp", CONTENT_X, PREAMP_Y, 24.0, WHITE, false);
        canvas.text_right(
            fonts,
            &gain_label(self.settings.preamp_half_db),
            CONTENT_RIGHT,
            PREAMP_Y,
            22.0,
            if self.adjusting(PREAMP) { GREEN } else { WHITE },
            true,
        );
        canvas.rounded_rect(
            PREAMP_LEFT,
            PREAMP_Y - 3,
            PREAMP_RIGHT - PREAMP_LEFT,
            6,
            3,
            WHITE,
            55,
        );
        let x = preamp_x(self.settings.preamp_half_db);
        let zero_x = preamp_x(0);
        let color = if self.adjusting(PREAMP) {
            GREEN
        } else {
            SECONDARY
        };
        canvas.rounded_rect(
            x.min(zero_x),
            PREAMP_Y - 3,
            (x - zero_x).abs().max(1),
            6,
            3,
            color,
            255,
        );
        canvas.rounded_rect(zero_x - 1, PREAMP_Y - 9, 2, 18, 0, SECONDARY, 255);
        canvas.circle(
            x,
            PREAMP_Y,
            12,
            if self.adjusting(PREAMP) {
                GREEN
            } else if self.focus == PREAMP {
                WHITE
            } else {
                SECONDARY
            },
            255,
        );
        for (label, x) in [
            ("−24", PREAMP_LEFT),
            ("0", zero_x),
            ("+12 dB", PREAMP_RIGHT),
        ] {
            canvas.text(
                fonts,
                label,
                x - fonts.text_width(label, 16.0, false) / 2,
                444,
                16.0,
                SECONDARY,
                false,
            );
        }
        let unavailable = availability == prelude_media::EqualizerAvailability::Unavailable;
        if let Some(footer) = error_message
            .or_else(|| unavailable.then_some("EQ unavailable · USB audio bypasses EQ"))
        {
            canvas.text(fonts, footer, CONTENT_X, 469, CAPTION_SIZE, RED, false);
        }
    }
}

fn band(focus: usize) -> Option<usize> {
    (FIRST_BAND..PREAMP)
        .contains(&focus)
        .then(|| focus - FIRST_BAND)
}

fn gain_label(gain: i8) -> String {
    if gain == 0 {
        "0.0 dB".to_owned()
    } else {
        format!("{:+.1} dB", half_db_to_db(gain))
    }
}

fn band_y(gain: i8) -> i32 {
    TRACK_BOTTOM
        - (i32::from(gain) - i32::from(MIN_BAND_HALF_DB)) * (TRACK_BOTTOM - TRACK_TOP)
            / i32::from(MAX_BAND_HALF_DB - MIN_BAND_HALF_DB)
}

fn preamp_x(gain: i8) -> i32 {
    PREAMP_LEFT
        + (i32::from(gain) - i32::from(MIN_PREAMP_HALF_DB)) * (PREAMP_RIGHT - PREAMP_LEFT)
            / i32::from(MAX_PREAMP_HALF_DB - MIN_PREAMP_HALF_DB)
}

fn back_hit(x: i32, y: i32) -> bool {
    (32..344).contains(&x) && (0..VIEWPORT_TOP).contains(&y)
}

fn hit(x: i32, y: i32) -> Option<usize> {
    if (18..70).contains(&y) {
        if (588..768).contains(&x) {
            return Some(TOGGLE);
        }
        if (440..568).contains(&x) {
            return Some(RESET);
        }
    }
    if (116..356).contains(&y) && (BAND_LEFT..BAND_LEFT + BAND_WIDTH * 10).contains(&x) {
        return Some(((x - BAND_LEFT) / BAND_WIDTH) as usize + FIRST_BAND);
    }
    if (398..456).contains(&y) && (32..768).contains(&x) {
        return Some(PREAMP);
    }
    None
}
fn band_gain(y: i32) -> i8 {
    (f64::from(TRACK_BOTTOM - y.clamp(TRACK_TOP, TRACK_BOTTOM))
        * f64::from(MAX_BAND_HALF_DB - MIN_BAND_HALF_DB)
        / f64::from(TRACK_BOTTOM - TRACK_TOP))
    .round() as i8
        + MIN_BAND_HALF_DB
}
fn preamp_gain(x: i32) -> i8 {
    (f64::from(x.clamp(PREAMP_LEFT, PREAMP_RIGHT) - PREAMP_LEFT)
        * f64::from(MAX_PREAMP_HALF_DB - MIN_PREAMP_HALF_DB)
        / f64::from(PREAMP_RIGHT - PREAMP_LEFT))
    .round() as i8
        + MIN_PREAMP_HALF_DB
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_select_without_changing_the_curve_or_starting_a_drag() {
        let mut ui = EqualizerUi::new(EqualizerSettings {
            bands_half_db: [4; 10],
            preamp_half_db: -12,
            ..Default::default()
        });
        let original = ui.settings;
        for index in 0..10 {
            let x = (BAND_LEFT + index * BAND_WIDTH + BAND_WIDTH / 2) as u16;
            ui.touch(TouchInputPhase::Down, x, BAND_LABEL_Y as u16);
            ui.touch(TouchInputPhase::Move, x, TRACK_TOP as u16);
            ui.touch(TouchInputPhase::Up, x, TRACK_TOP as u16);
            assert_eq!(ui.focus, index as usize + FIRST_BAND);
            assert_eq!(ui.settings, original);
            assert!(!ui.dirty);
        }
        for (x, y) in [(90, PREAMP_Y), (710, PREAMP_Y), (PREAMP_LEFT, 444)] {
            ui.touch(TouchInputPhase::Down, x as u16, y as u16);
            ui.touch(TouchInputPhase::Move, PREAMP_RIGHT as u16, PREAMP_Y as u16);
            ui.touch(TouchInputPhase::Up, PREAMP_RIGHT as u16, PREAMP_Y as u16);
            assert_eq!(ui.focus, PREAMP);
            assert_eq!(ui.settings, original);
            assert!(!ui.dirty);
        }
    }

    #[test]
    fn thumb_drags_preserve_the_grab_offset_and_capture_the_original_band() {
        let mut ui = EqualizerUi::new(EqualizerSettings {
            bands_half_db: [7; 10],
            preamp_half_db: -12,
            ..Default::default()
        });
        let y = (band_y(7) + 10) as u16;
        ui.touch(TouchInputPhase::Down, 117, y);
        assert_eq!(ui.settings.bands_half_db[0], 7);
        assert!(!ui.dirty, "grabbing a thumb must not change its gain");
        ui.touch(TouchInputPhase::Move, 183, y - 18);
        ui.touch(TouchInputPhase::Up, 183, y - 18);
        assert_eq!(ui.settings.bands_half_db[0], 12);
        assert_eq!(ui.settings.bands_half_db[1], 7);
        assert!(ui.drag_offset.is_none());

        ui.dirty = false;
        let x = (preamp_x(-12) + 10) as u16;
        ui.touch(TouchInputPhase::Down, x, PREAMP_Y as u16);
        assert_eq!(ui.settings.preamp_half_db, -12);
        assert!(!ui.dirty);
        ui.touch(TouchInputPhase::Up, x + 23, PREAMP_Y as u16);
        assert_eq!(ui.settings.preamp_half_db, -8);
        ui.touch(TouchInputPhase::Move, 0, PREAMP_Y as u16);
        assert_eq!(ui.settings.preamp_half_db, -8, "release ends capture");
    }

    #[test]
    fn header_actions_do_not_navigate_back_and_require_release_on_the_same_action() {
        let mut ui = EqualizerUi::new(EqualizerSettings {
            bands_half_db: [8; 10],
            preamp_half_db: -12,
            ..Default::default()
        });
        ui.touch(TouchInputPhase::Down, 710, 44);
        assert!(!ui.touch(TouchInputPhase::Up, 710, 44));
        assert!(ui.settings.enabled);
        assert_eq!(ui.settings.bands_half_db, [8; 10]);
        ui.touch(TouchInputPhase::Down, 500, 44);
        assert!(!ui.touch(TouchInputPhase::Up, 60, 44));
        assert_eq!(ui.settings.bands_half_db, [8; 10]);
        ui.touch(TouchInputPhase::Down, 500, 44);
        assert!(!ui.touch(TouchInputPhase::Up, 500, 44));
        assert_eq!(ui.settings.bands_half_db, [0; 10]);
        assert_eq!(ui.settings.preamp_half_db, 0);
        assert!(ui.settings.enabled);
        ui.touch(TouchInputPhase::Down, 60, 44);
        assert!(!ui.touch(TouchInputPhase::Up, 710, 44));
        assert!(ui.settings.enabled);
    }

    #[test]
    fn touch_bands_are_distinct_and_clamped_and_reset_keeps_enabled() {
        let mut ui = EqualizerUi::new(Default::default());
        for index in 0..10 {
            let x = (BAND_LEFT + index * BAND_WIDTH + BAND_WIDTH / 2) as u16;
            ui.touch(TouchInputPhase::Down, x, 250);
            ui.touch(TouchInputPhase::Move, x, 0);
            ui.touch(TouchInputPhase::Up, x, 0);
            assert_eq!(ui.settings.bands_half_db[index as usize], MAX_BAND_HALF_DB);
        }
        assert_eq!(band_gain(999), MIN_BAND_HALF_DB);
        assert_eq!(preamp_gain(-999), MIN_PREAMP_HALF_DB);
        assert_eq!(preamp_gain(999), MAX_PREAMP_HALF_DB);
        ui.focus = TOGGLE;
        ui.button(KnobInputButton::Select);
        ui.focus = RESET;
        ui.button(KnobInputButton::Select);
        assert_eq!(ui.settings.bands_half_db, [0; 10]);
        assert!(ui.settings.enabled);
    }
    #[test]
    fn knob_edits_half_db_and_back_finishes_before_leaving() {
        let mut ui = EqualizerUi::new(Default::default());
        assert_eq!(ui.focus, TOGGLE, "the page opens on the on/off toggle");
        ui.rotary(RotaryDirection::CounterClockwise, 1);
        assert_eq!(ui.focus, RESET, "reset sits before the toggle");
        ui.rotary(RotaryDirection::Clockwise, 100);
        assert_eq!(ui.focus, PREAMP, "preamp ends the list");
        ui.focus_toggle();
        ui.rotary(RotaryDirection::Clockwise, 1);
        ui.button(KnobInputButton::Select);
        ui.rotary(RotaryDirection::Clockwise, 1);
        assert_eq!(ui.settings.bands_half_db[0], 1);
        assert!(!ui.button(KnobInputButton::Back));
        assert!(ui.button(KnobInputButton::Back));
    }
    #[test]
    fn save_failure_retains_editable_curve_and_success_saves_latest_gesture() {
        let directory = tempfile::TempDir::new().expect("directory");
        let invalid = directory.path().join("file");
        fs::write(&invalid, "file blocks directory").expect("file");
        let mut ui = EqualizerUi::new(Default::default());
        ui.touch(TouchInputPhase::Down, 117, 250);
        ui.touch(TouchInputPhase::Move, 117, 180);
        assert!(
            !directory.path().join("settings.toml").exists(),
            "motion must not save"
        );
        let edited = ui.settings;
        assert!(ui.persist(&invalid, Default::default()).is_err());
        assert_eq!(ui.settings, edited);
        assert!(ui.dirty);
        ui.touch(TouchInputPhase::Up, 117, 180);
        let saved = ui
            .persist(directory.path(), Default::default())
            .expect("save release");
        assert!(!ui.dirty);
        assert_eq!(saved.equalizer, edited);
        assert_eq!(
            RuntimeSettings::load(directory.path(), Default::default())
                .expect("load")
                .equalizer,
            edited
        );
        ui.touch(TouchInputPhase::Down, 117, 250);
        ui.cancel_interaction();
        let before = ui.settings;
        ui.touch(TouchInputPhase::Move, 117, 180);
        assert_eq!(ui.settings, before, "reset cancels drag");
    }

    #[test]
    fn release_uses_final_slider_position_and_back_requires_a_header_press() {
        let mut ui = EqualizerUi::new(Default::default());
        assert!(!ui.touch(TouchInputPhase::Up, 60, 30));
        ui.touch(TouchInputPhase::Down, 117, 250);
        assert!(!ui.touch(TouchInputPhase::Up, 117, TRACK_BOTTOM as u16));
        assert_eq!(ui.settings.bands_half_db[0], MIN_BAND_HALF_DB);
        ui.touch(TouchInputPhase::Down, 60, 30);
        assert!(ui.touch(TouchInputPhase::Up, 60, 30));
    }

    #[test]
    fn frame_is_valid_nv12_and_can_be_reviewed() {
        let mut ui = EqualizerUi::new(EqualizerSettings {
            enabled: true,
            bands_half_db: [2, 4, 6, 0, -2, -4, 0, 4, 2, 0],
            preamp_half_db: -12,
        });
        let fonts = FontSet::load();
        for (name, focus, editing, enabled) in [
            ("focused", FIRST_BAND + 3, false, true),
            ("editing", FIRST_BAND + 3, true, true),
            ("off", TOGGLE, false, false),
            ("preamp", PREAMP, true, true),
            ("phone-audio", FIRST_BAND + 9, false, true),
            ("unavailable", RESET, false, true),
            ("save-error", FIRST_BAND, false, true),
            ("fallback-font", FIRST_BAND + 9, false, true),
        ] {
            ui.focus = focus;
            ui.editing = editing;
            ui.settings.enabled = enabled;
            let fallback = FontSet {
                regular: None,
                semibold: None,
            };
            let fonts = if name == "fallback-font" {
                &fallback
            } else {
                &fonts
            };
            let mut canvas = Canvas::new();
            canvas.scroll(0, VIEWPORT_TOP);
            ui.render(
                &mut canvas,
                fonts,
                name == "phone-audio",
                if name == "unavailable" {
                    prelude_media::EqualizerAvailability::Unavailable
                } else {
                    prelude_media::EqualizerAvailability::Available
                },
                (name == "save-error").then_some("Unable to save settings"),
            );
            canvas.scroll(0, 0);
            canvas.chevron(BACK_X, TITLE_Y, 12, false, WHITE, 4.0);
            canvas.text(
                fonts,
                "Equalizer",
                BACK_TITLE_X,
                TITLE_Y,
                TITLE_SIZE,
                WHITE,
                true,
            );
            ui.render_header(&mut canvas, fonts);
            assert_eq!(
                canvas.to_nv12().len(),
                DISPLAY_WIDTH * DISPLAY_HEIGHT * 3 / 2
            );
            if let Ok(path) = std::env::var("PRELUDE_EQ_PREVIEW") {
                let path = PathBuf::from(path);
                let path = if name == "focused" {
                    path
                } else {
                    let stem = path.file_stem().expect("preview file").to_string_lossy();
                    path.with_file_name(format!("{stem}-{name}.ppm"))
                };
                let mut data = format!("P6\n{DISPLAY_WIDTH} {DISPLAY_HEIGHT}\n255\n").into_bytes();
                data.extend(&canvas.rgb);
                fs::write(path, data).expect("write preview");
            }
        }
    }
}
