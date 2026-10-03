//! The network runtime for remote environments: one small current-thread
//! tokio runtime on its own thread, started the first time a remote
//! environment is used (a window with only the local core never starts it).

use std::sync::OnceLock;

static HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();

pub fn handle() -> tokio::runtime::Handle {
    HANDLE
        .get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("blongo-net".into())
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("network runtime");
                    let _ = tx.send(rt.handle().clone());
                    rt.block_on(std::future::pending::<()>());
                })
                .expect("spawn the network thread");
            rx.recv().expect("network runtime handle")
        })
        .clone()
}
