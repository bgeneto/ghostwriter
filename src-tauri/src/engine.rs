//! The typing engine.
//!
//! Runs on its own OS thread because the delay loop is sequential and blocking,
//! and because the typer owns a connection to the platform input stack that must
//! stay on a single thread. The engine never touches the UI: it reports through
//! a callback that Tauri forwards to the webview as events.
//!
//! Input injection and focus detection sit behind the [`Sink`] and
//! [`FocusProbe`] traits so the state machine can be tested end to end without
//! typing into the developer's real windows.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::timing::{self, Rng};
use crate::typer::{self, Typer};

/// What the engine is doing. Mirrored in the frontend as a plain string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Waiting out the start delay so the user can focus the target window.
    Countdown,
    Typing,
    Paused,
    Done,
    Stopped,
    Failed,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Countdown => "countdown",
            Phase::Typing => "typing",
            Phase::Paused => "paused",
            Phase::Done => "done",
            Phase::Stopped => "stopped",
            Phase::Failed => "failed",
        }
    }
}

/// Snapshot sent to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub phase: Phase,
    /// Characters already emitted.
    pub done: usize,
    pub total: usize,
    pub error: Option<String>,
    /// Whether the UI may offer "Continue from character N".
    ///
    /// False after a deliberate Stop: that ends the run, and the next Start
    /// begins from the beginning. Automatic interruptions stay resumable.
    pub resumable: bool,
}

impl Progress {
    fn new(phase: Phase, done: usize, total: usize) -> Self {
        Self {
            phase,
            done,
            total,
            error: None,
            resumable: false,
        }
    }

    fn failed(done: usize, total: usize, message: String) -> Self {
        Self {
            phase: Phase::Failed,
            done,
            total,
            error: Some(message),
            resumable: true,
        }
    }

    /// A stop the engine decided on (focus change, disconnected UI).
    fn stopped(done: usize, total: usize, resumable: bool) -> Self {
        Self {
            phase: Phase::Stopped,
            done,
            total,
            error: None,
            resumable,
        }
    }

    /// A deliberate stop that still carries a reason, so the UI can explain
    /// why the run ended instead of showing a bare "Interrompido".
    fn stopped_with(done: usize, total: usize, message: impl Into<String>) -> Self {
        Self {
            phase: Phase::Stopped,
            done,
            total,
            error: Some(message.into()),
            resumable: true,
        }
    }
}

/* --------------------------------- input -------------------------------- */

/// Where characters go. Production writes them to the OS; tests record them.
pub trait Sink: Send {
    fn emit(&mut self, ch: char) -> Result<(), String>;
}

/// Answers "which window is focused right now?".
pub type FocusProbe = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Tries to bring a window back to the foreground.
pub type FocusRestore = Arc<dyn Fn(&str) -> bool + Send + Sync>;

pub struct Deps {
    pub make_sink: Box<dyn FnOnce() -> Result<Box<dyn Sink>, String> + Send>,
    pub focus: FocusProbe,
    pub focus_window: FocusRestore,
}

impl Deps {
    /// The real dependencies: `enigo` and the active-window lookup.
    pub fn production() -> Self {
        Self {
            make_sink: Box::new(|| Typer::new().map(|t| Box::new(t) as Box<dyn Sink>)),
            focus: Arc::new(crate::window::active_id),
            focus_window: Arc::new(crate::window::focus_id),
        }
    }
}

/// `Typer` is the production sink.
impl Sink for Typer {
    fn emit(&mut self, ch: char) -> Result<(), String> {
        self.type_char(ch)
    }
}

/* --------------------------------- flags -------------------------------- */

/// `Default` starts the flag at zero, so zero means "running"; `stop()` writes
/// `STOPPED`, and any non-zero value counts as stopped.
const STOPPED: u8 = 1;

/// Shared run/stop switch. Flips without locking and without waiting for the
/// typing thread to reach its next check.
#[derive(Clone, Default)]
pub struct RunFlag {
    stopped: Arc<AtomicU8>,
    /// Set only by an explicit Stop command, never by an internal safety stop.
    manual: Arc<AtomicU8>,
}

impl RunFlag {
    /// Internal safety stop. The run stays resumable.
    pub fn stop(&self) {
        self.stopped.store(STOPPED, Ordering::SeqCst);
    }

