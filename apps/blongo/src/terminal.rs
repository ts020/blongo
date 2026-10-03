//! Terminal panel: a shell on a PTY (portable-pty, MIT) emulated by
//! alacritty_terminal (Apache-2.0). Zed's terminal crate is GPL and is not
//! used.
//!
//! A reader thread feeds PTY output into the emulator; the UI is told at
//! most once per frame that the screen changed. Scrollback is bounded by
//! [`SCROLLBACK`] lines. Closing the panel (or dropping it) hangs up the
//! shell this panel started — and only that process — and reaps it off the
//! UI thread.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Point;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode, point_to_viewport};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use gpui::{
    AppContext as _, Context, FocusHandle, Focusable, HighlightStyle, Hsla, KeyDownEvent,
    ScrollWheelEvent, SharedString, StyledText, Window, div, prelude::*, px, rgb,
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::theme;

/// Lines of scrollback kept per terminal (bounded memory: at most about
/// `SCROLLBACK × columns × 24` bytes of cells).
pub const SCROLLBACK: usize = 1_000;
pub const FONT_SIZE: f32 = 12.;
pub const LINE_HEIGHT: f32 = 16.;
const FRAME: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSize {
    pub columns: usize,
    pub lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

/// Answers the emulator's requests that need the PTY (device status
/// reports and the like).
#[derive(Clone)]
struct Listener {
    writer: Arc<FairMutex<Box<dyn Write + Send>>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        if let TermEvent::PtyWrite(text) = event {
            let mut w = self.writer.lock();
            let _ = w.write_all(text.as_bytes());
            let _ = w.flush();
        }
    }
}

pub struct TerminalView {
    term: Arc<FairMutex<Term<Listener>>>,
    writer: Arc<FairMutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Option<Box<dyn Child + Send + Sync>>,
    size: GridSize,
    exited: Arc<AtomicBool>,
    pub title: SharedString,
    focus_handle: FocusHandle,
}

