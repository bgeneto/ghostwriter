//! Cross-platform end-to-end tests for the typing engine.
//!
//! These drive the real `engine::spawn_with` state machine — real threads, real
//! channels, real timing — substituting only the OS boundaries:
//!
//!   * `Sink`, which would inject keystrokes,
//!   * `FocusProbe`, which would read the focused window,
//!   * `FocusRestore`, which would bring the destination back on resume.
//!
//! The Windows-only `e2e_windows_input.rs` closes the remaining gap by using the
//! production sink against a real Win32 `EDIT` control.

mod common;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use common::{buffer_string, shared_buffer, Recorder, RecordingSink};
use ghostwriter_lib::engine::{spawn_with, Deps, EngineConfig, FocusProbe, Phase, Sink};

/// Builds deps whose sink records into `buffer` and whose focus probe is fixed.
fn recording_deps(buffer: &Arc<std::sync::Mutex<Vec<char>>>, focus: &'static str) -> Deps {
    let buffer = buffer.clone();
    Deps {
        make_sink: Box::new(move || Ok(Box::new(RecordingSink(buffer)) as Box<dyn Sink>)),
        focus: Arc::new(move || Some(focus.to_string())),
        focus_window: Arc::new(|_| false),
    }
}

/// A focus probe the test can change between steps, like a user would.
fn controllable_focus(initial: &str) -> (Arc<std::sync::Mutex<String>>, FocusProbe) {
    let current = Arc::new(std::sync::Mutex::new(initial.to_string()));
    let probe_current = current.clone();
    let probe: FocusProbe = Arc::new(move || Some(probe_current.lock().unwrap().clone()));
    (current, probe)
}

fn set_focus(state: &Arc<std::sync::Mutex<String>>, value: &str) {
    *state.lock().unwrap() = value.to_string();
}

/// Deps that record what was typed while reading focus from a shared value.
///
/// `restore` enables the fake focus restoration: when the engine asks for the
/// destination to be brought back, the shared focus switches to it.
fn focus_test_deps(
    buffer: &Arc<std::sync::Mutex<Vec<char>>>,
    restore: Option<&Arc<std::sync::Mutex<String>>>,
    probe: FocusProbe,
) -> Deps {
    let sink_buffer = buffer.clone();
    let restore_state = restore.cloned();
    Deps {
        make_sink: Box::new(move || Ok(Box::new(RecordingSink(sink_buffer)) as Box<dyn Sink>)),
        focus: probe,
        focus_window: Arc::new(move |id| match &restore_state {
            Some(state) => {
                *state.lock().unwrap() = id.to_string();
                true
            }
            None => false,
        }),
    }
}

fn fast_config(text: impl Into<String>, wpm: u16) -> EngineConfig {
    let mut config = EngineConfig::new(text, wpm);
    config.delay_scale = 0.0;
    config
}

#[test]
fn full_run_types_the_whole_text_and_reports_done() {
    let rec = Recorder::new();
    let buffer = shared_buffer();
    let text = "Olá, mundo! Ação e coração.";

    let _handle = spawn_with(
        fast_config(text, 60),
        rec.callback(),
        recording_deps(&buffer, "janela"),
    );

    assert!(rec.wait_for_phase(Duration::from_secs(5), Phase::Done));
    assert_eq!(buffer_string(&buffer), text);

    let last = rec.last().unwrap();
    assert_eq!(last.phase, Phase::Done);
    assert_eq!(last.total, text.chars().count());
    assert_eq!(last.done, last.total);
    assert!(last.error.is_none());
}

#[test]
fn character_count_ignores_utf8_byte_length() {
    let rec = Recorder::new();
    let buffer = shared_buffer();
    // 8 chars, 20 bytes in UTF-8.
    let text = "ação🚀ção";

    let _handle = spawn_with(
        fast_config(text, 60),
        rec.callback(),
        recording_deps(&buffer, "janela"),
    );

    assert!(rec.wait_for_phase(Duration::from_secs(5), Phase::Done));
    assert_eq!(buffer_string(&buffer), text);
    assert_eq!(rec.last().unwrap().total, 8);
}

