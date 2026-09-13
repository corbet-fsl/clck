//! # clck
//!
//! A content-agnostic Wayland session locker that keeps designated **kiosk** outputs live while
//! every other output is a PAM lock screen. Full `ext-session-lock` security, split by output
//! role. CPU/`wl_shm` rendered — no GL/Mesa — so it drops into any wlroots compositor.
//!
//! clck is a **display SERVER**, not a library API for kiosk content: it listens on a Unix
//! socket (`$XDG_RUNTIME_DIR/nixlock.sock` by default, or `Config::socket_path`) and blits
//! whatever premultiplied-RGBA frames a client streams in onto the kiosk output — see the module
//! docs on `socket` (crate-internal) or `BEHAVIORS.md`'s `DISPLAY-*` entries for the exact wire
//! format. Until a frame arrives, or once the client disconnects, the kiosk output shows clck's
//! own built-in clock instead. That socket is **display-only**: nothing it carries can ever reach
//! password verification or the unlock call (`DISPLAY-2`).
//!
//! ```no_run
//! use clck::{run, Config};
//! run(Config {
//!     kiosk_outputs: vec!["DP-3".into()],
//!     pam_service: "clck".into(),
//!     username: None,
//!     socket_path: None, // default: $XDG_RUNTIME_DIR/nixlock.sock
//!     debug: false,
//! }).unwrap();
//! ```
//!
//! [`KioskContent`] is still here, but only as the INTERNAL mechanism behind the built-in clock
//! and the `Session` (password) lock screen — not a public per-output content API any more. A
//! consumer that wants a different `Session` look overrides it via [`builder`]; the `Kiosk` role's
//! fallback content is always the shipped [`ClockLockScreen`].

mod auth;
mod content;
mod daemon;
mod diagnostics;
mod locker;
mod lockscreen;
mod socket;
mod theme;

pub use content::{AuthState, AuthView, Frame, KioskContent, OutputRole};
pub use locker::{builder, run, run_daemonized, Builder, Config, LockError};
pub use lockscreen::ClockLockScreen;

/// Verbose PAM auth check used by `clck --check-auth` (verify your PAM service works).
pub use auth::diagnose as check_auth;