    /// Deliberate Stop from the UI or the global shortcut. This ends the run.
    pub fn stop_manually(&self) {
        self.manual.store(1, Ordering::SeqCst);
        self.stopped.store(STOPPED, Ordering::SeqCst);
    }

    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst) == STOPPED
    }

    pub fn manual_stop(&self) -> bool {
        self.manual.load(Ordering::SeqCst) != 0
    }

    /// Test helper: put the flag back into the running state.
    ///
    /// Zero is the running value; `Default` starts there and only `stop()`
    /// ever moves it to `STOPPED`.
    #[cfg(test)]
    fn reset(&self) {
        self.stopped.store(0, Ordering::SeqCst);
        self.manual.store(0, Ordering::SeqCst);
    }
}

/* -------------------------------- commands ------------------------------- */

/// Messages from the UI thread to the typing thread.
enum Cmd {
    Pause,
    Resume,
    Stop,
}

/// Handle kept by the app state. Calling `stop` interrupts even a paused run.
pub struct EngineHandle {
    cmd_tx: Sender<Cmd>,
    flag: RunFlag,
}

impl EngineHandle {
    pub fn pause(&self) {
        let _ = self.cmd_tx.send(Cmd::Pause);
    }

    pub fn resume(&self) {
        let _ = self.cmd_tx.send(Cmd::Resume);
    }

    pub fn stop(&self) {
        // The flag goes first: it interrupts a sleep even before the command is read.
        self.flag.stop_manually();
        let _ = self.cmd_tx.send(Cmd::Stop);
    }
}

pub struct EngineConfig {
    pub text: String,
    pub wpm: u16,
    pub start_delay: Duration,
    /// Stop when a third window is focused mid-run.
    ///
    /// The initial target handshake and the focus restore on *Retomar* are
    /// independent: they also work with the guard off. Without the guard, a
    /// third window is simply allowed to receive the text.
    pub abort_on_focus_change: bool,
    /// The window that had focus when the user pressed Start — the Ghostwriter
    /// window itself. It is not the target: typing only begins once focus has
    /// moved away from it, and returning to it pauses instead of aborting.
    pub own_window: Option<String>,
    /// Characters already emitted by a previous, interrupted run.
    pub skip: usize,
    /// Multiplies every computed delay. Production uses `1.0`; the tests use
    /// `0.0` so the full state machine runs in real time.
    pub delay_scale: f64,
}

impl EngineConfig {
    pub fn new(text: impl Into<String>, wpm: u16) -> Self {
        Self {
            text: text.into(),
            wpm,
            start_delay: Duration::ZERO,
            abort_on_focus_change: false,
            own_window: None,
            skip: 0,
            delay_scale: 1.0,
        }
    }
}

/// Callback invoked on the typing thread. Must not block.
pub type Reporter = Arc<dyn Fn(Progress) + Send + Sync>;

/* --------------------------------- spawn --------------------------------- */

/// Starts a run on its own thread and returns immediately.
pub fn spawn(config: EngineConfig, report: Reporter) -> EngineHandle {
    spawn_with(config, report, Deps::production())
}

/// Same as [`spawn`] but with injectable input and focus sources.
pub fn spawn_with(config: EngineConfig, report: Reporter, deps: Deps) -> EngineHandle {
    let flag = RunFlag::default();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let thread_flag = flag.clone();

    thread::Builder::new()
        .name("ghostwriter-engine".into())
        .spawn(move || run(config, cmd_rx, thread_flag, report, deps))
        .expect("failed to spawn the typing thread");

    EngineHandle { cmd_tx, flag }
}

/* ------------------------------- the machine ----------------------------- */

