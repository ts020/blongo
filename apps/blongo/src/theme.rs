//! Flat, opaque palettes (dark by default, light from the settings). No
//! blur, no gradients, no animation.

use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{Hsla, Rgba, rgb, rgba};

static LIGHT: AtomicBool = AtomicBool::new(false);

/// Switch palettes; the caller refreshes the windows.
pub fn set_light(light: bool) {
    LIGHT.store(light, Ordering::Relaxed);
}

pub fn is_light() -> bool {
    LIGHT.load(Ordering::Relaxed)
}

fn pick(dark: u32, light: u32) -> u32 {
    if is_light() { light } else { dark }
}

pub fn bg() -> Rgba {
    rgb(pick(0x161618, 0xfafafa))
}
pub fn sidebar() -> Rgba {
    rgb(pick(0x111113, 0xf0f0f2))
}
pub fn surface() -> Rgba {
    rgb(pick(0x1f1f23, 0xffffff))
}
pub fn surface_hover() -> Rgba {
    rgb(pick(0x26262b, 0xe9e9ee))
}
pub fn border() -> Rgba {
    rgb(pick(0x2a2a2f, 0xd9d9de))
}
pub fn code_bg() -> Rgba {
    rgb(pick(0x0f0f11, 0xf3f3f5))
}
pub fn text() -> Rgba {
    rgb(pick(0xe8e8ea, 0x1c1c1f))
}
pub fn text_muted() -> Rgba {
    rgb(pick(0x9a9aa1, 0x5c5c66))
}
pub fn text_faint() -> Hsla {
    rgb(pick(0x606067, 0x8e8e96)).into()
}
pub fn accent() -> Hsla {
    rgb(pick(0x6b8afd, 0x3a5ccc)).into()
}
pub fn accent_bg() -> Rgba {
    rgb(pick(0x2d3d7a, 0xd6e0ff))
}
pub fn selection() -> Hsla {
    rgba(if is_light() { 0x3a5ccc40 } else { 0x6b8afd55 }).into()
}
pub fn warning() -> Rgba {
    rgb(pick(0xe0a43a, 0x9a6400))
}
pub fn warning_bg() -> Rgba {
    rgb(pick(0x2a2214, 0xfff3d6))
}
pub fn danger() -> Rgba {
    rgb(pick(0xe5534b, 0xc4302b))
}
pub fn danger_bg() -> Rgba {
    rgb(pick(0x3a1d1b, 0xfde4e2))
}
pub fn success() -> Rgba {
    rgb(pick(0x4cb782, 0x1f8a55))
}
pub fn link() -> Hsla {
    rgb(pick(0x7fb3ff, 0x2457c5)).into()
}

pub fn diff_added_bg() -> Rgba {
    rgb(pick(0x15291e, 0xe3f6ea))
}
pub fn diff_removed_bg() -> Rgba {
    rgb(pick(0x2e1718, 0xfde8e7))
}
pub fn comment_bg() -> Rgba {
    rgb(pick(0x232a40, 0xeaf0ff))
}

/// Code colors for the classes of `highlight::NAMES`.
pub fn syntax(class: u8) -> Hsla {
    let color = match crate::highlight::NAMES.get(class as usize).copied() {
        Some("comment") => 0x6a737d,
        Some("keyword") | Some("label") => 0xc792ea,
        Some("string") | Some("escape") => 0xa5d6a7,
        Some("number") | Some("constant") | Some("boolean") => 0xf2a65a,
        Some("function") | Some("constructor") => 0x82aaff,
        Some("type") | Some("module") => 0xffcb6b,
        Some("property") | Some("tag") | Some("attribute") => 0x89ddff,
        Some("operator") => 0xb0b0b8,
        Some("variable.builtin") => 0xf07178,
        _ => return text().into(),
    };
    if is_light() {
        // Darker shades of the same hues for a light background.
        let c = rgb(color);
        return Rgba {
            r: c.r * 0.55,
            g: c.g * 0.55,
            b: c.b * 0.55,
            a: 1.,
        }
        .into();
    }
    rgb(color).into()
}

#[cfg(target_os = "macos")]
pub const MONO: &str = "Menlo";
#[cfg(target_os = "windows")]
pub const MONO: &str = "Consolas";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const MONO: &str = "DejaVu Sans Mono";
