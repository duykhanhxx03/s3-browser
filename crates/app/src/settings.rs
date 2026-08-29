//! What the user can change about the app itself.
//!
//! Kept apart from profiles: a profile is a place to connect to, these are
//! preferences about this machine. They live in their own file so that copying
//! a profile list between machines does not drag someone else's theme with it.
//!
//! A missing or unreadable file is the defaults rather than an error. Refusing
//! to start over a preferences file is not a trade anybody wants.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::locale::{self, Language};

/// Accent palettes, sourced from popular Color Hunt palettes and kept local so
/// startup never depends on the network.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ColorPalette {
    #[default]
    Default,
    CharcoalTeal,
    MintGlass,
    CloudBlue,
    CoralCream,
    LavenderNight,
    AquaPunch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomColorPalette {
    pub label: String,
    pub colors: [u32; 4],
}

impl CustomColorPalette {
    pub fn new(label: impl Into<String>, colors: [u32; 4]) -> Self {
        Self {
            label: label.into(),
            colors,
        }
    }
}

impl ColorPalette {
    pub const ALL: [ColorPalette; 7] = [
        ColorPalette::Default,
        ColorPalette::CharcoalTeal,
        ColorPalette::MintGlass,
        ColorPalette::CloudBlue,
        ColorPalette::CoralCream,
        ColorPalette::LavenderNight,
        ColorPalette::AquaPunch,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ColorPalette::Default => "Default",
            ColorPalette::CharcoalTeal => "Charcoal Teal",
            ColorPalette::MintGlass => "Mint Glass",
            ColorPalette::CloudBlue => "Cloud Blue",
            ColorPalette::CoralCream => "Coral Cream",
            ColorPalette::LavenderNight => "Lavender Night",
            ColorPalette::AquaPunch => "Aqua Punch",
        }
    }

    pub fn colors(self) -> [u32; 4] {
        match self {
            ColorPalette::Default => [0x0e0f12, 0xffffff, 0x3b82f6, 0xe5484d],
            ColorPalette::CharcoalTeal => [0x222831, 0x393e46, 0x00adb5, 0xeeeeee],
            ColorPalette::MintGlass => [0xe3fdfd, 0xcbf1f5, 0xa6e3e9, 0x71c9ce],
            ColorPalette::CloudBlue => [0xf9f7f7, 0xdbe2ef, 0x3f72af, 0x112d4e],
            ColorPalette::CoralCream => [0xfff5e4, 0xffe3e1, 0xffd1d1, 0xff9494],
            ColorPalette::LavenderNight => [0xf4eeff, 0xdcd6f7, 0xa6b1e1, 0x424874],
            ColorPalette::AquaPunch => [0x08d9d6, 0x252a34, 0xff2e63, 0xeaeaea],
        }
    }

    pub fn accent(self) -> Option<u32> {
        match self {
            ColorPalette::Default => None,
            ColorPalette::CharcoalTeal => Some(0x00adb5),
            ColorPalette::MintGlass => Some(0x71c9ce),
            ColorPalette::CloudBlue => Some(0x3f72af),
            ColorPalette::CoralCream => Some(0xff9494),
            ColorPalette::LavenderNight => Some(0x6f7fc8),
            ColorPalette::AquaPunch => Some(0xff2e63),
        }
    }
}

/// Which light/dark mode to paint, when the system's own answer is not wanted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    /// Follow the system, and keep following it when it changes.
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark];

    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => locale::text("theme.system"),
            ThemeChoice::Light => locale::text("theme.light"),
            ThemeChoice::Dark => locale::text("theme.dark"),
        }
    }
}

/// How much motion the interface should use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MotionChoice {
    /// Follow the operating system accessibility setting.
    #[default]
    System,
    Full,
    Reduced,
}

impl MotionChoice {
    pub const ALL: [MotionChoice; 3] = [
        MotionChoice::System,
        MotionChoice::Full,
        MotionChoice::Reduced,
    ];

    pub fn label(self) -> &'static str {
        match self {
            MotionChoice::System => locale::text("motion.system"),
            MotionChoice::Full => locale::text("motion.full"),
            MotionChoice::Reduced => locale::text("motion.reduced"),
        }
    }
}

/// How many megabytes of an object a preview may pull down.
///
/// A cap rather than a promise: the point of a preview is deciding whether this
/// is the right file, and paying for a gigabyte to find out is the opposite of
/// that.
pub const PREVIEW_LIMITS_MB: [u32; 3] = [1, 8, 32];

/// How many transfers run at once. More is not always faster — past a handful
/// the provider starts throttling — but on a fat link it is.
pub const JOB_CONCURRENCY_CHOICES: [usize; 4] = [1, 2, 4, 8];

