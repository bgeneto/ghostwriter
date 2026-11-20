//! End-to-end test: real keystroke injection on Linux/X11.
//!
//! The Windows counterpart (`e2e_windows_input.rs`) creates an EDIT control
//! through the Win32 API. Here the target is a GTK3 window holding a real
//! `GtkTextView`, built by `tests/support/gtk_target.c` and run as a separate
//! process, because GTK requires the main thread while `cargo test` hands each
//! test a worker thread.
//!
//! The target rewrites a state file 20 times a second with its buffer contents,
//! so assertions read exactly what the toolkit received.
//!
//! This test proves the two things that break naive Linux input code:
//!   * accented characters, which need correct keysym mapping;
//!   * line breaks, which need a real Return press and a multi-line widget.
//!
//! Requirements: a reachable X11 display, GTK3 dev headers, and a window manager
//! that hands out focus — WSLg and any desktop session qualify. Otherwise the
//! test skips itself and says so.

#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::Recorder;
use ghostwriter_lib::engine::{spawn, EngineConfig, Phase};
use ghostwriter_lib::typer::Typer;

/// The GTK target must run on X11 for XTest to reach it. Under WSLg and other
/// XWayland setups GTK defaults to the Wayland backend, which would make the
/// window invisible to the injection path the app actually uses.
const FORCE_X11: (&str, &str) = ("GDK_BACKEND", "x11");

/* ------------------------------ environment ----------------------------- */

