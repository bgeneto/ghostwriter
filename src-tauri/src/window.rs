//! Active-window lookup and focus restoration.
//!
//! On Wayland this is only partially reliable: GNOME/Mutter expose no portable
//! way to read the focused window, while KDE and Hyprland do. When the lookup
//! fails the engine simply keeps typing instead of aborting blindly, which is
//! the safer failure mode for a tool the user started on purpose.

/// Part of an active-window id before the `#`, which is the native window
/// handle; the part after it is the process id.
fn window_part(id: &str) -> &str {
    id.split('#').next().unwrap_or(id)
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
pub fn active_id() -> Option<String> {
    active_win_pos_rs::get_active_window()
        .ok()
        .map(|w| format!("{}#{}", w.window_id, w.process_id))
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn active_id() -> Option<String> {
    None
}

/// Human-readable name of the focused window, for the UI.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub fn active_description() -> Option<String> {
    active_win_pos_rs::get_active_window().ok().map(|w| {
        let title = w.title.trim();
        if title.is_empty() {
            w.app_name
        } else {
            format!("{} — {}", w.app_name, title)
        }
    })
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn active_description() -> Option<String> {
    None
}

/* ----------------------------- focus restore ----------------------------- */

/// Brings the window identified by [`active_id`] back to the foreground.
///
/// Used when the user clicks *Retomar* in Ghostwriter: the click moved focus to
/// this app, so without this the destination would silently stay in the
/// background and the run would appear to be stuck.
///
/// Returns `false` when the platform refuses or the id cannot be parsed; the
/// engine then falls back to waiting for the user to focus the destination.
#[cfg(target_os = "windows")]
pub fn focus_id(id: &str) -> bool {
    use core::ffi::c_void;

    #[link(name = "user32")]
    extern "system" {
        fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
        fn IsIconic(hwnd: *mut c_void) -> i32;
        fn ShowWindow(hwnd: *mut c_void, cmd: i32) -> i32;
    }

    // `SW_RESTORE`: only applied to a minimized window, so a maximized target
    // keeps its size.
    const SW_RESTORE: i32 = 9;

    let Some(raw) = parse_hwnd(id) else {
        return false;
    };
    let hwnd = raw as *mut c_void;

    // SAFETY: the handle is validated only for being non-null and well-formed;
    // Win32 returns failure for a window that has since been destroyed.
    unsafe {
        if IsIconic(hwnd) != 0 {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd) != 0
    }
}

#[cfg(target_os = "windows")]
fn parse_hwnd(id: &str) -> Option<isize> {
    let raw = window_part(id);
    let value = raw.strip_prefix("HWND(")?.strip_suffix(')')?;
    // The windows crate's Debug impl prints the handle as a decimal isize
    // (`HWND(2166774)`); accept a 0x prefix too so a format change does not
    // silently break focus restoration.
    let (digits, radix) = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => (hex, 16),
        None => (value, 10),
    };
    isize::from_str_radix(digits, radix)
        .ok()
        .filter(|&h| h != 0)
}

#[cfg(target_os = "linux")]
pub fn focus_id(id: &str) -> bool {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt, InputFocus, StackMode};

    let Some(xid) = parse_xid(id) else {
        return false;
    };
    let Ok((conn, _screen)) = x11rb::connect(None) else {
        return false;
    };

    // Refuse ids that are not usable X11 windows: on a Wayland session the
    // active-window crate can return an id from another protocol, and focusing
    // an unrelated XID would be worse than waiting for a click.
    let exists = conn
        .get_window_attributes(xid)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .is_some();
    if !exists {
        return false;
    }

    // Raise first so focusing also brings an obscured destination forward.
    let _ = conn.configure_window(xid, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
    let focused = match conn.set_input_focus(InputFocus::PARENT, xid, x11rb::CURRENT_TIME) {
        Ok(cookie) => cookie.check().is_ok(),
        Err(_) => false,
    };
    let _ = conn.flush();
    focused
}

#[cfg(target_os = "linux")]
fn parse_xid(id: &str) -> Option<u32> {
    window_part(id).parse().ok().filter(|&x| x != 0)
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn focus_id(_id: &str) -> bool {
    false
}

/* --------------------------------- tests --------------------------------- */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_part_stops_at_the_process_id() {
        assert_eq!(window_part("abc#123"), "abc");
        assert_eq!(window_part("abc"), "abc");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn parses_the_debug_format_of_an_hwnd() {
        // The real id from active-win-pos-rs: decimal, not hexadecimal.
        assert_eq!(parse_hwnd("HWND(2166774)#99"), Some(2_166_774));
        assert_eq!(parse_hwnd("HWND(0x0000000000012345)#99"), Some(0x12345));
        assert_eq!(parse_hwnd("HWND(0)#99"), None);
        assert_eq!(parse_hwnd("not-a-handle#99"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_a_decimal_x11_window_id() {
        assert_eq!(parse_xid("3735928559#99"), Some(0xDEAD_BEEF));
        assert_eq!(parse_xid("0#99"), None);
        assert_eq!(parse_xid("nope#99"), None);
    }
}
