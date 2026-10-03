//! Flat, opaque dark palette. No blur, no gradients, no animation.

use gpui::{Hsla, Rgba, rgb, rgba};

pub fn bg() -> Rgba {
    rgb(0x161618)
}
pub fn sidebar() -> Rgba {
    rgb(0x111113)
}
pub fn surface() -> Rgba {
    rgb(0x1f1f23)
}
pub fn surface_hover() -> Rgba {
    rgb(0x26262b)
}
pub fn border() -> Rgba {
    rgb(0x2a2a2f)
}
pub fn code_bg() -> Rgba {
    rgb(0x0f0f11)
}
pub fn text() -> Rgba {
    rgb(0xe8e8ea)
}
pub fn text_muted() -> Rgba {
    rgb(0x9a9aa1)
}
pub fn text_faint() -> Hsla {
    rgb(0x606067).into()
}
pub fn accent() -> Hsla {
    rgb(0x6b8afd).into()
}
pub fn accent_bg() -> Rgba {
    rgb(0x2d3d7a)
}
pub fn selection() -> Hsla {
    rgba(0x6b8afd55).into()
}
pub fn warning() -> Rgba {
    rgb(0xe0a43a)
}
pub fn warning_bg() -> Rgba {
    rgb(0x2a2214)
}
pub fn danger() -> Rgba {
    rgb(0xe5534b)
}
pub fn danger_bg() -> Rgba {
    rgb(0x3a1d1b)
}
pub fn success() -> Rgba {
    rgb(0x4cb782)
}

#[cfg(target_os = "macos")]
pub const MONO: &str = "Menlo";
#[cfg(target_os = "windows")]
pub const MONO: &str = "Consolas";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const MONO: &str = "DejaVu Sans Mono";