#[test]
fn start_delay_defers_the_first_keystroke() {
    let rec = Recorder::new();
    let buffer = shared_buffer();

    let mut config = fast_config("abc", 60);
    config.start_delay = Duration::from_millis(300);

    let _handle = spawn_with(config, rec.callback(), recording_deps(&buffer, "janela"));

    assert!(rec.wait_for_phase(Duration::from_secs(2), Phase::Countdown));

    // During the countdown nothing must reach the sink.
    thread::sleep(Duration::from_millis(150));
    assert!(
        buffer.lock().unwrap().is_empty(),
        "typed before the countdown elapsed"
    );

    assert!(rec.wait_for_phase(Duration::from_secs(3), Phase::Done));
    assert_eq!(buffer_string(&buffer), "abc");
}

#[test]
fn stop_is_immediate_and_leaves_the_prefix_intact() {
    let rec = Recorder::new();
    let buffer = shared_buffer();

    let mut config = EngineConfig::new("a".repeat(600), 80);
    config.delay_scale = 0.05;

    let handle = spawn_with(config, rec.callback(), recording_deps(&buffer, "janela"));

    assert!(rec.wait_for(Duration::from_secs(5), |p| {
        p.phase == Phase::Typing && p.done >= 5
    }));

    handle.stop();
    assert!(rec.wait_for_phase(Duration::from_secs(2), Phase::Stopped));
    assert!(
        !rec.last().unwrap().resumable,
        "a manual stop must not offer Continue"
    );

    let typed = buffer_string(&buffer);
    assert!(typed.len() < 600, "the whole text was typed despite stop");
    assert!(typed.chars().all(|c| c == 'a'));

    // Nothing may trickle in after the stop was acknowledged.
    thread::sleep(Duration::from_millis(80));
    assert_eq!(buffer_string(&buffer), typed);
}

#[test]
fn pause_freezes_output_and_resume_completes_the_run() {
    let rec = Recorder::new();
    let buffer = shared_buffer();

    let mut config = EngineConfig::new("0123456789".repeat(5), 80);
    config.delay_scale = 0.08;

    let handle = spawn_with(config, rec.callback(), recording_deps(&buffer, "janela"));

    assert!(rec.wait_for_phase(Duration::from_secs(5), Phase::Typing));
    handle.pause();
    assert!(rec.wait_for_phase(Duration::from_secs(2), Phase::Paused));

    let frozen = buffer.lock().unwrap().len();
    thread::sleep(Duration::from_millis(120));
    assert_eq!(
        buffer.lock().unwrap().len(),
        frozen,
        "kept typing while paused"
    );

    handle.resume();
    assert!(rec.wait_for_phase(Duration::from_secs(10), Phase::Done));
    assert_eq!(buffer.lock().unwrap().len(), 50);
}

#[test]
fn resume_from_continues_where_the_previous_run_stopped() {
    let rec = Recorder::new();
    let buffer = shared_buffer();

    let mut config = fast_config("abcdefghij", 60);
    config.skip = 6;

    let _handle = spawn_with(config, rec.callback(), recording_deps(&buffer, "janela"));

    assert!(rec.wait_for_phase(Duration::from_secs(5), Phase::Done));
    assert_eq!(buffer_string(&buffer), "ghij");
    // Progress still reflects the whole text, not just the freshly typed tail.
    assert_eq!(rec.last().unwrap().done, 10);
}

#[test]
fn switching_to_the_target_during_the_countdown_starts_typing_there() {
    // The exact reported failure: press Start, move to the destination during
    // the countdown, and get "Interrompido" instead of typed text.
    let rec = Recorder::new();
    let buffer = shared_buffer();
    let (focus, probe) = controllable_focus("ghostwriter");

    let mut config = EngineConfig::new("ção, coração!", 60);
    config.delay_scale = 0.0;
    config.abort_on_focus_change = true;
    config.own_window = Some("ghostwriter".to_string());
    config.start_delay = Duration::from_millis(200);

    let _handle = spawn_with(
        config,
        rec.callback(),
        focus_test_deps(&buffer, None, probe),
    );

    assert!(rec.wait_for_phase(Duration::from_secs(2), Phase::Countdown));

    // The user does what the countdown asks.
    set_focus(&focus, "editor");

    assert!(
        rec.wait_for_phase(Duration::from_secs(5), Phase::Done),
        "switching to the destination aborted the run"
    );
    assert_eq!(buffer_string(&buffer), "ção, coração!");
}