fn run(
    config: EngineConfig,
    cmd_rx: mpsc::Receiver<Cmd>,
    flag: RunFlag,
    report: Reporter,
    deps: Deps,
) {
    // Everything is counted in chars, not bytes: accented input must not
    // advance the progress bar twice.
    let chars: Vec<char> = config.text.chars().collect();
    let total = chars.len();
    let mut cursor = config.skip.min(total);

    let mut sink = match (deps.make_sink)() {
        Ok(sink) => sink,
        Err(e) => {
            let message = match typer::platform_hint() {
                Some(hint) => format!("{e} — {hint}"),
                None => e,
            };
            flag.stop();
            report(Progress::failed(cursor, total, message));
            return;
        }
    };

    let focus = deps.focus;
    let focus_window = deps.focus_window;
    let app_window = if config.abort_on_focus_change {
        // The command layer passes the window that had focus when Start was
        // pressed. Falling back to the probe keeps direct callers working.
        config.own_window.clone().or_else(|| focus())
    } else {
        // Even with the guard off, the command layer supplies own_window: the
        // destination handshake and focus restore still use it.
        config.own_window.clone()
    };

    let mut rng = Rng::seeded();
    let mut paused = false;

    report(Progress::new(Phase::Countdown, cursor, total));

    // --- Start delay: interruptible pause/stop ------------------------------
    let delay_until = Instant::now() + config.start_delay;
    while Instant::now() < delay_until {
        if flag.stopped() {
            report(Progress::stopped(cursor, total, !flag.manual_stop()));
            return;
        }
        match cmd_rx.recv_timeout(Duration::from_millis(25)) {
            Ok(Cmd::Stop) => {
                report(Progress::stopped(cursor, total, !flag.manual_stop()));
                return;
            }
            Ok(Cmd::Pause) => {
                paused = true;
                report(Progress::new(Phase::Paused, cursor, total));
            }
            Ok(Cmd::Resume) => {
                paused = false;
                report(Progress::new(Phase::Countdown, cursor, total));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                report(Progress::stopped(cursor, total, true));
                return;
            }
        }
    }

    // --- Acquire the destination -------------------------------------------
    // The Start button lives in this app, so at spawn the focused window is
    // always Ghostwriter itself. Typing must not begin until the user has moved
    // to the window they want written into: capturing the app window as the
    // target would abort on the very first focus change, which is the user
    // doing exactly what the countdown asked for.
    let mut target_window = None;
    if let Some(own) = app_window.as_deref() {
        match wait_for_target(
            &cmd_rx,
            &flag,
            &focus,
            &focus_window,
            own,
            None,
            &mut paused,
            cursor,
            total,
            &report,
        ) {
            TargetWait::Acquired(id) => target_window = Some(id),
            TargetWait::Stopped => return,
        }
    }

    if !paused {
        report(Progress::new(Phase::Typing, cursor, total));
    }

    // --- Typing loop --------------------------------------------------------
    while cursor < total {
        if flag.stopped() {
            report(Progress::stopped(cursor, total, !flag.manual_stop()));
            return;
        }

        if paused {
            report(Progress::new(Phase::Paused, cursor, total));
            match wait_for_resume(&cmd_rx) {
                Wait::Resume => {
                    paused = false;
                    // The click that resumed happened in Ghostwriter, so the
                    // destination lost focus. Put it back instead of making the
                    // user click the same window again.
                    if let Some(wanted) = target_window.as_deref() {
                        if focus().as_deref() != Some(wanted) && focus_window(wanted) {
                            // Activation is asynchronous: let the destination
                            // restore focus to its internal control (caret,
                            // text box) before the first keystroke arrives.
                            thread::sleep(RESTORE_SETTLE);
                        }
                    }
                }
                Wait::Stop => {
                    flag.stop();
                    report(Progress::stopped(cursor, total, !flag.manual_stop()));
                    return;
                }
            }
            continue;
        }

        // Safety net: never keep typing into a window the user switched to.
        if let Some(own) = app_window.as_deref() {
            match focus() {
                Some(current) if target_window.as_deref() == Some(current.as_str()) => {}

                // Returning to Ghostwriter is how the user reaches the
                // controls. Wait for the target instead of aborting, otherwise
                // the Stop button gets disabled by the abort before the click
                // that was meant to trigger it is even delivered.
                Some(current) if current == own => {
                    match wait_for_target(
                        &cmd_rx,
                        &flag,
                        &focus,
                        &focus_window,
                        own,
                        target_window.as_deref(),
                        &mut paused,
                        cursor,
                        total,
                        &report,
                    ) {
                        TargetWait::Acquired(_) => {}
                        TargetWait::Stopped => return,
                    }
                    continue;
                }

                Some(_) if config.abort_on_focus_change => {
                    flag.stop();
                    report(Progress::stopped_with(
                        cursor,
                        total,
                        "A janela em foco mudou. A digitação foi interrompida para não \
                         escrever no lugar errado.",
                    ));
                    return;
                }

                // Guard off: another window is an accepted destination change.
                Some(_) => {}

                // The probe failed; the documented safe failure mode is to keep
                // typing rather than abort blindly.
                None => {}
            }
        }

        let ch = chars[cursor];
        if let Err(e) = sink.emit(ch) {
            flag.stop();
            report(Progress::failed(cursor, total, e));
            return;
        }
        cursor += 1;

        // Repainting every keystroke at 40 WPM is wasted work; every fifth is
        // smooth enough for a 6 px progress bar.
        if cursor % 5 == 0 || cursor == total {
            report(Progress::new(Phase::Typing, cursor, total));
        }

        if cursor >= total {
            break;
        }

        // Sleep in slices so pause/stop are honoured mid-character.
        let delay = scaled(
            timing::char_delay(config.wpm, ch, &mut rng),
            config.delay_scale,
        );
        match sleep_unless_interrupted(&cmd_rx, &flag, delay) {
            Slept::Elapsed => {}
            Slept::Pause => paused = true,
            Slept::Stop => {
                flag.stop();
                report(Progress::stopped(cursor, total, !flag.manual_stop()));
                return;
            }
        }
    }

    report(Progress::new(Phase::Done, cursor, total));
}

