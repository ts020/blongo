//! The boundary the UI talks to: commands and requests go in, ordered
//! [`CoreEvent`]s come out on a channel. [`LocalBackend`] is the in-process
//! core (typed values over a channel, nothing serialized);
//! [`crate::remote::RemoteBackend`] is a `blongo serve` over the wire. The UI
//! cannot tell them apart except through [`Backend::is_remote`].

use std::path::PathBuf;

use blongo_protocol::client::CoreEvent;
use blongo_protocol::{CommandEnvelope, ProviderKind, ThreadId};
use tokio::sync::mpsc::UnboundedReceiver;

pub type Events = UnboundedReceiver<CoreEvent>;

pub trait Backend: Send + Sync + 'static {
    fn dispatch(&self, command: CommandEnvelope);
    /// Ask for a thread's snapshot ([`CoreEvent::Thread`]); a remote
    /// backend also streams that thread's item events from then on (and
    /// stops streaming the previously opened one).
    fn open_thread(&self, thread_id: ThreadId);
    fn login(&self, provider: ProviderKind);
    fn install_antigravity(&self);
    /// Import t3code's database. Remote: a path on the server (`None`: its
    /// default location).
    fn import_t3(&self, source: Option<PathBuf>);
    fn is_remote(&self) -> bool;

    /// Server-side terminals (remote backends with the terminal
    /// capability). Output arrives as [`CoreEvent::Terminal`].
    fn terminal_open(&self, _id: u32, _thread_id: ThreadId, _columns: u16, _lines: u16) {}
    fn terminal_input(&self, _id: u32, _data: Vec<u8>) {}
    fn terminal_resize(&self, _id: u32, _columns: u16, _lines: u16) {}
    fn terminal_close(&self, _id: u32) {}
}

#[cfg(feature = "local")]
pub use local::LocalBackend;

#[cfg(feature = "local")]
mod local {
    use super::*;
    use blongo_core::CoreClient;

    /// The in-process core. Every call is one channel send of a typed
    /// value; events are the core's own channel, untouched.
    #[derive(Clone)]
    pub struct LocalBackend(pub CoreClient);

    impl Backend for LocalBackend {
        fn dispatch(&self, command: CommandEnvelope) {
            self.0.dispatch(command);
        }

        fn open_thread(&self, thread_id: ThreadId) {
            self.0.open_thread(thread_id);
        }

        fn login(&self, provider: ProviderKind) {
            self.0.login(provider);
        }

        fn install_antigravity(&self) {
            self.0.install_antigravity();
        }

        fn import_t3(&self, source: Option<PathBuf>) {
            if let Some(source) = source.or_else(blongo_core::t3_import::default_source) {
                self.0.import_t3(source);
            }
        }

        fn is_remote(&self) -> bool {
            false
        }
    }
}
