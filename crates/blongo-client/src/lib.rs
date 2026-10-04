//! Client side of Blongo: the [`Backend`] boundary the UI talks to, the
//! in-process [`LocalBackend`] (feature `local`) and the [`RemoteBackend`]
//! that reaches a `blongo serve` over Blongo's wire protocol, plus the
//! pieces both ends share (transports, secrets, the reconnect policy).

pub mod backend;
pub mod backoff;
pub mod environments;
pub mod forge;
pub mod handshake;
pub mod http;
pub mod net;
pub mod pairing;
pub mod remote;
pub mod secret;
pub mod target;
pub mod transport;
pub mod update;

#[cfg(feature = "local")]
pub use backend::LocalBackend;
pub use backend::{Backend, Events};
pub use remote::{RemoteBackend, RemoteOptions};
