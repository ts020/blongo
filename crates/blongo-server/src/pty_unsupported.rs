//! Server-side terminals on platforms without the Unix PTY plumbing
//! (Windows): opening one fails with a clear message; everything else is
//! a no-op so connections behave the same otherwise.

use std::path::Path;
use std::sync::Arc;

use crate::outbox::Outbox;

#[derive(Default)]
pub struct Terminals {
    _none: (),
}

impl Terminals {
    pub fn open(
        &mut self,
        _id: u32,
        _cwd: &Path,
        _columns: u16,
        _lines: u16,
        _outbox: Arc<Outbox>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("server-side terminals are not supported on this platform yet")
    }

    pub fn input(&mut self, _id: u32, _data: Vec<u8>) -> bool {
        true
    }

    pub fn resize(&mut self, _id: u32, _columns: u16, _lines: u16) {}

    pub fn close(&mut self, _id: u32) {}

    pub fn close_all(&mut self) {}
}