#[test]
fn nothing_is_typed_while_the_app_itself_still_has_focus() {
    let rec = Recorder::new();
    let buffer = shared_buffer();
    let (focus, probe) = controllable_focus("ghostwriter");

    let mut config = EngineConfig::new("abc", 60);
    config.abort_on_focus_change = true;
    config.own_window = Some("ghostwriter".to_string());

    let _handle = spawn_with(
        config,
        rec.callback(),
        focus_test_deps(&buffer, None, probe),
    );

    // No start delay, and the user has not switched yet: the engine must wait,
    // not type into its own window.
    assert!(rec.wait_for_phase(Duration::from_secs(2), Phase::Countdown));
    thread::sleep(Duration::from_millis(150));
    assert!(
        buffer.lock().unwrap().is_empty(),
        "typed into the Ghostwriter window"
    );

    set_focus(&focus, "editor");
    assert!(rec.wait_for_phase(Duration::from_secs(3), Phase::Done));
    assert_eq!(buffer_string(&buffer), "abc");
}

#[test]
fn returning_to_the_app_waits_for_the_target_instead_of_aborting() {
    let rec = Recorder::new();
    let buffer = shared_buffer();
    let (focus, probe) = controllable_focus("ghostwriter");

    let mut config = EngineConfig::new("x".repeat(400), 80);
    config.delay_scale = 0.1;
    config.abort_on_focus_change = true;
    config.own_window = Some("ghostwriter".to_string());

    let _handle = spawn_with(
        config,
        rec.callback(),
        focus_test_deps(&buffer, None, probe),
    );

    set_focus(&focus, "editor");
    assert!(rec.wait_for(Duration::from_secs(2), |p| {
        p.phase == Phase::Typing && p.done >= 3
    }));

    // The user comes back to Ghostwriter to press Pause or Stop. That must
    // pause the run, not abort it under the cursor.
    set_focus(&focus, "ghostwriter");
    assert!(
        rec.wait_for_phase(Duration::from_secs(2), Phase::Countdown),
        "focusing the app did not pause the run"
    );
    let frozen = buffer.lock().unwrap().len();
    thread::sleep(Duration::from_millis(150));
    assert_eq!(
        buffer.lock().unwrap().len(),
        frozen,
        "kept typing into the app window"
    );

    // Another window is not the target: still waiting, still not typing.
    set_focus(&focus, "navegador");
    thread::sleep(Duration::from_millis(150));
    assert_eq!(
        buffer.lock().unwrap().len(),
        frozen,
        "a third window silently became the destination"
    );

    // Returning to the original target resumes the run.
    set_focus(&focus, "editor");
    assert!(rec.wait_for_phase(Duration::from_secs(10), Phase::Done));
    assert_eq!(buffer.lock().unwrap().len(), 400);
}

#[test]
fn pause_resume_after_focusing_the_app_resumes_on_the_target() {
    // The reported flow: the user clicks Pausar in Ghostwriter (which takes
    // focus), then clicks Retomar there. Resuming must put the destination back
    // in front; the user should not have to click it again.
    let rec = Recorder::new();
    let buffer = shared_buffer();
    let (focus, probe) = controllable_focus("ghostwriter");

    let mut config = EngineConfig::new("x".repeat(400), 80);
    config.delay_scale = 0.1;
    config.abort_on_focus_change = true;
    config.own_window = Some("ghostwriter".to_string());

    let handle = spawn_with(
        config,
        rec.callback(),
        focus_test_deps(&buffer, Some(&focus), probe),
    );

    set_focus(&focus, "editor");
    assert!(rec.wait_for(Duration::from_secs(2), |p| {
        p.phase == Phase::Typing && p.done >= 3
    }));

    // Clicking Pausar brings Ghostwriter to the foreground; the engine waits.
    set_focus(&focus, "ghostwriter");
    assert!(rec.wait_for_phase(Duration::from_secs(2), Phase::Countdown));

    handle.pause();
    assert!(rec.wait_for_phase(Duration::from_secs(2), Phase::Paused));
    let frozen = buffer.lock().unwrap().len();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(buffer.lock().unwrap().len(), frozen, "typed while paused");

    // Retomar restores the destination and continues without another click.
    handle.resume();
    assert!(
        rec.wait_for(Duration::from_secs(2), |p| {
            p.phase == Phase::Typing && p.done > frozen
        }),
        "resume did not restore focus to the destination: {:?}",
        rec.last()
    );
    assert!(rec.wait_for_phase(Duration::from_secs(10), Phase::Done));
    assert_eq!(buffer.lock().unwrap().len(), 400);
}

