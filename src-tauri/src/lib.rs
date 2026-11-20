//! Tauri glue: state, IPC commands and global shortcuts.
//!
//! The UI is deliberately thin. Everything that can fail slowly (reading the
//! clipboard, injecting keystrokes, polling for the focused window) happens in
//! Rust; the webview only renders state and forwards button presses.

#[doc(hidden)]
pub mod engine;
#[doc(hidden)]
pub mod settings;
#[doc(hidden)]
pub mod timing;
#[doc(hidden)]
pub mod typer;
mod window;

use std::path::PathBuf;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut, ShortcutState};

use engine::{EngineConfig, EngineHandle, Progress};
use settings::Settings;

const EVENT_PROGRESS: &str = "engine://progress";

/* --------------------------------- types -------------------------------- */

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformInfo {
    os: &'static str,
    /// Set when the session limits keystroke injection (Wayland, no display).
    hint: Option<&'static str>,
    /// False where the OS gives no portable way to read the focused window.
    can_track_focus: bool,
    shortcuts: Shortcuts,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Shortcuts {
    pause_resume: &'static str,
    stop: &'static str,
}

/// Everything the UI needs to render, sent on every engine update.
#[derive(Clone, Serialize)]
struct Snapshot {
    phase: &'static str,
    done: usize,
    total: usize,
    error: Option<String>,
    /// Whether the UI should offer "Continue from character N".
    resumable: bool,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            phase: "idle",
            done: 0,
            total: 0,
            error: None,
            resumable: false,
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsDto {
    wpm: u16,
    start_delay_secs: u8,
    abort_on_focus_change: bool,
}

impl From<Settings> for SettingsDto {
    fn from(s: Settings) -> Self {
        Self {
            wpm: s.wpm,
            start_delay_secs: s.start_delay_secs,
            abort_on_focus_change: s.abort_on_focus_change,
        }
    }
}

impl From<SettingsDto> for Settings {
    fn from(d: SettingsDto) -> Self {
        Settings {
            wpm: d.wpm,
            start_delay_secs: d.start_delay_secs,
            abort_on_focus_change: d.abort_on_focus_change,
        }
    }
}

pub struct AppState {
    engine: Mutex<Option<EngineHandle>>,
    snapshot: Mutex<Snapshot>,
    settings: Mutex<Settings>,
    /// Where settings are persisted; `None` disables persistence.
    settings_path: Option<PathBuf>,
}

impl AppState {
    fn new(settings_path: Option<PathBuf>) -> Self {
        let settings = settings::load(settings_path.as_deref());
        Self {
            engine: Mutex::new(None),
            snapshot: Mutex::new(Snapshot::default()),
            settings: Mutex::new(settings),
            settings_path,
        }
    }

    fn publish(&self, snapshot: Snapshot) {
        *self.snapshot.lock().unwrap() = snapshot;
    }

    fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().unwrap().clone()
    }

    fn running(&self) -> bool {
        matches!(
            self.snapshot.lock().unwrap().phase,
            "countdown" | "typing" | "paused"
        )
    }

    /// Stores validated settings and writes them to disk.
    ///
    /// A write failure is surfaced so the user learns their preferences will not
    /// survive a restart, but it does not undo the in-memory change.
    fn store_settings(&self, settings: Settings) -> Result<Settings, String> {
        let clamped = settings.clamped();
        *self.settings.lock().unwrap() = clamped;
        settings::save(self.settings_path.as_deref(), clamped)?;
        Ok(clamped)
    }
}

/* ------------------------------- shortcuts ------------------------------ */

fn pause_shortcut() -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyP)
}

fn stop_shortcut() -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyS)
}

/* -------------------------------- commands ------------------------------ */

#[tauri::command]
fn platform_info() -> PlatformInfo {
    PlatformInfo {
        os: std::env::consts::OS,
        hint: typer::platform_hint(),
        // Windows always has the API; on Linux, ask the real probe because a
        // Wayland session may expose the OS but not the focused window.
        can_track_focus: cfg!(target_os = "windows") || window::active_id().is_some(),
        shortcuts: Shortcuts {
            pause_resume: "Ctrl+Shift+P",
            stop: "Ctrl+Shift+S",
        },
    }
}

#[tauri::command]
fn read_clipboard() -> Result<String, String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard.get_text().map_err(|e| match e {
        arboard::Error::ContentNotAvailable => {
            "A área de transferência não contém texto.".to_string()
        }
        other => other.to_string(),
    })
}

