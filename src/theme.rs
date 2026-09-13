//! clck's own theme + CPU drawing helpers, used by the shipped [`crate::ClockLockScreen`].
//! Consumers' kiosk content brings its own renderer; these helpers are not imposed on them.
//! Dark ground, violet accent — never cream.

use fontdue::Font;
use tiny_skia::{Paint, PathBuilder, Pixmap, Rect, Transform};

pub type Rgb = (u8, u8, u8);

pub const BG: Rgb = (0x07, 0x07, 0x0b);
pub const TEXT: Rgb = (0xea, 0xea, 0xf2);
pub const MUTED: Rgb = (0x8a, 0x8a, 0x9c);
pub const FAINT: Rgb = (0x55, 0x55, 0x66);
#[allow(dead_code)] // part of the palette; not used by the minimal default lock screen
pub const ACCENT: Rgb = (0xa7, 0x8b, 0xfa);
#[allow(dead_code)]
pub const GREEN: Rgb = (0x3f, 0xd0, 0x68);
pub const RED: Rgb = (0xf4, 0x53, 0x53);
pub const AMBER: Rgb = (0xf5, 0x9e, 0x0b);

/// Embedded fonts — a locker must not depend on fontconfig / session services at lock time.
pub struct Fonts {
    pub sans: Font,
    pub mono: Font,
}

impl Fonts {
    pub fn embedded() -> Self {
        let s = fontdue::FontSettings::default();
        Fonts {
            sans: Font::from_bytes(&include_bytes!("../assets/Inter.ttf")[..], s).expect("Inter"),
            mono: Font::from_bytes(&include_bytes!("../assets/Mono.ttf")[..], s).expect("Mono"),
        }
    }
}

pub fn fill_rect(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, c: Rgb) {
    if let Some(r) = Rect::from_xywh(x, y, w, h) {
        let mut p = Paint::default();
        p.set_color_rgba8(c.0, c.1, c.2, 255);
        pm.as_mut().fill_rect(r, &p, Transform::identity(), None);
    }
}

pub fn circle(pm: &mut Pixmap, cx: f32, cy: f32, r: f32, c: Rgb) {
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, r);
    if let Some(path) = pb.finish() {
        let mut p = Paint::default();
        p.set_color_rgba8(c.0, c.1, c.2, 255);
        p.anti_alias = true;
        pm.as_mut().fill_path(
            &path,
            &p,
            tiny_skia::FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

#[inline]
fn blend(data: &mut [u8], i: usize, c: Rgb, cov: u8) {
    if cov == 0 {
        return;
    }
    let sa = cov as f32 / 255.0;
    let ia = 1.0 - sa;
    data[i] = (c.0 as f32 * sa + data[i] as f32 * ia).round() as u8;
    data[i + 1] = (c.1 as f32 * sa + data[i + 1] as f32 * ia).round() as u8;
    data[i + 2] = (c.2 as f32 * sa + data[i + 2] as f32 * ia).round() as u8;
    data[i + 3] = (sa * 255.0 + data[i + 3] as f32 * ia).round() as u8;
}

pub fn text_width(font: &Font, s: &str, size: f32) -> f32 {
    s.chars()
        .map(|ch| font.metrics(ch, size).advance_width)
        .sum()
}

/// Draw a left-anchored string at baseline `by`.
pub fn text(pm: &mut Pixmap, font: &Font, s: &str, x: f32, by: f32, size: f32, c: Rgb) {
    let w = pm.width() as i32;
    let h = pm.height() as i32;
    let data = pm.data_mut();
    let mut pen = x;
    for ch in s.chars() {
        let (m, bitmap) = font.rasterize(ch, size);
        let gx = (pen + m.xmin as f32).round() as i32;
        let gy = (by - (m.height as f32 + m.ymin as f32)).round() as i32;
        for row in 0..m.height {
            for col in 0..m.width {
                let cov = bitmap[row * m.width + col];
                let px = gx + col as i32;
                let py = gy + row as i32;
                if px < 0 || py < 0 || px >= w || py >= h {
                    continue;
                }
                blend(data, ((py * w + px) * 4) as usize, c, cov);
            }
        }
        pen += m.advance_width;
    }
}

pub fn text_center(pm: &mut Pixmap, font: &Font, s: &str, cx: f32, by: f32, size: f32, c: Rgb) {
    let tw = text_width(font, s, size);
    text(pm, font, s, cx - tw / 2.0, by, size, c);
}