#[test]
fn resume_falls_back_to_waiting_when_the_target_cannot_be_focused() {
    // On platforms where focus restoration is unavailable, the run must wait
    // for the user to click the destination instead of typing into the app.
    let rec = Recorder::new();
    let buffer = shared_buffer();
    let (focus, probe) = controllable_focus("ghostwriter");

    let mut config = EngineConfig::new("x".repeat(200), 80);
    config.delay_scale = 0.1;
    config.abort_on_focus_change = true;
    config.own_window = Some("ghostwriter".to_string());

    let handle = spawn_with(
        config,
        rec.callback(),
        focus_test_deps(&buffer, None, probe),
    );

    set_focus(&focus, "editor");
    assert!(rec.wait_for(Duration::from_secs(2), |p| {
        p.phase == Phase::Typing && p.done >= 3
    }));

    set_focus(&focus, "ghostwriter");
    assert!(rec.wait_for_phase(Duration::from_secs(2), Phase::Countdown));
    handle.pause();
    assert!(rec.wait_for_phase(Duration::from_secs(2), Phase::Paused));

    handle.resume();
    assert!(rec.wait_for_phase(Duration::from_secs(2), Phase::Countdown));

    let frozen = buffer.lock().unwrap().len();
    thread::sleep(Duration::from_millis(150));
    assert_eq!(
        buffer.lock().unwrap().len(),
        frozen,
        "typed while waiting for a target that could not be focused"
    );

    // The user clicks the destination manually; the run continues.
    set_focus(&focus, "editor");
    assert!(rec.wait_for(Duration::from_secs(2), |p| {
        p.phase == Phase::Typing && p.done > frozen
    }));
    assert!(rec.wait_for_phase(Duration::from_secs(10), Phase::Done));
}

#[test]
fn handshake_and_resume_work_with_the_focus_guard_off() {
    // The checkbox only controls stopping on a third window. Waiting for the
    // destination and restoring it on Retomar must work regardless.
    let rec = Recorder::new();
    let buffer = shared_buffer();
    let (focus, probe) = controllable_focus("ghostwriter");

    let mut config = EngineConfig::new("x".repeat(300), 80);
    config.delay_scale = 0.1;
    config.abort_on_focus_change = false;
    config.own_window = Some("ghostwriter".to_string());

    let handle = spawn_with(
        config,
        rec.callback(),
        focus_test_deps(&buffer, Some(&focus), probe),
    );

    assert!(rec.wait_for_phase(Duration::from_secs(2), Phase::Countdown));
    thread::sleep(Duration::from_millis(100));
    assert!(
        buffer.lock().unwrap().is_empty(),
        "typed into the app with the guard off"
    );

    set_focus(&focus, "editor");
    assert!(rec.wait_for(Duration::from_secs(2), |p| {
        p.phase == Phase::Typing && p.done >= 2
    }));

    handle.pause();
    assert!(rec.wait_for_phase(Duration::from_secs(2), Phase::Paused));
    let frozen = buffer.lock().unwrap().len();

    set_focus(&focus, "ghostwriter");
    handle.resume();
    assert!(
        rec.wait_for(Duration::from_secs(2), |p| {
            p.phase == Phase::Typing && p.done > frozen
        }),
        "resume did not restore the target with the guard off: {:?}",
        rec.last()
    );
    assert!(rec.wait_for_phase(Duration::from_secs(10), Phase::Done));
    assert_eq!(buffer.lock().unwrap().len(), 300);
}

