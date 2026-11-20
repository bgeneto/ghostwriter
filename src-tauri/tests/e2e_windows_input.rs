//! End-to-end test: real keystroke injection through `enigo` on Windows.
//!
//! The rest of the suite proves the state machine with a fake sink. This test
//! closes the loop that actually matters for a Portuguese-speaking audience: it
//! creates a real Win32 window with a real `EDIT` control, focuses it, and lets
//! the production `Typer` type into it. Then it reads the text back with
//! `GetWindowTextW`.
//!
//! What this catches that unit tests cannot:
//!   * accented characters (`ã ç é õ ê`) surviving the injection path,
//!   * line breaks arriving as real Enter presses,
//!   * the engine's character count matching the visible text.
//!
//! Requires an interactive desktop session. On a headless CI agent it skips
//! itself with a printed reason instead of failing.
//!
//! ## Why the window lives on its own thread
//!
//! Taking the foreground requires `AttachThreadInput`, which merges the caller's
//! input queue with the queue of the thread that currently owns the foreground
//! window. If the calling thread does not pump messages, the merge deadlocks as
//! soon as the other thread sends it anything. So one thread owns the window,
//! pumps its message loop, and performs focus and text reads on request; the
//! test thread only sends commands and waits for replies.

#![cfg(target_os = "windows")]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common::Recorder;
use ghostwriter_lib::engine::{spawn, spawn_with, Deps, EngineConfig, Phase};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    LoadCursorW, MoveWindow, PeekMessageW, RegisterClassW, SetForegroundWindow, SetWindowTextW,
    ShowWindow, TranslateMessage, ES_AUTOVSCROLL, ES_MULTILINE, ES_WANTRETURN, IDC_ARROW, MSG,
    PM_REMOVE, SW_RESTORE, SW_SHOWNORMAL, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW, WS_CHILD,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE, WS_VSCROLL,
};

const CLASS_NAME: &str = "GhostwriterE2EHost";
const EDIT_CLASS: &str = "EDIT";

/* ------------------------- focus-stealing helpers ------------------------ */

// `AttachThreadInput` and `SetFocus` are not exposed by the `windows` crate, so
// they are linked directly from user32. Both are plain Win32 calls.

#[link(name = "user32")]
extern "system" {
    #[link_name = "AttachThreadInput"]
    fn AttachThreadInput(id_attach: u32, id_attach_to: u32, attach: i32) -> i32;
    #[link_name = "SetFocus"]
    fn SetFocus(hwnd: HWND) -> HWND;
}

fn attach_thread_input(from: u32, to: u32, attach: bool) -> bool {
    unsafe { AttachThreadInput(from, to, attach as i32) != 0 }
}

fn set_focus(hwnd: HWND) -> bool {
    unsafe { !SetFocus(hwnd).0.is_null() }
}

/// Minimal window procedure: everything goes to the default handler.
unsafe extern "system" fn def_window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, w, l)
}

/* -------------------------------- commands ------------------------------- */

/// Requests handled by the window thread.
enum WndCmd {
    /// Take the foreground and focus the edit control. Replies with success.
    Focus(Sender<bool>),
    /// Read the edit control's contents.
    Text(Sender<String>),
    /// Empty the edit control.
    Clear(Sender<()>),
    /// Destroy the window and stop the loop.
    Shutdown,
}

/* -------------------------------- the host ------------------------------- */

/// Handle to the window thread. Dropping it tears the window down.
struct TestWindow {
    cmd_tx: Sender<WndCmd>,
    join: Option<thread::JoinHandle<()>>,
}

impl TestWindow {
    /// A host window with an EDIT child, like a typical application.
    fn create() -> Option<Self> {
        Self::create_with(false)
    }

    /// A top-level EDIT control, so focusing the window is enough to receive
    /// keystrokes even after the window was deactivated and reactivated.
    fn create_top_level_edit() -> Option<Self> {
        Self::create_with(true)
    }

    /// `None` when there is no usable interactive desktop.
    fn create_with(top_level_edit: bool) -> Option<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();