fn scaled(delay: Duration, scale: f64) -> Duration {
    if scale >= 1.0 {
        delay
    } else {
        Duration::from_secs_f64(delay.as_secs_f64() * scale.max(0.0))
    }
}

/// How long to give a newly activated destination to restore its internal
/// focus before the first keystroke is injected.
const RESTORE_SETTLE: Duration = Duration::from_millis(150);

enum Slept {
    Elapsed,
    Pause,
    Stop,
}

fn sleep_unless_interrupted(
    cmd_rx: &mpsc::Receiver<Cmd>,
    flag: &RunFlag,
    delay: Duration,
) -> Slept {
    // A zero delay must not spin: still give the UI a chance to preempt.
    let until = Instant::now() + delay;
    loop {
        if flag.stopped() {
            return Slept::Stop;
        }
        let remaining = until.saturating_duration_since(Instant::now());
        let slice = remaining.min(Duration::from_millis(15));
        match cmd_rx.recv_timeout(slice) {
            Ok(Cmd::Pause) => return Slept::Pause,
            Ok(Cmd::Resume) => {}
            Ok(Cmd::Stop) => return Slept::Stop,
            Err(RecvTimeoutError::Timeout) => {
                if Instant::now() >= until {
                    return Slept::Elapsed;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Slept::Stop,
        }
    }
}

enum Wait {
    Resume,
    Stop,
}

fn wait_for_resume(cmd_rx: &mpsc::Receiver<Cmd>) -> Wait {
    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Cmd::Resume) => return Wait::Resume,
            Ok(Cmd::Pause) => {}
            Ok(Cmd::Stop) => return Wait::Stop,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Wait::Stop,
        }
    }
}

enum TargetWait {
    Acquired(String),
    Stopped,
}

/// Waits until the window that should receive the text has focus.
///
/// `expected` is used once the target is known (the user came back to the app):
/// only that same window may resume the run. Without it, the first window other
/// than `own` is accepted — that is how the destination is discovered.
#[allow(clippy::too_many_arguments)]
fn wait_for_target(
    cmd_rx: &mpsc::Receiver<Cmd>,
    flag: &RunFlag,
    focus: &FocusProbe,
    focus_window: &FocusRestore,
    own: &str,
    expected: Option<&str>,
    paused: &mut bool,
    cursor: usize,
    total: usize,
    report: &Reporter,
) -> TargetWait {
    report(Progress::new(
        if *paused {
            Phase::Paused
        } else {
            Phase::Countdown
        },
        cursor,
        total,
    ));

    loop {
        if flag.stopped() {
            report(Progress::stopped(cursor, total, !flag.manual_stop()));
            return TargetWait::Stopped;
        }

        if !*paused {
            if let Some(current) = focus() {
                let is_wanted = match expected {
                    Some(wanted) => current == wanted,
                    // Only a change away from the app tells us the user has
                    // picked a destination.
                    None => current != own,
                };
                if is_wanted {
                    return TargetWait::Acquired(current);
                }
            }
        }

        match cmd_rx.recv_timeout(Duration::from_millis(25)) {
            Ok(Cmd::Pause) => {
                *paused = true;
                report(Progress::new(Phase::Paused, cursor, total));
            }
            Ok(Cmd::Resume) => {
                *paused = false;
                // Resuming from the app should put the destination back in
                // front, not wait for a click the user already made once.
                if let Some(wanted) = expected {
                    if focus_window(wanted) {
                        // Let the destination process activation and restore
                        // its internal focus before typing resumes.
                        thread::sleep(RESTORE_SETTLE);
                    }
                }
                report(Progress::new(Phase::Countdown, cursor, total));
            }
            Ok(Cmd::Stop) => {
                report(Progress::stopped(cursor, total, !flag.manual_stop()));
                return TargetWait::Stopped;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                report(Progress::stopped(cursor, total, true));
                return TargetWait::Stopped;
            }
        }
    }
}