#[test]
fn with_the_guard_off_a_third_window_does_not_stop_the_run() {
    let rec = Recorder::new();
    let buffer = shared_buffer();
    let (focus, probe) = controllable_focus("ghostwriter");

    let mut config = EngineConfig::new("x".repeat(100), 80);
    config.delay_scale = 0.1;
    config.abort_on_focus_change = false;
    config.own_window = Some("ghostwriter".to_string());

    let _handle = spawn_with(
        config,
        rec.callback(),
        focus_test_deps(&buffer, None, probe),
    );

    set_focus(&focus, "editor");
    assert!(rec.wait_for(Duration::from_secs(2), |p| {
        p.phase == Phase::Typing && p.done >= 2
    }));

    // With the checkbox off, another window is an accepted destination change.
    set_focus(&focus, "navegador");
    assert!(
        rec.wait_for_phase(Duration::from_secs(10), Phase::Done),
        "the guard stopped the run even though it was off: {:?}",
        rec.last()
    );
    assert_eq!(buffer.lock().unwrap().len(), 100);
}

#[test]
fn stop_is_honoured_while_waiting_for_the_target() {
    // The user returns to Ghostwriter and presses Stop before going back to the
    // destination. The engine must accept the command while it is waiting.
    let rec = Recorder::new();
    let buffer = shared_buffer();
    let (_focus, probe) = controllable_focus("ghostwriter");

    let mut config = EngineConfig::new("x".repeat(200), 80);
    config.abort_on_focus_change = true;
    config.own_window = Some("ghostwriter".to_string());

    let handle = spawn_with(
        config,
        rec.callback(),
        focus_test_deps(&buffer, None, probe),
    );

    assert!(rec.wait_for_phase(Duration::from_secs(2), Phase::Countdown));
    thread::sleep(Duration::from_millis(100));
    assert!(buffer.lock().unwrap().is_empty());

    handle.stop();
    assert!(rec.wait_for_phase(Duration::from_secs(2), Phase::Stopped));
    assert!(
        !rec.last().unwrap().resumable,
        "Stop while waiting is still a deliberate end"
    );
    assert!(buffer.lock().unwrap().is_empty(), "typed after the stop");
}

#[test]
fn switching_windows_mid_run_aborts_instead_of_typing_elsewhere() {
    let rec = Recorder::new();
    let buffer = shared_buffer();
    let (focus, probe) = controllable_focus("ghostwriter");

    let mut config = EngineConfig::new("x".repeat(300), 80);
    config.delay_scale = 0.1;
    config.abort_on_focus_change = true;
    config.own_window = Some("ghostwriter".to_string());

    let _handle = spawn_with(
        config,
        rec.callback(),
        focus_test_deps(&buffer, None, probe),
    );

    // Nothing may be typed while the app itself still has focus.
    thread::sleep(Duration::from_millis(100));
    assert!(
        buffer.lock().unwrap().is_empty(),
        "typed into the app window"
    );

    set_focus(&focus, "editor");
    assert!(rec.wait_for(Duration::from_secs(2), |p| {
        p.phase == Phase::Typing && p.done >= 3
    }));

    // A third window is a genuine destination change: abort.
    set_focus(&focus, "navegador");
    assert!(rec.wait_for_phase(Duration::from_secs(3), Phase::Stopped));
    assert!(
        buffer.lock().unwrap().len() < 300,
        "typed the whole text into the wrong window"
    );

    let last = rec.last().unwrap();
    assert!(
        last.error
            .as_deref()
            .unwrap_or("")
            .contains("janela em foco"),
        "the stop must explain why it happened: {:?}",
        last.error
    );
    assert!(
        last.resumable,
        "an automatic focus stop must stay resumable"
    );
}

#[test]
fn focus_guard_is_off_by_default_for_programmatic_runs() {
    // `EngineConfig::new` leaves the guard disabled, exactly like the unit tests
    // expect: a changing focus probe must not stop the run.
    let rec = Recorder::new();
    let buffer = shared_buffer();

    let calls = Arc::new(std::sync::Mutex::new(0u32));
    let probe_calls = calls.clone();
    let sink_buffer = buffer.clone();
    let deps = Deps {
        make_sink: Box::new(move || Ok(Box::new(RecordingSink(sink_buffer)) as Box<dyn Sink>)),
        focus: Arc::new(move || {
            let mut n = probe_calls.lock().unwrap();
            *n += 1;
            Some(format!("janela-{n}"))
        }),
        focus_window: Arc::new(|_| false),
    };

    let _handle = spawn_with(fast_config("abcdefghij", 60), rec.callback(), deps);

    assert!(rec.wait_for_phase(Duration::from_secs(5), Phase::Done));
    assert_eq!(buffer.lock().unwrap().len(), 10);
}