        let join = thread::Builder::new()
            .name("gw-e2e-window".into())
            .spawn(move || window_thread(cmd_rx, ready_tx, top_level_edit))
            .ok()?;

        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(true) => Some(Self {
                cmd_tx,
                join: Some(join),
            }),
            // Either the window could not be created or the thread died.
            _ => {
                let _ = join.join();
                None
            }
        }
    }

    fn call<T>(&self, make: impl FnOnce(Sender<T>) -> WndCmd, timeout: Duration) -> Option<T> {
        let (tx, rx) = mpsc::channel();
        self.cmd_tx.send(make(tx)).ok()?;
        rx.recv_timeout(timeout).ok()
    }

    fn focus(&self) -> bool {
        self.call(WndCmd::Focus, Duration::from_secs(10))
            .unwrap_or(false)
    }

    fn text(&self) -> String {
        self.call(WndCmd::Text, Duration::from_secs(5))
            .unwrap_or_default()
    }

    fn clear(&self) {
        let _ = self.call(WndCmd::Clear, Duration::from_secs(5));
    }

    /// Waits until the control's contents stop changing.
    ///
    /// `SendInput` hands keystrokes to the target thread's message queue, so the
    /// control processes them a few milliseconds later. Measuring immediately
    /// after the engine reports `Stopped` would catch a character that was
    /// already in flight — a race in the test, not a failure to stop.
    fn settle(&self, quiet_for: Duration, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let mut last = self.text();
        let mut stable_since = Instant::now();
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
            let now = self.text();
            if now != last {
                last = now;
                stable_since = Instant::now();
            } else if stable_since.elapsed() >= quiet_for {
                break;
            }
        }
        last
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WndCmd::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Owns the window for its whole life and pumps its message queue.
fn window_thread(cmd_rx: Receiver<WndCmd>, ready: Sender<bool>, top_level_edit: bool) {
    unsafe {
        let class_name: Vec<u16> = CLASS_NAME.encode_utf16().chain(Some(0)).collect();
        let title: Vec<u16> = "Ghostwriter E2E".encode_utf16().chain(Some(0)).collect();
        let edit_class: Vec<u16> = EDIT_CLASS.encode_utf16().chain(Some(0)).collect();

        let instance: HINSTANCE = match GetModuleHandleW(None) {
            Ok(h) => h.into(),
            Err(_) => {
                let _ = ready.send(false);
                return;
            }
        };

        // The top-level EDIT variant has no parent/child pair: the window that
        // must be in the foreground is the control that receives the text.
        let (hwnd, edit) = if top_level_edit {
            let edit = match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(edit_class.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW
                    | WS_VISIBLE
                    | WINDOW_STYLE((ES_MULTILINE | ES_WANTRETURN | ES_AUTOVSCROLL) as u32),
                80,
                80,
                640,
                240,
                None,
                None,
                Some(instance),
                None,
            ) {
                Ok(h) => h,
                Err(_) => {
                    let _ = ready.send(false);
                    return;
                }
            };
            (edit, edit)
        } else {
            // A window class must exist before `CreateWindowExW` can make one.
            // Re-registering across tests fails harmlessly.
            let wnd_class = WNDCLASSW {
                lpfnWndProc: Some(def_window_proc),
                hInstance: instance,
                lpszClassName: PCWSTR(class_name.as_ptr()),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            let _ = RegisterClassW(&wnd_class);

            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                80,
                80,
                640,
                240,
                None,
                None,
                Some(instance),
                None,
            ) {
                Ok(h) => h,
                Err(_) => {
                    let _ = ready.send(false);
                    return;
                }
            };

            let edit = match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(edit_class.as_ptr()),
                PCWSTR::null(),
                // Multi-line with want-return: a single-line EDIT *commits* on
                // Enter instead of inserting a newline, which would silently
                // change what the test is asserting.
                WS_CHILD
                    | WS_VISIBLE
                    | WS_VSCROLL
                    | WINDOW_STYLE((ES_MULTILINE | ES_WANTRETURN | ES_AUTOVSCROLL) as u32),
                8,
                8,
                610,
                180,
                Some(hwnd),
                None,
                Some(instance),
                None,
            ) {
                Ok(h) => h,
                Err(_) => {
                    let _ = DestroyWindow(hwnd);
                    let _ = ready.send(false);
                    return;
                }
            };
            (hwnd, edit)
        };

        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = MoveWindow(hwnd, 80, 80, 640, 240, true);
        if top_level_edit {
            let _ = set_focus(edit);
        }
        let _ = ready.send(true);

        let this_thread = GetCurrentThreadId();
        let mut running = true;

        while running {
            // Handle commands, then drain the message queue so this thread is a
            // well-behaved window owner (required for AttachThreadInput).
            loop {
                match cmd_rx.recv_timeout(Duration::from_millis(5)) {
                    Ok(WndCmd::Focus(reply)) => {
                        let ok = take_foreground(hwnd, edit, this_thread);
                        let _ = reply.send(ok);
                    }
                    Ok(WndCmd::Text(reply)) => {
                        let _ = reply.send(read_text(edit));
                    }
                    Ok(WndCmd::Clear(reply)) => {
                        let _ = SetWindowTextW(edit, PCWSTR::null());
                        let _ = reply.send(());
                    }
                    Ok(WndCmd::Shutdown) => {
                        running = false;
                        break;
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        running = false;
                        break;
                    }
                }
            }

            // Pump. `PeekMessage` (not `GetMessage`) so the loop can also poll
            // the command channel.
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            if !running {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        let _ = DestroyWindow(hwnd);
    }
}

/// Takes the foreground for `hwnd` and puts the keyboard focus on `edit`.
///
/// Must be called from the thread that owns both windows: that is the only
/// position from which merging input queues is safe.
unsafe fn take_foreground(hwnd: HWND, edit: HWND, this_thread: u32) -> bool {
    let foreground = GetForegroundWindow();

    // Already ours: nothing to negotiate.
    if foreground == hwnd {
        let _ = set_focus(edit);
        return true;
    }

    let fg_thread = GetWindowThreadProcessId(foreground, None);
    let mut attached = Vec::new();
    if fg_thread != 0
        && fg_thread != this_thread
        && attach_thread_input(this_thread, fg_thread, true)
    {
        attached.push(fg_thread);
    }

    let _ = ShowWindow(hwnd, SW_RESTORE);
    let _ = BringWindowToTop(hwnd);
    let _ = SetForegroundWindow(hwnd);
    let _ = set_focus(hwnd);
    let _ = set_focus(edit);

    for t in attached {
        let _ = attach_thread_input(this_thread, t, false);
    }

    // Confirm the OS actually agreed, pumping while we wait.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if GetForegroundWindow() == hwnd {
            let _ = set_focus(edit);
            return true;
        }
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

unsafe fn read_text(edit: HWND) -> String {
    let len = GetWindowTextLengthW(edit);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let written = GetWindowTextW(edit, &mut buf);
    let raw = String::from_utf16_lossy(&buf[..written.max(0) as usize]);
    // A multi-line EDIT stores line breaks as CRLF, the Windows convention.
    // Normalising here keeps the assertions about *characters*, not about the
    // toolkit's line-ending style.
    raw.replace("\r\n", "\n")
}

/* ------------------------------ test helpers ----------------------------- */

fn desktop_available() -> bool {
    unsafe { !GetForegroundWindow().0.is_null() }
}

/// Creates and focuses the window, or returns `None` with a skip message.
fn prepare(context: &str) -> Option<TestWindow> {
    if !desktop_available() {
        eprintln!("skipping {context}: no interactive desktop (no foreground window)");
        return None;
    }
    let window = match TestWindow::create() {
        Some(w) => w,
        None => {
            eprintln!("skipping {context}: could not create a Win32 window");
            return None;
        }
    };
    if !window.focus() {
        eprintln!("skipping {context}: could not take the foreground");
        return None;
    }
    Some(window)
}

/// Aborts the whole process if a test exceeds its budget.
///
/// Without this a lost focus reply turns the suite into a hang, which is much
/// harder to diagnose than a failure. The guard is disarmed when the test ends.
struct Watchdog(Arc<AtomicBool>);

impl Watchdog {
    fn arm(name: &'static str, budget: Duration) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        thread::spawn(move || {
            let deadline = Instant::now() + budget;
            while Instant::now() < deadline {
                if flag.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
            eprintln!("TIMEOUT: {name} exceeded {budget:?}");
            std::process::exit(101);
        });
        Self(done)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/* --------------------------------- tests -------------------------------- */

#[test]
fn types_accents_and_line_breaks_into_a_real_win32_edit_control() {
    let _dog = Watchdog::arm("accent test", Duration::from_secs(90));

    let Some(window) = prepare("accent test") else {
        return;
    };

    // The characters that break naive injection: accents, cedilla, ellipsis.
    let expected = "Ação, coração e você!\nNão é fácil…";

    let mut config = EngineConfig::new(expected, 80);
    // Fast enough for a test, slow enough to still go through the real timing
    // and sleeping path rather than a busy loop.
    config.delay_scale = 0.05;

    let rec = Recorder::new();
    let _handle = spawn(config, rec.callback());

    assert!(
        rec.wait_for_phase(Duration::from_secs(45), Phase::Done),
        "engine never reached Done: {:?}",
        rec.last()
    );
    assert!(
        rec.last().unwrap().error.is_none(),
        "engine reported a failure: {:?}",
        rec.last().unwrap().error
    );

    let typed = window.settle(Duration::from_millis(300), Duration::from_secs(3));
    assert_eq!(
        typed, expected,
        "\n  expected: {expected:?}\n  actually typed: {typed:?}"
    );

    // The engine's own count must agree with what the control received.
    assert_eq!(rec.last().unwrap().done, expected.chars().count());
}

#[test]
fn stop_is_honoured_while_typing_into_a_real_window() {
    let _dog = Watchdog::arm("stop test", Duration::from_secs(90));

    let Some(window) = prepare("stop test") else {
        return;
    };

    let full = "palavra ".repeat(80);
    let mut config = EngineConfig::new(full.clone(), 60);
    config.delay_scale = 0.6; // slow enough to interrupt reliably

    let rec = Recorder::new();
    let handle = spawn(config, rec.callback());

    assert!(
        rec.wait_for(Duration::from_secs(15), |p| p.phase == Phase::Typing
            && p.done > 0),
        "engine never started typing"
    );

    handle.stop();
    assert!(
        rec.wait_for_phase(Duration::from_secs(10), Phase::Stopped),
        "engine did not acknowledge the stop"
    );

    let after_stop = window.settle(Duration::from_millis(300), Duration::from_secs(3));
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        window.settle(Duration::from_millis(200), Duration::from_secs(2)),
        after_stop,
        "characters kept arriving after the stop command"
    );
    assert!(
        after_stop.chars().count() < full.chars().count(),
        "the full text was typed despite the stop"
    );
}

#[test]
fn the_focus_guard_discovers_the_real_foreground_window() {
    let _dog = Watchdog::arm("real focus test", Duration::from_secs(90));

    let Some(window) = prepare("real focus test") else {
        return;
    };
    window.clear();

    let text = "foco real ";
    let mut config = EngineConfig::new(text, 80);
    config.delay_scale = 0.05;
    config.abort_on_focus_change = true;
    // Deliberately not the test window: the engine must read the actual
    // foreground window through the production probe and adopt it as the
    // target. This is the handshake that was missing when focusing the
    // destination aborted the run.
    config.own_window = Some("ghostwriter-app-window".to_string());

    let rec = Recorder::new();
    let _handle = spawn_with(config, rec.callback(), Deps::production());

    // Fail fast on a deliberate stop: if the desktop's foreground window
    // changes under the test (a real user interacting with the machine), the
    // guard is supposed to stop, and waiting the full budget hides that.
    assert!(
        rec.wait_for(Duration::from_secs(10), |p| matches!(
            p.phase,
            Phase::Done | Phase::Stopped | Phase::Failed
        )),
        "the engine never settled: {:?}",
        rec.last()
    );
    let last = rec.last().unwrap();
    assert_eq!(
        last.phase,
        Phase::Done,
        "the real focus probe did not adopt the foreground window: {last:?}"
    );
    assert!(
        last.error.is_none(),
        "engine reported a failure: {:?}",
        last.error
    );

    let typed = window.settle(Duration::from_millis(300), Duration::from_secs(3));
    assert_eq!(
        typed, text,
        "\n  expected: {text:?}\n  actually typed: {typed:?}"
    );
}

#[test]
fn retomar_restores_the_destination_window() {
    let _dog = Watchdog::arm("restore focus test", Duration::from_secs(90));

    // Create the window that will steal focus *before* the destination, so the
    // destination is the last activation and therefore the engine's target.
    let Some(other) = TestWindow::create() else {
        eprintln!("skipping restore focus test: could not create the second window");
        return;
    };
    thread::sleep(Duration::from_millis(200));

    if !desktop_available() {
        eprintln!("skipping restore focus test: no interactive desktop");
        return;
    }
    // A top-level EDIT: focusing the window also gives the text control focus,
    // which is what a real destination app does for its own editor.
    let Some(target) = TestWindow::create_top_level_edit() else {
        eprintln!("skipping restore focus test: could not create the target");
        return;
    };
    if !target.focus() {
        eprintln!("skipping restore focus test: could not focus the target");
        return;
    }
    target.clear();

    let text = "recomeço depois da pausa ";
    let mut config = EngineConfig::new(text, 80);
    config.delay_scale = 0.05;
    config.abort_on_focus_change = true;
    // Deliberately not either real window: the engine must discover the
    // foreground window as the target.
    config.own_window = Some("ghostwriter-app-window".to_string());

    let rec = Recorder::new();
    let handle = spawn_with(config, rec.callback(), Deps::production());

    assert!(
        rec.wait_for(Duration::from_secs(15), |p| p.phase == Phase::Typing
            && p.done >= 2),
        "the engine never started typing: {:?}",
        rec.last()
    );

    handle.pause();
    assert!(
        rec.wait_for_phase(Duration::from_secs(5), Phase::Paused),
        "the engine did not pause: {:?}",
        rec.last()
    );

    // Pausing does not move focus by itself; move it away explicitly, as the
    // user does when they click the Ghostwriter window.
    assert!(other.focus(), "could not focus the second window");
    thread::sleep(Duration::from_millis(300));

    // Retomar must bring the first window back and continue typing there.
    handle.resume();
    assert!(
        rec.wait_for_phase(Duration::from_secs(20), Phase::Done),
        "resume did not restore the target: {:?}",
        rec.last()
    );

    let typed = target.settle(Duration::from_millis(300), Duration::from_secs(3));
    let other_text = other.text();
    assert_eq!(
        typed, text,
        "\n  expected: {text:?}\n  actually typed: {typed:?}"
    );
    assert!(
        other_text.is_empty(),
        "text leaked into the second window: {other_text:?}"
    );
}

#[test]
fn a_typing_run_reports_a_total_matching_the_text() {
    let _dog = Watchdog::arm("totals test", Duration::from_secs(90));

    let Some(window) = prepare("totals test") else {
        return;
    };
    window.clear();

    let text = "ção repetido ";
    let mut config = EngineConfig::new(text, 80);
    config.delay_scale = 0.05;

    let rec = Recorder::new();
    let _handle = spawn(config, rec.callback());

    assert!(
        rec.wait_for_phase(Duration::from_secs(45), Phase::Done),
        "engine never reached Done: {:?}",
        rec.last()
    );
    assert_eq!(rec.last().unwrap().total, text.chars().count());

    // And the characters really landed in the control. The last keystroke can
    // still be in the target's message queue when `Done` is reported, so let
    // the control settle before comparing.
    let typed = window.settle(Duration::from_millis(300), Duration::from_secs(3));
    assert_eq!(
        typed, text,
        "\n  expected: {text:?}\n  actually typed: {typed:?}"
    );
}
