//! Thin wrapper over `enigo` so the rest of the app never touches input APIs.
//!
//! One Typer must live on the thread that types: it holds the connection to the
//! platform input stack, and that connection is not meant to be shared.

use enigo::{Direction, Enigo, Key, Keyboard, Settings};

pub struct Typer {
    enigo: Enigo,
}

impl Typer {
    pub fn new() -> Result<Self, String> {
        let enigo = Enigo::new(&Settings::default()).map_err(describe)?;
        Ok(Self { enigo })
    }

    /// Emits a single character as a real keystroke.
    ///
    /// Accented characters are sent as Unicode events on Windows
    /// (`KEYEVENTF_UNICODE`), which is independent of the active keyboard
    /// layout. On X11 enigo maps the char to a keysym, so a few exotic
    /// characters can be dropped depending on the layout.
    pub fn type_char(&mut self, ch: char) -> Result<(), String> {
        match ch {
            // `text()` treats a literal newline inconsistently across backends,
            // so line breaks are sent as a real Return key press.
            '\n' => self.press(Key::Return),
            // A carriage return in the middle of text is a paste artefact.
            '\r' => Ok(()),
            '\t' => self.press(Key::Tab),
            other => {
                let mut buf = [0u8; 4];
                self.enigo
                    .text(other.encode_utf8(&mut buf))
                    .map_err(describe)
            }
        }
    }

    fn press(&mut self, key: Key) -> Result<(), String> {
        self.enigo.key(key, Direction::Click).map_err(describe)
    }
}

fn describe<E: std::fmt::Debug>(err: E) -> String {
    format!("{err:?}")
}

/// Keyboard layout / session problems that are worth explaining to the user.
#[cfg(target_os = "linux")]
pub fn platform_hint() -> Option<&'static str> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    match (wayland, x11) {
        // Under Wayland with XWayland, DISPLAY exists but XTest only reaches
        // X11 clients; native Wayland windows silently receive nothing.
        (true, true) => Some(
            "Sessão Wayland detectada. A digitação atinge apenas janelas X11 (XWayland). \
             Se o app de destino for Wayland nativo, compile com --features linux-wayland \
             ou use uma sessão X11.",
        ),
        (true, false) => Some(
            "Sessão Wayland sem XWayland: a injeção de teclas X11 não funciona. \
             Compile com --features linux-wayland ou --no-default-features --features linux-libei.",
        ),
        (false, false) => Some("Nenhum DISPLAY encontrado: não há servidor gráfico acessível."),
        _ => None,
    }
}

#[cfg(not(target_os = "linux"))]
pub fn platform_hint() -> Option<&'static str> {
    None
}
