//! The content interface clck paints INTERNALLY: the `Session` (password) lock screen, always,
//! and the `Kiosk` output's fallback clock whenever the socket has no live frame for it (see
//! `crate::socket`). It is no longer a per-output plug-in API for kiosk content — that content now
//! arrives over the Unix socket as raw premultiplied RGBA, content-blind to clck. A library
//! consumer's only remaining lever here is overriding the `Session` look via [`crate::builder`];
//! the shipped [`crate::ClockLockScreen`] is the default for both `Session` and the `Kiosk`
//! fallback.

use std::time::Duration;

/// Which lock surface an output carries. `Kiosk` = live pluggable content (a dashboard) with no
/// password affordance; `Session` = the PAM lock screen. Anything not named a kiosk output is
/// `Session` — the SAFE default, so a newly-plugged or unknown monitor shows the lock screen,
/// never the dashboard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputRole {
    Kiosk,
    Session,
}

/// Decide an output's role from its compositor name. Exact, case-sensitive, whole-string equality
/// against the configured kiosk list -- anything else is `Session`.
///
/// This is a one-line rule and it is the single place the SAFE default lives, which is why it is a
/// named function rather than an inline `if`: every way of getting it wrong hands a password-free
/// live display to an output that was supposed to be a lock screen. A prefix or `contains` match
/// would make `DP-3` capture `DP-30`; a case-insensitive one would make a typo'd config silently
/// succeed. An output the compositor gave no name (`""`) is `Session` too, unless someone has
/// literally configured `""` as a kiosk output.
pub(crate) fn role_for(kiosk_outputs: &[String], name: &str) -> OutputRole {
    if kiosk_outputs.iter().any(|k| k == name) {
        OutputRole::Kiosk
    } else {
        OutputRole::Session
    }
}

/// What the lock screen may render about the current auth attempt. The password itself is NEVER
/// here — only its shape. `Kiosk` content ignores this.
#[derive(Clone, Copy, Debug)]
pub enum AuthState {
    /// No attempt in progress; `usize` is how many characters are currently typed.
    Idle(usize),
    /// A PAM attempt is running off-thread; input is inert until it resolves.
    Verifying,
    /// The last attempt was rejected.
    Failed,
}

/// The auth view handed to content each frame (meaningful only on `Session` outputs).
#[derive(Clone, Copy, Debug)]
pub struct AuthView {
    pub state: AuthState,
    pub caps_lock: bool,
}

/// Everything content needs to paint one output for one tick. No wayland, no shm, no auth
/// internals leak through — content stays a pure `(state, size) -> pixels` function.
pub struct Frame<'a> {
    pub role: OutputRole,
    pub output_name: &'a str,
    pub width: u32,
    pub height: u32,
    pub auth: AuthView,
}

/// Pluggable per-output content. It runs inside the locker (`Send + 'static`).
pub trait KioskContent: Send + 'static {
    /// Paint one output. Return PREMULTIPLIED RGBA8, exactly `width * height * 4` bytes. The
    /// framework owns the BGRA swizzle and the `wl_shm` commit; a wrong-sized buffer is rejected
    /// (never blitted), and the output falls back to the lock screen.
    fn paint(&mut self, frame: &Frame) -> Vec<u8>;

    /// How often to repaint absent input (clock tick / poll cadence).
    fn tick_interval(&self) -> Duration {
        Duration::from_secs(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kiosks(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    // The default is the whole point: an output nobody configured must be a LOCK SCREEN. Getting
    // this backwards is not a cosmetic bug -- it puts a password-free live display on a monitor
    // that was supposed to be locked.
    #[test]
    fn an_unconfigured_output_is_a_session_lock_screen() {
        assert_eq!(role_for(&kiosks(&["DP-3"]), "DP-1"), OutputRole::Session);
        assert_eq!(role_for(&[], "DP-3"), OutputRole::Session);
        assert_eq!(role_for(&kiosks(&["DP-3"]), ""), OutputRole::Session);
    }

    #[test]
    fn an_exactly_named_output_is_the_kiosk() {
        assert_eq!(role_for(&kiosks(&["DP-3"]), "DP-3"), OutputRole::Kiosk);
        assert_eq!(
            role_for(&kiosks(&["HDMI-A-1", "DP-3"]), "DP-3"),
            OutputRole::Kiosk
        );
    }

    // A prefix/`contains` match would make the configured `DP-3` also capture a newly plugged
    // `DP-30`, silently turning a fresh monitor into a password-free display.
    #[test]
    fn a_name_that_merely_starts_with_a_kiosk_name_is_not_the_kiosk() {
        assert_eq!(role_for(&kiosks(&["DP-3"]), "DP-30"), OutputRole::Session);
        assert_eq!(role_for(&kiosks(&["DP-3"]), "DP-3 "), OutputRole::Session);
        assert_eq!(role_for(&kiosks(&["DP-30"]), "DP-3"), OutputRole::Session);
    }

    // Wayland output names are case-sensitive; a case-insensitive match would let a typo'd config
    // succeed and would be indistinguishable from the operator having meant it.
    #[test]
    fn matching_is_case_sensitive() {
        assert_eq!(role_for(&kiosks(&["DP-3"]), "dp-3"), OutputRole::Session);
    }
}