#[tauri::command]
fn active_window() -> Option<String> {
    window::active_description()
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> SettingsDto {
    (*state.settings.lock().unwrap()).into()
}

#[tauri::command]
fn update_settings(
    state: State<'_, AppState>,
    settings: SettingsDto,
) -> Result<SettingsDto, String> {
    state.store_settings(settings.into()).map(Into::into)
}

#[tauri::command]
fn status(state: State<'_, AppState>) -> Snapshot {
    state.snapshot()
}

#[tauri::command]
fn start(
    app: AppHandle,
    state: State<'_, AppState>,
    text: String,
    resume_from: usize,
) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("Cole ou digite algum texto primeiro.".into());
    }

    // A new run always replaces the previous one.
    if let Some(previous) = state.engine.lock().unwrap().take() {
        previous.stop();
    }

    let settings = *state.settings.lock().unwrap();

    // Always captured: the engine uses it to wait for the destination before
    // the first keystroke and to restore focus on Retomar. The checkbox only
    // decides whether a third window stops the run.
    let own_window = window::active_id();

    let config = EngineConfig {
        text,
        wpm: settings.wpm,
        start_delay: Duration::from_secs(settings.start_delay_secs as u64),
        abort_on_focus_change: settings.abort_on_focus_change,
        own_window,
        skip: resume_from,
        delay_scale: 1.0,
    };

    let reporter_app = app.clone();
    let reporter = Arc::new(move |progress: Progress| {
        let snapshot = Snapshot {
            phase: progress.phase.as_str(),
            done: progress.done,
            total: progress.total,
            error: progress.error,
            resumable: progress.resumable,
        };
        if let Some(state) = reporter_app.try_state::<AppState>() {
            state.publish(snapshot.clone());
        }
        let _ = reporter_app.emit(EVENT_PROGRESS, snapshot);
    });

    let handle = engine::spawn(config, reporter);
    *state.engine.lock().unwrap() = Some(handle);
    Ok(())
}

#[tauri::command]
fn pause(state: State<'_, AppState>) {
    if let Some(handle) = state.engine.lock().unwrap().as_ref() {
        handle.pause();
    }
}

#[tauri::command]
fn resume(state: State<'_, AppState>) {
    if let Some(handle) = state.engine.lock().unwrap().as_ref() {
        handle.resume();
    }
}

#[tauri::command]
fn toggle_pause(state: State<'_, AppState>) {
    let paused = state.snapshot().phase == "paused";
    if let Some(handle) = state.engine.lock().unwrap().as_ref() {
        if paused {
            handle.resume();
        } else {
            handle.pause();
        }
    }
}

#[tauri::command]
fn stop(state: State<'_, AppState>) -> Snapshot {
    if let Some(handle) = state.engine.lock().unwrap().as_ref() {
        handle.stop();
    }
    // The engine reports `stopped` a few milliseconds later; reflect it now so
    // the buttons do not lag behind the click.
    let snapshot = state.snapshot();
    state.publish(Snapshot {
        phase: "stopped",
        resumable: false,
        ..snapshot.clone()
    });
    Snapshot {
        phase: "stopped",
        resumable: false,
        ..snapshot
    }
}

/// Ends the current run and keeps the text.
///
/// While a run is active this behaves like the stop shortcut. After an
/// automatic interruption it also discards the resume point, so the next Start
/// begins from the first character instead of offering "Continue".
#[tauri::command]
fn cancel(state: State<'_, AppState>) -> Snapshot {
    if state.running() {
        if let Some(handle) = state.engine.lock().unwrap().as_ref() {
            handle.stop();
        }
        let snapshot = state.snapshot();
        let stopped = Snapshot {
            phase: "stopped",
            resumable: false,
            ..snapshot
        };
        state.publish(stopped.clone());
        stopped
    } else {
        let idle = Snapshot::default();
        state.publish(idle.clone());
        idle
    }
}

#[tauri::command]
fn estimate_seconds(text: String, wpm: u16) -> u64 {
    timing::estimate(text.chars().count(), wpm).as_secs()
}

/* ---------------------------------- run --------------------------------- */

pub fn run() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_shortcut(pause_shortcut())
                .expect("registering the pause shortcut")
                .with_shortcut(stop_shortcut())
                .expect("registering the stop shortcut")
                .with_handler(|app, shortcut, event| {
                    // Fires twice (press and release); only react to the press.
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let state = app.state::<AppState>();
                    if !state.running() {
                        return;
                    }
                    if *shortcut == pause_shortcut() {
                        if let Some(handle) = state.engine.lock().unwrap().as_ref() {
                            if state.snapshot().phase == "paused" {
                                handle.resume();
                            } else {
                                handle.pause();
                            }
                        }
                    } else if *shortcut == stop_shortcut() {
                        if let Some(handle) = state.engine.lock().unwrap().as_ref() {
                            handle.stop();
                        }
                    }
                })
                .build(),
        )
        .setup(|app| {
            // The config directory is only known once the app exists, so state
            // is created here rather than with `manage` at build time.
            let config_dir = app.path().app_config_dir().ok();
            app.manage(AppState::new(settings::default_path(config_dir)));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            platform_info,
            read_clipboard,
            active_window,
            get_settings,
            update_settings,
            status,
            start,
            pause,
            resume,
            toggle_pause,
            stop,
            cancel,
            estimate_seconds
        ])
        .run(tauri::generate_context!())
        .expect("error while running Ghostwriter");
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn assert<T: Send + Sync>() {}
    assert::<AppState>();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The UI reads these names directly. A snake_case field would render as
    /// `undefined` in the shortcut hint, which is exactly the bug this pins.
    #[test]
    fn platform_info_serializes_with_the_camel_case_the_ui_reads() {
        let json = serde_json::to_value(platform_info()).expect("serialize platform info");

        assert!(
            json["canTrackFocus"].is_boolean(),
            "the UI expects a boolean"
        );
        assert!(
            json.get("can_track_focus").is_none(),
            "the UI reads canTrackFocus"
        );

        let shortcuts = &json["shortcuts"];
        assert_eq!(shortcuts["pauseResume"], "Ctrl+Shift+P");
        assert_eq!(shortcuts["stop"], "Ctrl+Shift+S");
        assert!(
            shortcuts.get("pause_resume").is_none(),
            "the UI reads pauseResume"
        );
    }
}
