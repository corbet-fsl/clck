//! The default `Session` content clck ships: a centred clock + date + a password field driven
//! by the framework's auth view. Generic (no branding); override via the builder for a custom one.

use crate::content::{AuthState, Frame, KioskContent};
use crate::theme::{self, Fonts};
use std::time::Duration;
use tiny_skia::Pixmap;

pub struct ClockLockScreen {
    fonts: Fonts,
}

impl ClockLockScreen {
    pub fn new() -> Self {
        ClockLockScreen {
            fonts: Fonts::embedded(),
        }
    }
}

impl Default for ClockLockScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl KioskContent for ClockLockScreen {
    fn paint(&mut self, frame: &Frame) -> Vec<u8> {
        let (w, h) = (frame.width, frame.height);
        let mut pm = Pixmap::new(w, h).unwrap();
        let (wf, hf) = (w as f32, h as f32);
        theme::fill_rect(&mut pm, 0.0, 0.0, wf, hf, theme::BG);
        let cx = wf / 2.0;

        let now = chrono::Local::now();
        let clock = now.format("%H:%M:%S").to_string();
        let date = now.format("%a %d %b %Y").to_string();

        let clock_size = (hf * 0.15).min(170.0);
        theme::text_center(
            &mut pm,
            &self.fonts.mono,
            &clock,
            cx,
            hf * 0.44,
            clock_size,
            theme::TEXT,
        );
        theme::text_center(
            &mut pm,
            &self.fonts.sans,
            &date,
            cx,
            hf * 0.52,
            26.0,
            theme::MUTED,
        );

        // password field: one dot per typed character (never the glyphs).
        let field_y = hf * 0.68;
        let dots = match frame.auth.state {
            AuthState::Idle(n) => n.min(40),
            _ => 0,
        };
        let gap = 22.0;
        if dots > 0 {
            let start = cx - (dots as f32 - 1.0) * gap / 2.0;
            for i in 0..dots {
                theme::circle(&mut pm, start + i as f32 * gap, field_y, 6.0, theme::TEXT);
            }
        } else {
            theme::fill_rect(&mut pm, cx - 90.0, field_y - 1.0, 180.0, 2.0, theme::FAINT);
        }

        match frame.auth.state {
            AuthState::Verifying => theme::text_center(
                &mut pm,
                &self.fonts.sans,
                "checking…",
                cx,
                hf * 0.76,
                18.0,
                theme::MUTED,
            ),
            AuthState::Failed => theme::text_center(
                &mut pm,
                &self.fonts.sans,
                "incorrect — try again",
                cx,
                hf * 0.76,
                18.0,
                theme::RED,
            ),
            AuthState::Idle(_) => {}
        }
        if frame.auth.caps_lock {
            theme::text_center(
                &mut pm,
                &self.fonts.sans,
                "caps lock on",
                cx,
                hf * 0.82,
                15.0,
                theme::AMBER,
            );
        }
        theme::text_center(
            &mut pm,
            &self.fonts.sans,
            "locked",
            cx,
            hf * 0.92,
            15.0,
            theme::FAINT,
        );

        pm.data().to_vec()
    }

    fn tick_interval(&self) -> Duration {
        Duration::from_secs(1)
    }
}