/* ---------------------------------- tests -------------------------------- */

#[cfg(test)]
mod tests {
    //! Unit tests for the pieces the integration tests cannot reach: internal
    //! helpers, edge cases that are impractical to trigger through the public
    //! API, and the string contract shared with the frontend.
    //!
    //! End-to-end behaviour (typing order, pause, stop, resume, focus guard,
    //! error paths) lives in `tests/engine_e2e.rs` and drives the real threads.

    use super::*;

    #[test]
    fn phase_strings_match_the_frontend_contract() {
        // These literals are consumed by ui/app.js and pinned by ui-tests.
        assert_eq!(Phase::Countdown.as_str(), "countdown");
        assert_eq!(Phase::Typing.as_str(), "typing");
        assert_eq!(Phase::Paused.as_str(), "paused");
        assert_eq!(Phase::Done.as_str(), "done");
        assert_eq!(Phase::Stopped.as_str(), "stopped");
        assert_eq!(Phase::Failed.as_str(), "failed");
    }

    #[test]
    fn run_flag_round_trip() {
        let flag = RunFlag::default();
        assert!(!flag.stopped(), "a fresh run must start unstopped");
        assert!(!flag.manual_stop(), "a fresh run is not a manual stop");

        flag.stop();
        assert!(flag.stopped());
        assert!(
            !flag.manual_stop(),
            "an internal safety stop must stay resumable"
        );

        flag.reset();
        assert!(!flag.stopped());

        flag.stop_manually();
        assert!(flag.stopped());
        assert!(flag.manual_stop(), "the UI Stop is final");
    }

    #[test]
    fn run_flag_is_shared_between_clones() {
        let flag = RunFlag::default();
        let clone = flag.clone();

        clone.stop_manually();
        assert!(flag.stopped(), "clones must observe the same state");
        assert!(flag.manual_stop(), "clones must share the stop intent");
    }

    #[test]
    fn delay_scale_shortens_but_never_negates() {
        let d = Duration::from_millis(200);
        assert_eq!(scaled(d, 1.0), d, "a scale of 1 must be a no-op");
        assert_eq!(scaled(d, 0.0), Duration::ZERO);
        assert_eq!(scaled(d, 0.5), Duration::from_millis(100));
        // A negative scale must not panic or produce a bogus duration.
        assert_eq!(scaled(d, -3.0), Duration::ZERO);
    }

    #[test]
    fn progress_constructors_set_the_expected_shape() {
        let ok = Progress::new(Phase::Typing, 3, 10);
        assert_eq!(ok.phase, Phase::Typing);
        assert_eq!(ok.done, 3);
        assert_eq!(ok.total, 10);
        assert!(ok.error.is_none());
        assert!(!ok.resumable, "mid-run progress is not a resumable stop");

        let bad = Progress::failed(1, 10, "falhou".into());
        assert_eq!(bad.phase, Phase::Failed);
        assert_eq!(bad.error.as_deref(), Some("falhou"));
        assert!(bad.resumable, "a failure can be continued");

        let automatic = Progress::stopped(4, 10, true);
        assert_eq!(automatic.phase, Phase::Stopped);
        assert!(automatic.resumable);

        let manual = Progress::stopped(4, 10, false);
        assert!(!manual.resumable, "a deliberate stop ends the run");
    }

    #[test]
    fn empty_text_is_a_valid_config() {
        let config = EngineConfig::new("", 60);
        assert_eq!(config.text.chars().count(), 0);
        assert_eq!(config.delay_scale, 1.0, "production default");
        assert!(!config.abort_on_focus_change, "guard is opt-in");
    }

    #[test]
    fn skip_beyond_the_end_is_clamped() {
        // The cursor is clamped in `run`; check the arithmetic it relies on.
        let chars = "abc".chars().count();
        assert_eq!(99usize.min(chars), 3);
    }
}