/// Row heights the object list steps through, smallest first.
///
/// Discrete steps rather than a free-running scale: the row holds text at a
/// fixed size, and a continuous zoom spends most of its range on heights that
/// only add padding around it. Seven stops covers "fit as much on screen as
/// possible" through to "readable across the room".
pub const LIST_ROW_HEIGHTS: [f32; 7] = [22., 25., 28., 32., 38., 45., 54.];

/// The step the list starts on, which is the height it had before this was
/// adjustable.
pub const LIST_ZOOM_DEFAULT: usize = 2;

/// One stop on the grid's zoom.
///
/// Columns and heights travel together deliberately. A tile is `flex_1`, so
/// its width is the pane divided by the column count — dropping to three
/// columns without growing the row would draw wide, flat letterboxes rather
/// than bigger thumbnails.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridStep {
    pub columns: usize,
    pub row_height: f32,
    pub media_height: f32,
}

/// Grid stops, smallest tile first. Every row is its thumbnail area plus the
/// 100px of checkbox, name and size that do not scale with it.
pub const GRID_STEPS: [GridStep; 6] = [
    GridStep {
        columns: 8,
        row_height: 168.,
        media_height: 68.,
    },
    GridStep {
        columns: 7,
        row_height: 179.,
        media_height: 79.,
    },
    GridStep {
        columns: 6,
        row_height: 193.,
        media_height: 93.,
    },
    GridStep {
        columns: 5,
        row_height: 212.,
        media_height: 112.,
    },
    GridStep {
        columns: 4,
        row_height: 242.,
        media_height: 142.,
    },
    GridStep {
        columns: 3,
        row_height: 294.,
        media_height: 194.,
    },
];

/// The step the grid starts on: five columns, as it was before this was
/// adjustable.
pub const GRID_ZOOM_DEFAULT: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub language: Language,
    pub theme: ThemeChoice,
    pub color_palette: ColorPalette,
    pub custom_color_palette: Option<CustomColorPalette>,
    pub motion: MotionChoice,
    pub preview_limit_mb: u32,
    /// Bytes per second, zero for unlimited.
    pub bandwidth_limit: u64,
    pub job_concurrency: usize,
    /// Whether to ask GitHub for a newer release on launch.
    ///
    /// On by default, and a real setting rather than a constant: a check is a
    /// network request to a third party that the user did not initiate, and
    /// someone on a metered link or an air-gapped machine is entitled to turn
    /// it off. Turning it off does not disable the manual command - asking on
    /// purpose is a different act from being asked on every launch.
    pub check_updates: bool,
    /// Which step of [`LIST_ROW_HEIGHTS`] the object list is drawn at.
    ///
    /// Persisted rather than reset each launch: someone who zoomed out to fit
    /// a long listing on screen meant it, and having to do it again every time
    /// the app opens is the opposite of a preference.
    pub list_zoom: usize,
    /// Which step of [`GRID_STEPS`] the grid is drawn at.
    pub grid_zoom: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: Language::English,
            theme: ThemeChoice::System,
            color_palette: ColorPalette::Default,
            custom_color_palette: None,
            motion: MotionChoice::System,
            preview_limit_mb: 8,
            bandwidth_limit: 0,
            job_concurrency: 2,
            check_updates: true,
            list_zoom: LIST_ZOOM_DEFAULT,
            grid_zoom: GRID_ZOOM_DEFAULT,
        }
    }
}

impl Settings {
    /// The preview cap in bytes, clamped to something a window can hold.
    ///
    /// Clamped on read rather than trusted: the file is editable by hand, and a
    /// zero there would make every preview say "unsupported" with no way to see
    /// why from inside the app.
    pub fn preview_limit_bytes(&self) -> u64 {
        let mb = self.preview_limit_mb.clamp(1, 256) as u64;
        mb * 1024 * 1024
    }

    pub fn job_concurrency(&self) -> usize {
        self.job_concurrency.clamp(1, 16)
    }

    /// The list's row height, for the step it is on.
    ///
    /// Clamped on read for the same reason the preview cap is: the file is
    /// editable by hand, and an index past the end of the table would be a
    /// panic rather than a mistake anyone could see and correct.
    pub fn list_row_height(&self) -> f32 {
        LIST_ROW_HEIGHTS[self.list_zoom.min(LIST_ROW_HEIGHTS.len() - 1)]
    }

    pub fn grid_step(&self) -> GridStep {
        GRID_STEPS[self.grid_zoom.min(GRID_STEPS.len() - 1)]
    }
}

pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    /// Beside the profile list, in the platform config directory.
    pub fn beside(profiles: &Path) -> Self {
        Self {
            path: profiles.with_file_name("settings.json"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Settings {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, settings: &Settings) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(settings)?;
        std::fs::write(&self.path, text).with_context(|| format!("writing {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hand_edited_file_cannot_break_the_app() {
        // The file is plain JSON in a folder people open. A zero here would
        // make every preview report "unsupported" with nothing inside the app
        // to explain why.
        let settings = Settings {
            preview_limit_mb: 0,
            job_concurrency: 0,
            ..Default::default()
        };
        assert_eq!(settings.preview_limit_bytes(), 1024 * 1024);
        assert_eq!(settings.job_concurrency(), 1);

        // And an absurd one does not turn a preview into a download.
        let settings = Settings {
            preview_limit_mb: 100_000,
            job_concurrency: 5_000,
            ..Default::default()
        };
        assert_eq!(settings.preview_limit_bytes(), 256 * 1024 * 1024);
        assert_eq!(settings.job_concurrency(), 16);

        // A zoom step past the end of its table would index out of bounds,
        // which is a panic rather than something the user could see and undo.
        let settings = Settings {
            list_zoom: 99,
            grid_zoom: 99,
            ..Default::default()
        };
        assert_eq!(
            settings.list_row_height(),
            LIST_ROW_HEIGHTS[LIST_ROW_HEIGHTS.len() - 1]
        );
        assert_eq!(settings.grid_step(), GRID_STEPS[GRID_STEPS.len() - 1]);
    }

    #[test]
    fn the_default_zoom_is_the_size_the_app_had_before_it_was_adjustable() {
        // Anyone upgrading has no zoom in their settings file, so the default
        // has to land on what they were already looking at rather than
        // silently resizing every listing on first launch.
        let settings = Settings::default();
        assert_eq!(settings.list_row_height(), 28.);
        assert_eq!(settings.grid_step().columns, 5);
        assert_eq!(settings.grid_step().row_height, 212.);
        assert_eq!(settings.grid_step().media_height, 112.);
    }

    #[test]
    fn every_grid_step_keeps_the_chrome_the_tile_cannot_scale() {
        // A tile is its thumbnail plus a checkbox row, a name and a size line.
        // Those hold a fixed text size, so they cost the same 100px at every
        // step — a table that forgets this crops the name at one end of the
        // range and leaves a gap at the other.
        for step in GRID_STEPS {
            assert_eq!(
                step.row_height - step.media_height,
                100.,
                "step {step:?} does not leave room for the fixed rows"
            );
        }

        // Wider tiles, taller rows: the two have to move together or fewer
        // columns just draws letterboxes.
        for pair in GRID_STEPS.windows(2) {
            assert!(pair[0].columns > pair[1].columns, "{pair:?}");
            assert!(pair[0].media_height < pair[1].media_height, "{pair:?}");
        }
    }

    #[test]
    fn a_missing_or_broken_file_is_the_defaults() {
        let dir = std::env::temp_dir().join(format!("s3b-settings-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        let store = SettingsStore::beside(&dir.join("profiles.json"));

        // Never opened before.
        assert_eq!(store.load(), Settings::default());

        // And a file someone edited into nonsense: refusing to start over a
        // preferences file is not a trade anybody wants.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(store.path(), "{ not json").unwrap();
        assert_eq!(store.load(), Settings::default());

        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_round_trip_and_survive_a_missing_field() {
        let dir = std::env::temp_dir().join(format!("s3b-settings-rt-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        let store = SettingsStore::beside(&dir.join("profiles.json"));

        let settings = Settings {
            language: Language::Vietnamese,
            theme: ThemeChoice::Dark,
            motion: MotionChoice::Reduced,
            preview_limit_mb: 32,
            bandwidth_limit: 5_000_000,
            job_concurrency: 4,
            // Not the default, so the round trip below proves the field is
            // written and read rather than quietly falling back both times.
            check_updates: false,
            color_palette: ColorPalette::CloudBlue,
            custom_color_palette: Some(CustomColorPalette::new(
                "Color Hunt demo",
                [0x222831, 0x393e46, 0x00adb5, 0xeeeeee],
            )),
            list_zoom: 5,
            grid_zoom: 1,
        };
        store.save(&settings).unwrap();
        assert_eq!(store.load(), settings);

        // A file written by an older build has fewer keys. Every one of them
        // has to fall back rather than throwing the whole file away, or an
        // upgrade silently resets everything the user chose.
        std::fs::write(store.path(), r#"{"theme":"dark"}"#).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.theme, ThemeChoice::Dark);
        assert_eq!(loaded.color_palette, Settings::default().color_palette);
        assert_eq!(
            loaded.custom_color_palette,
            Settings::default().custom_color_palette
        );
        assert_eq!(loaded.language, Settings::default().language);
        assert_eq!(loaded.motion, Settings::default().motion);
        assert_eq!(
            loaded.preview_limit_mb,
            Settings::default().preview_limit_mb
        );
        assert_eq!(loaded.check_updates, Settings::default().check_updates);
        assert_eq!(loaded.list_zoom, Settings::default().list_zoom);
        assert_eq!(loaded.grid_zoom, Settings::default().grid_zoom);

        _ = std::fs::remove_dir_all(&dir);
    }
}