impl TerminalView {
    /// Start `shell` (default: `$SHELL`, else `/bin/sh`) in `cwd`.
    pub fn open(
        cwd: &Path,
        shell: Option<String>,
        cx: &mut gpui::App,
    ) -> anyhow::Result<gpui::Entity<Self>> {
        let size = GridSize {
            columns: 100,
            lines: 12,
        };
        let pty = native_pty_system().openpty(pty_size(size))?;
        let shell = shell
            .or_else(|| std::env::var("SHELL").ok())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        let child = pty.slave.spawn_command(cmd)?;
        // The child holds its own copy of the slave side.
        drop(pty.slave);
        let reader = pty.master.try_clone_reader()?;
        let writer: Arc<FairMutex<Box<dyn Write + Send>>> =
            Arc::new(FairMutex::new(pty.master.take_writer()?));
        let config = Config {
            scrolling_history: SCROLLBACK,
            ..Config::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(
            config,
            &size,
            Listener {
                writer: writer.clone(),
            },
        )));
        let exited = Arc::new(AtomicBool::new(false));
        let master = pty.master;
        Ok(cx.new(|cx| {
            Self::pump(reader, term.clone(), exited.clone(), cx);
            Self {
                term,
                writer,
                master,
                child: Some(child),
                size,
                exited,
                title: SharedString::from(shell),
                focus_handle: cx.focus_handle(),
            }
        }))
    }

    /// Reader thread → emulator; wake the view at most once per frame.
    fn pump(
        mut reader: Box<dyn Read + Send>,
        term: Arc<FairMutex<Term<Listener>>>,
        exited: Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        let (wake_tx, mut wake_rx) = tokio::sync::mpsc::channel::<()>(1);
        std::thread::Builder::new()
            .name("blongo-pty".into())
            .spawn(move || {
                let mut processor: Processor = Processor::new();
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            processor.advance(&mut *term.lock(), &buf[..n]);
                            // Full channel: a wake is already pending.
                            let _ = wake_tx.try_send(());
                        }
                    }
                }
                exited.store(true, Ordering::Relaxed);
                let _ = wake_tx.try_send(());
            })
            .expect("spawn pty reader");
        cx.spawn(async move |this, cx| {
            while wake_rx.recv().await.is_some() {
                cx.background_executor().timer(FRAME).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
            // Reader gone: the shell exited.
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    pub fn exited(&self) -> bool {
        self.exited.load(Ordering::Relaxed)
    }

    pub fn write(&self, bytes: &[u8]) {
        let mut w = self.writer.lock();
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }

    /// Fit the grid to the panel.
    pub fn resize(&mut self, size: GridSize) {
        let size = GridSize {
            columns: size.columns.clamp(10, 500),
            lines: size.lines.clamp(2, 200),
        };
        if size == self.size {
            return;
        }
        self.size = size;
        let _ = self.master.resize(pty_size(size));
        self.term.lock().resize(size);
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let app_cursor = self.term.lock().mode().contains(TermMode::APP_CURSOR);
        if let Some(bytes) = key_bytes(&event.keystroke, app_cursor) {
            // Typing jumps back to the bottom, like other terminals.
            self.term.lock().scroll_display(Scroll::Bottom);
            self.write(&bytes);
            cx.stop_propagation();
        }
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let dy = event.delta.pixel_delta(px(LINE_HEIGHT)).y;
        let lines = (f32::from(dy) / LINE_HEIGHT).round() as i32;
        if lines != 0 {
            self.term.lock().scroll_display(Scroll::Delta(lines));
            cx.notify();
        }
    }
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        // Hang up the shell we started (its own PID only), escalate if it
        // ignores that, and reap it off the UI thread.
        let pid = child.process_id();
        std::thread::spawn(move || {
            if let Some(pid) = pid {
                if matches!(child.try_wait(), Ok(None)) {
                    unsafe { libc::kill(pid as i32, libc::SIGHUP) };
                }
                for _ in 0..20 {
                    if !matches!(child.try_wait(), Ok(None)) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                if matches!(child.try_wait(), Ok(None)) {
                    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                }
            }
            let _ = child.wait();
        });
    }
}

fn pty_size(size: GridSize) -> PtySize {
    PtySize {
        rows: size.lines as u16,
        cols: size.columns as u16,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Bytes a key press sends to the PTY (xterm conventions).
pub fn key_bytes(keystroke: &gpui::Keystroke, app_cursor: bool) -> Option<Vec<u8>> {
    let m = &keystroke.modifiers;
    let arrow = |c: u8| {
        if app_cursor {
            vec![0x1b, b'O', c]
        } else {
            vec![0x1b, b'[', c]
        }
    };
    let bytes = match keystroke.key.as_str() {
        "enter" => vec![b'\r'],
        "backspace" => vec![0x7f],
        "tab" if m.shift => b"\x1b[Z".to_vec(),
        "tab" => vec![b'\t'],
        "escape" => vec![0x1b],
        "up" => arrow(b'A'),
        "down" => arrow(b'B'),
        "right" => arrow(b'C'),
        "left" => arrow(b'D'),
        "home" => arrow(b'H'),
        "end" => arrow(b'F'),
        "delete" => b"\x1b[3~".to_vec(),
        "pageup" => b"\x1b[5~".to_vec(),
        "pagedown" => b"\x1b[6~".to_vec(),
        "space" if m.control => vec![0],
        key if m.control && !m.alt && key.len() == 1 => {
            let c = key.as_bytes()[0].to_ascii_lowercase();
            match c {
                b'a'..=b'z' => vec![c - b'a' + 1],
                b'[' => vec![0x1b],
                b'\\' => vec![0x1c],
                b']' => vec![0x1d],
                _ => return None,
            }
        }
        _ => {
            if m.platform || m.control {
                return None;
            }
            let text = keystroke
                .key_char
                .clone()
                .or_else(|| (keystroke.key == "space").then(|| " ".into()))
                .or_else(|| (keystroke.key.chars().count() == 1).then(|| keystroke.key.clone()))?;
            let mut out = Vec::new();
            if m.alt {
                out.push(0x1b);
            }
            out.extend_from_slice(text.as_bytes());
            out
        }
    };
    Some(bytes)
}

/// xterm's 256-color palette.
pub fn indexed_color(ix: u8) -> u32 {
    const BASE: [u32; 16] = [
        0x1f1f23, 0xe5534b, 0x4cb782, 0xe0a43a, 0x6b8afd, 0xc792ea, 0x56b6c2, 0xd0d0d4, 0x606067,
        0xff6b63, 0x6fdc9c, 0xffcb6b, 0x8aa4ff, 0xddaaff, 0x89ddff, 0xffffff,
    ];
    match ix {
        0..=15 => BASE[ix as usize],
        16..=231 => {
            let i = ix - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v as u32 };
            (level(i / 36) << 16) | (level((i / 6) % 6) << 8) | level(i % 6)
        }
        _ => {
            let v = 8 + 10 * (ix - 232) as u32;
            (v << 16) | (v << 8) | v
        }
    }
}

fn color_value(color: Color, colors: &alacritty_terminal::term::color::Colors, fg: bool) -> u32 {
    let rgb_u32 = |c: Rgb| ((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32;
    match color {
        Color::Spec(c) => rgb_u32(c),
        Color::Indexed(ix) => colors[ix as usize].map_or(indexed_color(ix), rgb_u32),
        Color::Named(name) => {
            if let Some(c) = colors[name] {
                return rgb_u32(c);
            }
            match name {
                NamedColor::Foreground | NamedColor::BrightForeground => 0xe8e8ea,
                NamedColor::Background => 0x0f0f11,
                NamedColor::DimForeground => 0x9a9aa1,
                NamedColor::Cursor => 0xe8e8ea,
                n if (n as usize) < 16 => indexed_color(n as u8),
                n if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize)
                    .contains(&(n as usize)) =>
                {
                    indexed_color((n as usize - NamedColor::DimBlack as usize) as u8)
                }
                _ if fg => 0xe8e8ea,
                _ => 0x0f0f11,
            }
        }
    }
}

/// One screen line as text plus color runs; the cursor cell is inverted.
pub struct RenderedLine {
    pub text: String,
    pub runs: Vec<(std::ops::Range<usize>, HighlightStyle)>,
}

fn render_lines(term: &Term<Listener>, focused: bool) -> Vec<RenderedLine> {
    let content = term.renderable_content();
    let lines = term.screen_lines();
    let mut out: Vec<RenderedLine> = (0..lines)
        .map(|_| RenderedLine {
            text: String::new(),
            runs: Vec::new(),
        })
        .collect();
    let cursor = content.cursor.point;
    let show_cursor =
        content.display_offset == 0 && content.mode.contains(TermMode::SHOW_CURSOR) && focused;
    let default_fg = color_value(Color::Named(NamedColor::Foreground), content.colors, true);
    let default_bg = color_value(Color::Named(NamedColor::Background), content.colors, false);
    let display_offset = content.display_offset;
    let colors = content.colors;
    for indexed in content.display_iter {
        let Some(view) = point_to_viewport(display_offset, indexed.point) else {
            continue;
        };
        let Some(line) = out.get_mut(view.line) else {
            continue;
        };
        let cell: &Cell = indexed.cell;
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        let mut fg = color_value(cell.fg, colors, true);
        let mut bg = color_value(cell.bg, colors, false);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        let at_cursor = show_cursor && indexed.point == Point::new(cursor.line, cursor.column);
        if at_cursor {
            std::mem::swap(&mut fg, &mut bg);
            if fg == bg {
                bg = default_fg;
                fg = default_bg;
            }
        }
        let start = line.text.len();
        let ch = if cell.flags.contains(Flags::HIDDEN) || cell.c == '\0' {
            ' '
        } else {
            cell.c
        };
        line.text.push(ch);
        if let Some(zw) = cell.zerowidth() {
            line.text.extend(zw);
        }
        let end = line.text.len();
        let bold = cell.flags.contains(Flags::BOLD);
        if fg != default_fg || bg != default_bg || bold {
            let style = HighlightStyle {
                color: (fg != default_fg).then(|| Hsla::from(rgb(fg))),
                background_color: (bg != default_bg).then(|| Hsla::from(rgb(bg))),
                font_weight: bold.then_some(gpui::FontWeight::BOLD),
                ..Default::default()
            };
            match line.runs.last_mut() {
                Some((r, s)) if r.end == start && *s == style => r.end = end,
                _ => line.runs.push((start..end, style)),
            }
        }
    }
    for line in &mut out {
        // Keep trailing styled cells (the cursor), drop plain trailing blanks.
        let keep = line.runs.last().map_or(0, |(r, _)| r.end);
        let trimmed = line.text.trim_end().len().max(keep);
        line.text.truncate(trimmed);
    }
    out
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let lines = render_lines(&self.term.lock(), focused);
        let exited = self.exited();
        div()
            .id("terminal")
            .track_focus(&self.focus_handle)
            .key_context("Terminal")
            .on_key_down(cx.listener(Self::on_key))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_click(cx.listener(|this, _, window, cx| {
                window.focus(&this.focus_handle, cx);
            }))
            .size_full()
            .overflow_hidden()
            .px_2()
            .py_1()
            .bg(theme::code_bg())
            .font_family(theme::MONO)
            .text_size(px(FONT_SIZE))
            .line_height(px(LINE_HEIGHT))
            .text_color(theme::text())
            .children(lines.into_iter().map(|line| {
                div().h(px(LINE_HEIGHT)).whitespace_nowrap().child(
                    StyledText::new(SharedString::from(line.text)).with_highlights(line.runs),
                )
            }))
            .when(exited, |d| {
                d.child(
                    div()
                        .text_color(theme::text_faint())
                        .child("[process exited — close and reopen the terminal]"),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Keystroke, Modifiers};

    fn key(key: &str, ch: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.into(),
            key_char: ch.map(Into::into),
        }
    }

    #[test]
    fn keys_map_to_xterm_bytes() {
        let none = Modifiers::default();
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(
            key_bytes(&key("a", Some("a"), none), false),
            Some(b"a".to_vec())
        );
        assert_eq!(key_bytes(&key("c", None, ctrl), false), Some(vec![3]));
        assert_eq!(
            key_bytes(&key("enter", None, none), false),
            Some(vec![b'\r'])
        );
        assert_eq!(
            key_bytes(&key("up", None, none), false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_bytes(&key("up", None, none), true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            key_bytes(&key("space", Some(" "), none), false),
            Some(b" ".to_vec())
        );
        assert_eq!(
            key_bytes(&key("界", Some("界"), none), false),
            Some("界".as_bytes().to_vec())
        );
        let platform = Modifiers {
            platform: true,
            ..Default::default()
        };
        assert_eq!(key_bytes(&key("v", Some("v"), platform), false), None);
    }

    #[test]
    fn palette_covers_cube_and_grays() {
        assert_eq!(indexed_color(16), 0x000000);
        assert_eq!(indexed_color(231), 0xffffff);
        assert_eq!(indexed_color(232), 0x080808);
        assert_eq!(indexed_color(196), 0xff0000);
    }

    #[test]
    fn emulator_scrollback_is_bounded() {
        let sink: Box<dyn Write + Send> = Box::new(std::io::sink());
        let size = GridSize {
            columns: 20,
            lines: 5,
        };
        let mut term = Term::new(
            Config {
                scrolling_history: SCROLLBACK,
                ..Config::default()
            },
            &size,
            Listener {
                writer: Arc::new(FairMutex::new(sink)),
            },
        );
        let mut processor: Processor = Processor::new();
        for i in 0..(SCROLLBACK * 3) {
            processor.advance(&mut term, format!("line {i}\r\n").as_bytes());
        }
        assert_eq!(term.grid().total_lines(), SCROLLBACK + 5);
        processor.advance(&mut term, b"\x1b[31mred\x1b[0m plain");
        let lines = render_lines(&term, true);
        let last = lines.iter().rev().find(|l| !l.text.is_empty()).unwrap();
        assert!(last.text.starts_with("red plain"), "{:?}", last.text);
        // "red" carries a color run, and the cursor cell is styled.
        assert!(last.runs[0].0 == (0..3) && last.runs[0].1.color.is_some());
        assert!(last.runs.len() >= 2);
    }
}
