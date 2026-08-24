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

/// Which palette to paint, when the system's own answer is not wanted.
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub language: Language,
    pub theme: ThemeChoice,
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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: Language::English,
            theme: ThemeChoice::System,
            motion: MotionChoice::System,
            preview_limit_mb: 8,
            bandwidth_limit: 0,
            job_concurrency: 2,
            check_updates: true,
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
        };
        store.save(&settings).unwrap();
        assert_eq!(store.load(), settings);

        // A file written by an older build has fewer keys. Every one of them
        // has to fall back rather than throwing the whole file away, or an
        // upgrade silently resets everything the user chose.
        std::fs::write(store.path(), r#"{"theme":"dark"}"#).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.theme, ThemeChoice::Dark);
        assert_eq!(loaded.language, Settings::default().language);
        assert_eq!(loaded.motion, Settings::default().motion);
        assert_eq!(
            loaded.preview_limit_mb,
            Settings::default().preview_limit_mb
        );
        assert_eq!(loaded.check_updates, Settings::default().check_updates);

        _ = std::fs::remove_dir_all(&dir);
    }
}