fn display_available() -> bool {
    // A display name alone is not enough: under WSL the variable can be set
    // while the socket is gone. Ask the X server directly.
    if std::env::var_os("DISPLAY").is_none() {
        return false;
    }
    Command::new("xdpyinfo")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Builds the GTK target on first use, returning its path.
fn ensure_target() -> Option<PathBuf> {
    let support = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support");
    let source = support.join("gtk_target.c");
    let binary = support.join("gtk_target");

    // Rebuild when the binary is missing or older than its source.
    let stale = match (std::fs::metadata(&binary), std::fs::metadata(&source)) {
        (Ok(bin), Ok(src)) => match (bin.modified(), src.modified()) {
            (Ok(b), Ok(s)) => b < s,
            _ => true,
        },
        _ => true,
    };

    if stale {
        let script = support.join("build_gtk_target.sh");
        let output = Command::new("bash")
            .arg(&script)
            .arg(&binary)
            .output()
            .ok()?;
        if !output.status.success() {
            eprintln!(
                "skipping: could not build the GTK target\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return None;
        }
    }

    binary.exists().then_some(binary)
}

/* -------------------------------- target -------------------------------- */

/// Owns the GTK target process and its state file.
struct GtkTarget {
    child: Child,
    state: PathBuf,
}

impl GtkTarget {
    fn start(binary: &Path) -> Option<Self> {
        let state = std::env::temp_dir().join(format!(
            "ghostwriter-e2e-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_file(&state);

        let child = Command::new(binary)
            .arg(&state)
            .env(FORCE_X11.0, FORCE_X11.1)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let target = Self { child, state };
        target
            .wait_until_ready(Duration::from_secs(15))
            .then_some(target)
    }

    /// `(status, title, xid, payload)`.
    fn read_state(&self) -> Option<(String, String, u32, String)> {
        let raw = std::fs::read_to_string(&self.state).ok()?;
        let mut parts = raw.splitn(4, '\n');
        let status = parts.next()?.to_string();
        let title = parts.next().unwrap_or_default().to_string();
        let xid = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
        let text = parts.next().unwrap_or_default().to_string();
        Some((status, title, xid, text))
    }

    fn wait_until_ready(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some((status, _, xid, _)) = self.read_state() {
                // Ready means mapped *and* we know the window to focus.
                if status == "ready" && xid != 0 {
                    return true;
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn xid(&self) -> Option<u32> {
        match self.read_state() {
            Some((_, _, xid, _)) if xid != 0 => Some(xid),
            _ => None,
        }
    }

    /// Buffer contents as the toolkit sees them.
    fn typed(&self) -> String {
        self.read_state()
            .map(|(_, _, _, text)| text)
            .unwrap_or_default()
    }

    /// Waits until the buffer stops changing, so assertions do not race the
    /// target's 50 ms write interval.
    fn settle(&self, quiet_for: Duration, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let mut last = self.typed();
        let mut stable_since = Instant::now();
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(30));
            let now = self.typed();
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

/// Gives the target keyboard focus.
///
/// The window manager cannot be trusted to hand out focus on its own — under
/// Weston/WSLg a freshly mapped window often never becomes the active one.
/// `set_input_focus` on the X server is immediate, deterministic and needs no
/// window manager cooperation, which is exactly what a test wants.
fn focus_window(xid: u32) -> Result<(), String> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConnectionExt, InputFocus};

    let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("connect: {e}"))?;
    let screen = &conn.setup().roots[screen_num];
    let _ = screen;

    // Raise it first so it is not hidden behind anything, then focus it.
    let _ = conn.configure_window(
        xid,
        &x11rb::protocol::xproto::ConfigureWindowAux::new()
            .stack_mode(x11rb::protocol::xproto::StackMode::ABOVE),
    );
    conn.set_input_focus(InputFocus::PARENT, xid, x11rb::CURRENT_TIME)
        .map_err(|e| format!("set_input_focus: {e}"))?
        .check()
        .map_err(|e| format!("set_input_focus check: {e}"))?;
    conn.flush().map_err(|e| format!("flush: {e}"))?;

    // Give the toolkit a moment to process the focus event.
    thread::sleep(Duration::from_millis(150));
    Ok(())
}

impl Drop for GtkTarget {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.state);
    }
}

fn prepare(context: &str) -> Option<GtkTarget> {
    if !display_available() {
        eprintln!("skipping {context}: no reachable X11 display");
        return None;
    }
    let binary = ensure_target()?;
    let target = match GtkTarget::start(&binary) {
        Some(t) => t,
        None => {
            eprintln!("skipping {context}: the GTK target did not become ready");
            return None;
        }
    };

    let xid = match target.xid() {
        Some(x) => x,
        None => {
            eprintln!("skipping {context}: the target reported no window id");
            return None;
        }
    };
    if let Err(e) = focus_window(xid) {
        eprintln!("skipping {context}: could not focus the target window: {e}");
        return None;
    }

    Some(target)
}

/* --------------------------------- tests -------------------------------- */

#[test]
fn types_accents_and_line_breaks_into_a_real_gtk_widget() {
    let Some(target) = prepare("accent + newline test") else {
        return;
    };

    let expected = "Ação, coração e você!\nNão é fácil…";

    let mut config = EngineConfig::new(expected, 80);
    config.delay_scale = 0.05;

    let rec = Recorder::new();
    let _handle = spawn(config, rec.callback());

    assert!(
        rec.wait_for_phase(Duration::from_secs(30), Phase::Done),
        "engine never reached Done: {:?}",
        rec.last()
    );
    if let Some(err) = rec.last().unwrap().error {
        panic!("engine reported a failure: {err}");
    }

    let typed = target.settle(Duration::from_millis(300), Duration::from_secs(5));
    assert_eq!(
        typed, expected,
        "\n  expected: {expected:?}\n  actually typed: {typed:?}"
    );
}

#[test]
fn every_portuguese_accented_character_arrives_intact() {
    let Some(target) = prepare("charset test") else {
        return;
    };

    // The full Portuguese repertoire, including characters that need a dead key
    // or a compose sequence on a US layout.
    let expected = "áàâãäéèêëíìîïóòôõöúùûüçÁÀÂÃÄÉÈÊËÍÌÎÏÓÒÔÕÖÚÙÛÜÇñÑ";

    let mut config = EngineConfig::new(expected, 80);
    config.delay_scale = 0.03;

    let rec = Recorder::new();
    let _handle = spawn(config, rec.callback());

    assert!(
        rec.wait_for_phase(Duration::from_secs(60), Phase::Done),
        "engine never reached Done: {:?}",
        rec.last()
    );

    let typed = target.settle(Duration::from_millis(300), Duration::from_secs(5));
    assert_eq!(
        typed, expected,
        "\n  expected: {expected:?}\n  actually typed: {typed:?}"
    );
}

#[test]
fn punctuation_and_digits_arrive_exactly() {
    let Some(target) = prepare("punctuation test") else {
        return;
    };

    let expected = "Olá! Tudo bem? Sim: 42, 7; 3. — fim.";

    let mut config = EngineConfig::new(expected, 80);
    config.delay_scale = 0.04;

    let rec = Recorder::new();
    let _handle = spawn(config, rec.callback());

    assert!(
        rec.wait_for_phase(Duration::from_secs(45), Phase::Done),
        "engine never reached Done: {:?}",
        rec.last()
    );

    let typed = target.settle(Duration::from_millis(300), Duration::from_secs(5));
    assert_eq!(
        typed, expected,
        "\n  expected: {expected:?}\n  actually typed: {typed:?}"
    );

    let last = rec.last().unwrap();
    assert_eq!(last.done, expected.chars().count());
    assert_eq!(last.total, expected.chars().count());
}

#[test]
fn stop_is_honoured_while_typing_into_a_real_window() {
    let Some(target) = prepare("stop test") else {
        return;
    };

    let full = "palavra ".repeat(80);
    let mut config = EngineConfig::new(full.clone(), 60);
    config.delay_scale = 0.6;

    let rec = Recorder::new();
    let handle = spawn(config, rec.callback());

    assert!(
        rec.wait_for(Duration::from_secs(10), |p| p.phase == Phase::Typing
            && p.done > 0),
        "engine never started typing"
    );

    handle.stop();
    assert!(
        rec.wait_for_phase(Duration::from_secs(5), Phase::Stopped),
        "engine did not acknowledge the stop"
    );

    let after_stop = target.settle(Duration::from_millis(400), Duration::from_secs(3));
    thread::sleep(Duration::from_millis(400));
    assert_eq!(
        target.typed(),
        after_stop,
        "characters kept arriving after the stop command"
    );
    assert!(
        after_stop.chars().count() < full.chars().count(),
        "the full text was typed despite the stop"
    );
}

/// Verifies the production `Typer` can connect at all on this session.
///
/// This is the check that catches a missing XTest extension, or a Wayland
/// session with no XWayland, before the user ever presses Start.
#[test]
fn the_production_typer_can_connect_to_the_session() {
    if !display_available() {
        eprintln!("skipping: no reachable X11 display");
        return;
    }

    let mut typer = match Typer::new() {
        Ok(t) => t,
        Err(e) => panic!("Typer::new failed on an X11 session: {e}"),
    };

    // A single harmless character proves the connection works end to end.
    // A space is safe to emit into whatever window happens to be focused.
    typer
        .type_char(' ')
        .expect("the typer could not emit a character");
}

#[test]
fn the_platform_hint_matches_the_session() {
    let hint = ghostwriter_lib::typer::platform_hint();
    let has_x11 = std::env::var_os("DISPLAY").is_some();
    let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();

    if has_x11 && !has_wayland {
        assert!(
            hint.is_none(),
            "unexpected warning on a pure X11 session: {hint:?}"
        );
    }
    if has_x11 && has_wayland {
        assert!(
            hint.is_some(),
            "an XWayland session must warn that only X11 windows are reachable"
        );
        assert!(hint.unwrap().contains("Wayland"));
    }
    if !has_x11 {
        assert!(
            hint.is_some(),
            "a session with no DISPLAY must warn the user"
        );
    }
}
