//! Settings persistence.
//!
//! Kept deliberately small: a single JSON file in the platform config
//! directory, written on every change. No database, no schema migration — a
//! corrupt or unreadable file falls back to defaults instead of failing to
//! start, because losing a slider position must never be worse than a reset.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::timing;

/// User preferences, mirrored to the frontend as camelCase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub wpm: u16,
    pub start_delay_secs: u8,
    pub abort_on_focus_change: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            wpm: timing::WPM_DEFAULT,
            start_delay_secs: 3,
            abort_on_focus_change: true,
        }
    }
}

impl Settings {
    /// Forces every field into its supported range.
    ///
    /// The file on disk is user-editable, so values are validated on read as
    /// well as on write.
    pub fn clamped(self) -> Self {
        Self {
            wpm: self.wpm.clamp(timing::WPM_MIN, timing::WPM_MAX),
            start_delay_secs: self.start_delay_secs.min(15),
            abort_on_focus_change: self.abort_on_focus_change,
        }
    }
}

/// Where settings live: `<config_dir>/<bundle identifier>/settings.json`.
pub fn default_path(config_dir: Option<PathBuf>) -> Option<PathBuf> {
    config_dir.map(|dir| dir.join("settings.json"))
}

/// Loads settings, falling back to defaults for anything unreadable.
pub fn load(path: Option<&Path>) -> Settings {
    let Some(path) = path else {
        return Settings::default();
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Settings::default();
    };
    match serde_json::from_str::<Settings>(&raw) {
        Ok(s) => s.clamped(),
        Err(_) => Settings::default(),
    }
}

/// Writes settings, replacing the file atomically.
///
/// The write goes to a sibling temp file first so a crash mid-write cannot
/// leave a truncated settings file behind.
pub fn save(path: Option<&Path>, settings: Settings) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create config dir: {e}"))?;
    }
    let json = serde_json::to_string_pretty(&settings).map_err(|e| format!("encode: {e}"))?;

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("replace {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ghostwriter-settings-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn defaults_match_the_documented_values() {
        let s = Settings::default();
        assert_eq!(s.wpm, 60);
        assert_eq!(s.start_delay_secs, 3);
        assert!(s.abort_on_focus_change);
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("settings.json");
        let wanted = Settings {
            wpm: 74,
            start_delay_secs: 5,
            abort_on_focus_change: false,
        };

        save(Some(&path), wanted).expect("save");
        assert_eq!(load(Some(&path)), wanted);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_yields_defaults() {
        let path = temp_dir("missing").join("settings.json");
        assert_eq!(load(Some(&path)), Settings::default());
    }

    #[test]
    fn corrupt_json_yields_defaults_instead_of_panicking() {
        let dir = temp_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{ this is not json").unwrap();

        // Losing the slider position must never prevent the app from starting.
        assert_eq!(load(Some(&path)), Settings::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_range_values_are_clamped_on_load() {
        let dir = temp_dir("clamp");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"wpm": 500, "startDelaySecs": 99, "abortOnFocusChange": true}"#,
        )
        .unwrap();

        let loaded = load(Some(&path));
        assert_eq!(loaded.wpm, timing::WPM_MAX, "wpm must be clamped");
        assert_eq!(loaded.start_delay_secs, 15, "delay must be clamped");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_wpm_is_clamped_up() {
        let dir = temp_dir("zero");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"wpm": 0, "startDelaySecs": 3, "abortOnFocusChange": true}"#,
        )
        .unwrap();

        assert_eq!(load(Some(&path)).wpm, timing::WPM_MIN);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_missing_directories() {
        let dir = temp_dir("nested");
        let path = dir.join("a/b/c/settings.json");
        save(Some(&path), Settings::default()).expect("save into nested dirs");
        assert!(path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_path_is_a_no_op_rather_than_an_error() {
        // A platform without a config directory must not break the app.
        assert_eq!(load(None), Settings::default());
        assert!(save(None, Settings::default()).is_ok());
    }

    #[test]
    fn the_written_file_is_human_readable_camel_case() {
        let dir = temp_dir("format");
        let path = dir.join("settings.json");
        save(Some(&path), Settings::default()).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"wpm\""));
        assert!(
            raw.contains("\"startDelaySecs\""),
            "expected camelCase: {raw}"
        );
        assert!(raw.contains("\"abortOnFocusChange\""));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_leftover_temp_file_does_not_break_loading() {
        let dir = temp_dir("tmp");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        save(Some(&path), Settings::default()).unwrap();
        // Simulate a crash that left the staging file behind.
        std::fs::write(dir.join("settings.json.tmp"), "garbage").unwrap();

        assert_eq!(load(Some(&path)), Settings::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_default_path_lives_under_the_app_config_dir() {
        let path = default_path(Some(PathBuf::from("/tmp/cfg"))).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/cfg/settings.json"));
        assert!(default_path(None).is_none());
    }
}
