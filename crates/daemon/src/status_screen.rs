use std::fs;

use prelude_media::{SCREEN_HEIGHT, SCREEN_WIDTH};

use crate::{
    DisplayPlatform, DisplayStatus,
    fonts::{self, FontSet},
};

const WIDTH: usize = SCREEN_WIDTH as usize;
const HEIGHT: usize = SCREEN_HEIGHT as usize;
const FRAME_BYTES: usize = WIDTH * HEIGHT * 3 / 2;
const CENTER_X: i32 = WIDTH as i32 / 2;
const MAXIMUM_TEXT_WIDTH: i32 = WIDTH as i32 - 80;
const HEADLINE_Y: i32 = 349;
const HEADLINE_SIZE: f32 = 20.0;
const HEADLINE_LUMA: u8 = 235;
const DETAIL_Y: i32 = 381;
const DETAIL_SIZE: f32 = 17.0;
const DETAIL_LUMA: u8 = 146;

#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub(crate) fn render(platform: DisplayPlatform, status: DisplayStatus) -> Result<Vec<u8>, String> {
    let path = platform.logo_path();
    let mut frame = fs::read(path).map_err(|error| format!("reading {path} failed: {error}"))?;
    if frame.len() != FRAME_BYTES {
        return Err(format!("{path} is not one {WIDTH}x{HEIGHT} NV12 frame"));
    }
    draw_status(&mut frame, fonts::shared(), status);
    Ok(frame)
}

fn draw_status(frame: &mut [u8], fonts: &FontSet, status: DisplayStatus) {
    let luma = &mut frame[..WIDTH * HEIGHT];
    draw_line(
        luma,
        fonts,
        status.headline(),
        HEADLINE_Y,
        HEADLINE_SIZE,
        HEADLINE_LUMA,
    );
    if let Some(detail) = status.detail() {
        draw_line(luma, fonts, &detail, DETAIL_Y, DETAIL_SIZE, DETAIL_LUMA);
    }
}

fn draw_line(luma: &mut [u8], fonts: &FontSet, text: &str, center_y: i32, size: f32, value: u8) {
    let text = fonts.fit(text, size, false, MAXIMUM_TEXT_WIDTH);
    let x = CENTER_X - fonts.text_width(&text, size, false) / 2;
    fonts.draw(&text, (x, center_y), size, false, |px, py, alpha| {
        if !(0..WIDTH as i32).contains(&px) || !(0..HEIGHT as i32).contains(&py) {
            return;
        }
        let pixel = &mut luma[py as usize * WIDTH + px as usize];
        let alpha = u16::from(alpha);
        *pixel = ((u16::from(*pixel) * (255 - alpha) + u16::from(value) * alpha + 127) / 255) as u8;
    });
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::ConnectionStep;

    fn logo(platform: DisplayPlatform) -> Vec<u8> {
        let name = Path::new(platform.logo_path())
            .file_name()
            .expect("logo file name");
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../image/meta-prelude/recipes-core/prelude/files")
            .join(name);
        fs::read(path).expect("packaged logo frame")
    }

    fn changed_rows(before: &[u8], after: &[u8]) -> Vec<usize> {
        let mut rows: Vec<usize> = before
            .iter()
            .zip(after)
            .enumerate()
            .filter(|(_, (before, after))| before != after)
            .map(|(index, _)| index / WIDTH)
            .collect();
        rows.dedup();
        rows
    }

    #[test]
    fn packaged_logos_are_text_free_frames() {
        for platform in [DisplayPlatform::CarPlay, DisplayPlatform::AndroidAuto] {
            let frame = logo(platform);
            assert_eq!(frame.len(), FRAME_BYTES);
            let background = frame[0];
            assert!(
                frame[300 * WIDTH..HEIGHT * WIDTH]
                    .iter()
                    .all(|&luma| luma == background),
                "{platform:?} logo has content where status text is drawn"
            );
        }
    }

    #[test]
    fn text_only_touches_luma_below_the_logo() {
        let fonts = fonts::shared();
        let base = logo(DisplayPlatform::CarPlay);
        let mut frame = base.clone();
        draw_status(&mut frame, fonts, DisplayStatus::WaitingForPhone);
        let rows = changed_rows(&base, &frame);
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|row| (330..370).contains(row)), "{rows:?}");

        let mut frame = base.clone();
        draw_status(
            &mut frame,
            fonts,
            DisplayStatus::Connecting(Some(ConnectionStep::SecuringConnection)),
        );
        let rows = changed_rows(&base, &frame);
        assert!(rows.iter().any(|&row| row > 370));
        assert!(rows.iter().all(|&row| row < HEIGHT));
    }

    #[test]
    fn every_status_renders_a_full_frame() {
        let preview = std::env::var("PRELUDE_STATUS_PREVIEW")
            .ok()
            .map(PathBuf::from);
        let statuses = [
            ("waiting", DisplayStatus::WaitingForPhone),
            ("unlock", DisplayStatus::UnlockPhone),
            ("connecting", DisplayStatus::Connecting(None)),
            (
                "securing",
                DisplayStatus::Connecting(Some(ConnectionStep::SecuringConnection)),
            ),
            (
                "join-wifi",
                DisplayStatus::Connecting(Some(ConnectionStep::WaitingForIphoneOnWifi)),
            ),
            ("pairing-code", DisplayStatus::ConfirmPairing(48_213)),
            ("connected", DisplayStatus::Connected),
            ("missing-audio", DisplayStatus::MissingAudioOutput),
        ];
        for platform in [DisplayPlatform::CarPlay, DisplayPlatform::AndroidAuto] {
            for (name, status) in statuses {
                let mut frame = logo(platform);
                draw_status(&mut frame, fonts::shared(), status);
                assert_eq!(frame.len(), FRAME_BYTES);
                if let Some(directory) = preview.as_ref() {
                    fs::create_dir_all(directory).expect("preview directory");
                    let platform = match platform {
                        DisplayPlatform::CarPlay => "carplay",
                        DisplayPlatform::AndroidAuto => "android-auto",
                    };
                    fs::write(directory.join(format!("{platform}-{name}.nv12")), frame)
                        .expect("write preview");
                }
            }
        }
    }
}
