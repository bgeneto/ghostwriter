<div align="center">

<img src="app-icon.png" alt="Ghostwriter" width="120" height="120" />

# Ghostwriter

**Type any text into the app that has focus — at the speed of a human.**

Paste a script, pick a typing speed, press Start, and click into the window where
it should appear. Ghostwriter types it out character by character, 60 to 300 words
per minute, and stops the instant you tell it to.

[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange?logo=rust)](https://www.rust-lang.org)
[![Tauri](https://img.shields.io/badge/tauri-2.x-24C8DB?logo=tauri)](https://v2.tauri.app)
[![Windows](https://img.shields.io/badge/Windows-10%2B-0078D6?logo=windows)](#)
[![Linux](https://img.shields.io/badge/Linux-X11%20%7C%20Wayland-FCC624?logo=linux&logoColor=black)](#linux-notes)
[![License](https://img.shields.io/badge/license-MIT-blue)](#license)

[How it works](#how-it-works) · [Install](#install) · [Usage](#usage) ·
[Linux notes](#linux-notes) · [Development](#development) · [Testing](#testing)

</div>

---

## Why

Screencasts, live coding, demos and streams all look wrong when text appears
instantly. Pasting gives you a wall of characters; typing by hand is slow and
error-prone. Ghostwriter sits in between: you supply the text once, it enters the
target application one keystroke at a time, with the rhythm of someone actually
typing.

Because it injects real OS-level keystrokes rather than simulating them inside
its own window, it works with anything that accepts keyboard input — editors,
browsers, chat clients, terminals, slide decks.

## Features

- **Real keystrokes, not a text animation.** Text arrives in whatever app has
  focus, including accented characters (`ã ç é õ ê`) and line breaks.
- **Human pacing.** 60–300 WPM, with ±15% jitter per character, shorter spaces,
  and extra pauses after commas and sentence endings.
- **Focus guard.** Typing only begins once you focus the destination, and — when
  the guard is enabled — stops if you switch to a third window mid-run. Returning
  to Ghostwriter pauses instead of aborting, so the controls stay usable.
  Pressing *Retomar* puts the destination back in front and continues there
  (Windows and X11; elsewhere it waits for the click).
- **Global shortcuts.** Pause and cancel from anywhere — even when another
  application owns the keyboard.
- **Pause and cancel are different.** *Pausar* holds the run and *Retomar*
  continues from the same character, restoring the destination's focus.
  *Cancelar* ends the run without clearing the text; the next start begins from
  the beginning. After an automatic interruption (a window change or an input
  error), *Cancelar* discards the resume point so you can start over.
- **Countdown before typing.** Three seconds by default, so you can focus the
  target window. Ghostwriter waits for that switch before the first keystroke.
- **No bundler, no `node_modules`.** The UI is plain HTML/CSS/JS served straight
  from disk; the whole app is one Rust binary plus the system webview.
- **Small.** A 3.5 MB executable — no bundled runtime, no interpreter, no
  `node_modules` to ship.

## How it works

```
┌──────────────────┐   IPC     ┌───────────────────┐  own thread
│  WebView (UI)    │ ────────► │  Tauri (Rust)     │ ─────────────► enigo
│  HTML/CSS/JS     │ ◄──────── │  engine + state   │ ◄───────────── OS keyboard
└──────────────────┘  events   └───────────────────┘
```

The webview holds no timing logic. It sends intents (`start`, `pause`, `stop`)
and renders the snapshots the Rust engine emits on `engine://progress`.

The engine runs on its own thread because the typing loop is sequential and
blocking, and because the connection to the platform input stack must stay on a
single thread. Progress is counted in **characters**, never bytes, so `ção` is
three steps on the progress bar rather than five.

### Choosing the destination

Pressing Start always happens inside Ghostwriter, so the app cannot assume that
the focused window is the target. It remembers its own window, waits until focus
moves elsewhere, and only then records the destination and starts typing.
Moving back to Ghostwriter pauses the run — that is how the Pause and Stop
buttons stay reachable while a run is in flight — while moving to any other
window stops it (when the guard is enabled) and explains why. Pressing *Retomar*
brings the destination back to the foreground and continues there (Windows and
X11; elsewhere it waits for a click). The handshake and the restore work even
with the checkbox off; the checkbox only controls the third-window abort. A
deliberate *Cancelar* ends the run and keeps the text; only automatic
interruptions stay resumable, and *Cancelar* discards that resume point.
The global shortcuts work regardless of which window has focus.

### Timing model

A "word" is the standard five characters, so the base delay is
`60 000 / (WPM × 5)` milliseconds:

| Speed | Per character |
| ----: | ------------: |
| 60 WPM | 200 ms |
| 100 WPM | 120 ms |
| 160 WPM | 75 ms |
| 300 WPM | 40 ms |

That base is then shaped:

| Input | Effect |
| --- | --- |
| space | ×0.75 — spaces are the fastest keystrokes people make |
| `,` `;` `:` `—` | +90 ms |
| `.` `!` `?` `…` | +180 ms |
| newline | ×3 +180 ms |
| everything else | a uniform ±15% jitter |

On X11 the `enigo` backend adds a small fixed delay per keystroke, so the
effective ceiling is a little below the selected WPM. Windows injects through
`SendInput` without that overhead.

## Install

Download from [Releases](../../releases):

| Platform | Artifact | Size | Notes |
| --- | --- | ---: | --- |
| Windows 10/11 | `Ghostwriter-<version>-x64-portable.exe` | ~3.5 MB | **No install** — just run it |
| Windows 10/11 | `Ghostwriter-<version>-x64-portable.zip` | ~1.4 MB | Portable + `README.txt` |
| Windows 10/11 | `Ghostwriter-<version>-x64.msi` | ~1.7 MB | WiX installer |
| Windows 10/11 | `Ghostwriter-<version>-x64.setup.exe` | ~1.2 MB | NSIS installer |
| Debian / Ubuntu | `ghostwriter_<version>_amd64.deb` | | |
| Any Linux | `ghostwriter_<version>_amd64.AppImage` | | |

Every Windows build needs the **WebView2** runtime. It ships with Windows 11 and
is present on almost all Windows 10 machines; if the window comes up blank,
install it once from
[microsoft.com/edge/webview2](https://developer.microsoft.com/microsoft-edge/webview2/).

### The portable build

The portable executable is the plain release binary — Tauri has no separate
"portable" bundle target, and none is needed: the app is a single `.exe` with no
side files, no registry writes and no service. Copy it anywhere, including a USB
drive, and double-click it.

Preferences (speed, countdown, focus guard) are stored as JSON in the platform
config directory:

| Platform | Location |
| --- | --- |
| Windows | `%APPDATA%\dev.ghostwriter.desktop\settings.json` |
| Linux | `~/.config/dev.ghostwriter.desktop/settings.json` |

A corrupt or unreadable file falls back to defaults rather than blocking
startup, and values are clamped to their supported range on read, so
hand-editing it is safe.

### Building the artifacts yourself

```sh
pwsh -File scripts/dist.ps1              # portable exe + zip + msi + setup.exe
pwsh -File scripts/dist.ps1 -SkipBundle  # portable only, much faster
```

Output lands in `dist/`. The MSI target needs the Windows *VBSCRIPT* optional
feature (enabled by default on nearly every machine); if `light.exe` fails, turn
it on under *Settings → Apps → Optional features*.

The Linux equivalents come from the same CLI inside a Linux session:

```sh
cargo tauri build --bundles deb appimage
```


## Usage

1. **Paste** your text — the *Colar da área de transferência* button reads the
   clipboard, and the text box is editable if you want to fix something.
2. **Pick a speed** between 60 and 300 WPM (60 is the default) and how long the
   countdown should last.
3. **Press Iniciar**, then focus the window where the text should appear. The
   countdown gives you time to switch; typing only starts once the destination
   has focus, so the first keystroke never lands in the wrong place.
4. **Pausar** holds the run; **Retomar** restores the destination's focus and
   continues from the same character. **Cancelar** ends the run without clearing
   the text, so the next start begins from the beginning. After an automatic
   interruption, **Cancelar** discards the resume point. The global shortcuts do
   the same from any window.

| Action | Shortcut |
| --- | --- |
| Pause / resume | <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>P</kbd> |
| Cancel | <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>S</kbd> |

Both work with another application in the foreground — that is the point: you
start the run, focus the target window, and can still cancel it.

The text box is locked while a run is active, so the progress bar cannot drift
out of sync with the content. If you edit after an interruption, the resume point
is discarded and the next run starts from the beginning.

## Linux notes

Window creation and keystroke injection are separate problems on Wayland: a
compositor will happily show your window while refusing to let you send input to
another one.

| Session | Status |
| --- | --- |
| X11 | Works out of the box |
| Wayland + XWayland | Types into X11 windows; the app warns about native Wayland ones |
| Wayland native, wlroots (Sway, Hyprland, Wayfire) | `--features linux-wayland` *(experimental)* |
| GNOME 46+ / KDE 6.1+ Wayland | `--no-default-features --features linux-libei` *(experimental)* |

The `libei` backend routes input through the `xdg-desktop-portal`
`RemoteDesktop` interface, which prompts for consent on first use and then
remembers the grant. **Do not combine `libei` with the default `x11rb` backend**:
under XWayland both succeed and every keystroke is injected twice.

The focus guard needs an OS API to read the focused window. Windows, X11, KDE and
Hyprland provide one; generic Wayland does not. Where it is unavailable the
checkbox is disabled rather than pretending to protect you.

Pressing *Retomar* also restores the destination's focus, on Windows and X11.
Where the session cannot do that, Ghostwriter waits for you to click the
destination instead of typing into whatever happens to be in front.

## Development

### Prerequisites

- Rust 1.85 or newer
- Linux only: the usual Tauri system packages

```sh
# Debian / Ubuntu
sudo apt install libwebkit2gtk-4.1-dev libxdo-dev libxkbcommon-dev \
                 libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev
```

### Run

```sh
cargo run --manifest-path src-tauri/Cargo.toml
```

There is no frontend build step — Tauri serves `ui/` directly.

### Layout

```
ui/                          frontend (plain HTML/CSS/JS)
  index.html
  styles.css
  app.js
scripts/
  dist.ps1                   builds portable exe/zip + msi + setup.exe
src-tauri/
  src/main.rs                binary entry point
  src/lib.rs                 IPC commands, global shortcuts, app state
  src/engine.rs              typing state machine
  src/settings.rs            preferences, persisted as JSON
  src/timing.rs              WPM math, jitter, punctuation
  src/typer.rs               enigo wrapper
  src/window.rs              active-window lookup (focus guard)
  tests/engine_e2e.rs        end-to-end state machine tests
  tests/e2e_windows_input.rs real Win32 EDIT control
  tests/e2e_linux_input.rs   real GTK3 GtkTextView
  tests/support/             GTK test target (C)
ui-tests/                    UI tests against a fake IPC surface
```

### Build an installer

```sh
pwsh -File scripts/dist.ps1
```

or drive the CLI directly:

```sh
cargo tauri build                 # bundled installers
cargo tauri build --no-bundle     # just the portable binary
```

Artifacts land in `dist/` when using the script, or in
`src-tauri/target/release/bundle/` (installers) plus
`src-tauri/target/release/` (the raw binary) when calling the CLI directly.
Icons are generated from `app-icon.png` (1024×1024) with
`cargo tauri icon app-icon.png`.

## Testing

Three layers, each covering what the one below cannot.

```sh
# 1. Unit — timing math, edge cases, the string contract with the UI
cargo test --manifest-path src-tauri/Cargo.toml --lib

# 2. Integration — the real engine with real threads, fake input sink
cargo test --manifest-path src-tauri/Cargo.toml --test engine_e2e

# 3. End-to-end — real keystrokes into a real toolkit widget
cargo test --manifest-path src-tauri/Cargo.toml --test e2e_windows_input -- --test-threads=1
cargo test --manifest-path src-tauri/Cargo.toml --test e2e_linux_input   -- --test-threads=1

# 4. UI — the real app.js against a fake Tauri IPC
cd ui-tests && npm install && npm test
```

| Layer | Count | What it proves |
| --- | ---: | --- |
| Unit | 28 | WPM table, jitter bounds, phase names, settings persistence, IPC casing |
| Integration | 21 | Ordering, pause, stop, resume, focus restore, target acquisition, errors |
| E2E input | 5 + 6 | Accents and newlines survive real injection; focus restore |
| UI | 28 | Command names, argument shapes, event contract, pause/stop semantics |

All of it is green on Windows 11 and on Debian 13 under WSLg. The Windows suite
was run four times back-to-back to shake out the timing races that plague input
tests; the two that surfaced were both test bugs, not app bugs, and are
documented in the code where they were fixed.

### What the end-to-end tests are really for

Accented text is where naive input code fails: a byte-oriented loop, a keysym
mapping that only covers ASCII, or a newline sent as `U+000A` instead of a Return
press all look fine until someone types *coração*.

So the Windows test creates a real window with a real `EDIT` control, focuses it
and lets the production `Typer` type
`Ação, coração e você!⏎Não é fácil…`, then reads the result back with
`GetWindowTextW`. The Linux test does the same against a GTK3 `GtkTextView`,
which is closer to what the app actually targets, and checks the entire
Portuguese repertoire — `áàâãäéèêëíìîïóòôõöúùûüçÁÀÂÃÄÉÈÊËÍÌÎÏÓÒÔÕÖÚÙÛÜÇñÑ`.

Two details make these tests trustworthy rather than decorative:

- **Feedback comes from the toolkit, not from us.** The GTK target writes its
  buffer to a state file 20 times a second; assertions read what the widget
  received, not what the engine believes it sent.
- **Focus is taken explicitly.** `SetForegroundWindow` is refused when the
  calling process is not already in the foreground, so the Windows test attaches
  to the foreground thread's input queue. The Linux test calls
  `set_input_focus` on the X server and forces `GDK_BACKEND=x11`, because under
  WSLg GTK otherwise picks Wayland and the window becomes invisible to XTest.

Both skip themselves with a printed reason when no interactive session is
available, so CI without a desktop stays green without lying.

### Continuous integration

`.github/workflows/ci.yml` runs on every push and pull request:

| Job | Runner | What it does |
| --- | --- | --- |
| `check` | Ubuntu | `rustfmt`, `clippy -D warnings`, unit + integration tests |
| `ui-tests` | Ubuntu | the UI contract suite via `npm ci` |
| `e2e-windows` | windows-latest | real keystrokes into a real `EDIT` control |
| `e2e-linux` | Ubuntu | `Xvfb` + `openbox`, then real keystrokes into a GTK widget |
| `build-windows` | windows-latest | portable exe, zip, MSI, NSIS |
| `build-linux` | Ubuntu | `.deb` and `.AppImage` |
| `release` | Ubuntu | publishes a **draft** release on `v*` tags |

Two details worth knowing:

- **The portable build is launch-tested.** `build-windows` starts
  `Ghostwriter-*-portable.exe`, waits for a window title to appear, checks the
  process is alive and then terminates it. A portable binary that does not open
  fails the build instead of reaching a release.
- **Artifacts are magic-byte checked.** A truncated MSI or a mislabelled
  installer is caught before upload, because that class of defect is invisible
  until someone tries to install.

The Linux E2E job needs a window manager, not just a display: `Xvfb` alone has no
focus model, so `openbox` runs alongside it. The whole recipe was rehearsed
locally under a bare Xvfb display before being committed.

## License

MIT.