#[test]
fn an_input_failure_stops_the_run_and_surfaces_the_message() {
    let rec = Recorder::new();

    let deps = Deps {
        make_sink: Box::new(|| Ok(Box::new(common::FailingSink("acesso negado")) as Box<dyn Sink>)),
        focus: Arc::new(|| Some("janela".to_string())),
        focus_window: Arc::new(|_| false),
    };

    let _handle = spawn_with(fast_config("abcdefghij", 60), rec.callback(), deps);

    assert!(rec.wait_for_phase(Duration::from_secs(5), Phase::Failed));
    let last = rec.last().unwrap();
    assert_eq!(last.error.as_deref(), Some("acesso negado"));
    assert_eq!(last.done, 0);
}

#[test]
fn a_missing_typer_backend_is_reported_with_a_hint() {
    let rec = Recorder::new();

    let deps = Deps {
        make_sink: Box::new(|| Err("no successful connection".to_string())),
        focus: Arc::new(|| None),
        focus_window: Arc::new(|_| false),
    };

    let _handle = spawn_with(fast_config("texto", 60), rec.callback(), deps);

    assert!(rec.wait_for_phase(Duration::from_secs(5), Phase::Failed));
    assert!(rec
        .last()
        .unwrap()
        .error
        .unwrap()
        .contains("no successful connection"));
}

#[test]
fn every_phase_the_frontend_knows_is_reachable() {
    // The UI maps these strings; a typo here would silently render raw IDs.
    let expected = ["countdown", "typing", "paused", "done", "stopped", "failed"];
    let actual = [
        Phase::Countdown.as_str(),
        Phase::Typing.as_str(),
        Phase::Paused.as_str(),
        Phase::Done.as_str(),
        Phase::Stopped.as_str(),
        Phase::Failed.as_str(),
    ];
    assert_eq!(actual, expected);
}

#[test]
fn progress_reports_are_monotonic_and_bounded() {
    let rec = Recorder::new();
    let buffer = shared_buffer();
    let text = "abcdefghijklmnopqrstuvwxyz0123456789";

    let _handle = spawn_with(
        fast_config(text, 80),
        rec.callback(),
        recording_deps(&buffer, "janela"),
    );

    assert!(rec.wait_for_phase(Duration::from_secs(5), Phase::Done));

    let reports = rec.all();
    assert!(!reports.is_empty());

    let mut previous = 0;
    for p in &reports {
        assert!(
            p.done >= previous,
            "progress went backwards: {} after {previous}",
            p.done
        );
        assert!(p.done <= p.total, "done exceeded total");
        assert_eq!(p.total, text.chars().count(), "total changed mid-run");
        previous = p.done;
    }
    assert_eq!(previous, text.chars().count());
}

#[test]
fn a_second_start_replaces_the_first_run() {
    // Two engines must never type simultaneously; the app stops the previous
    // handle before spawning. Verify a stopped engine goes quiet.
    let rec_first = Recorder::new();
    let first_buffer = shared_buffer();

    let mut config = EngineConfig::new("1".repeat(400), 80);
    config.delay_scale = 0.05;

    let first = spawn_with(
        config,
        rec_first.callback(),
        recording_deps(&first_buffer, "janela"),
    );
    assert!(rec_first.wait_for_phase(Duration::from_secs(5), Phase::Typing));
    first.stop();
    assert!(rec_first.wait_for_phase(Duration::from_secs(2), Phase::Stopped));

    let frozen = first_buffer.lock().unwrap().len();

    let rec_second = Recorder::new();
    let second_buffer = shared_buffer();
    let _second = spawn_with(
        fast_config("2222", 60),
        rec_second.callback(),
        recording_deps(&second_buffer, "janela"),
    );

    assert!(rec_second.wait_for_phase(Duration::from_secs(5), Phase::Done));
    assert_eq!(buffer_string(&second_buffer), "2222");
    assert_eq!(
        first_buffer.lock().unwrap().len(),
        frozen,
        "the replaced engine kept typing"
    );
}
