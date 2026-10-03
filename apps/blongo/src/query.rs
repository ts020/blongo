//! Workspace queries from any view: a query goes to a backend with a fresh
//! id and its reply (a `CoreEvent::Reply`, delivered to the shell) is
//! handed to the entity that asked, if it still exists.

use std::collections::HashMap;
use std::sync::Arc;

use blongo_client::Backend;
use blongo_protocol::workspace::{Query, QueryId, QueryReply};
use gpui::{App, Context, Global, WeakEntity};

type Callback = Box<dyn FnOnce(Result<QueryReply, String>, &mut App)>;

#[derive(Default)]
pub struct Queries {
    next: QueryId,
    pending: HashMap<QueryId, Callback>,
}

impl Global for Queries {}

/// Ask `backend`; `f` runs on `entity` with the reply.
pub fn ask<T: 'static>(
    backend: &Arc<dyn Backend>,
    query: Query,
    entity: WeakEntity<T>,
    cx: &mut App,
    f: impl FnOnce(&mut T, Result<QueryReply, String>, &mut Context<T>) + 'static,
) {
    let queries = cx.default_global::<Queries>();
    queries.next += 1;
    let id = queries.next;
    queries.pending.insert(
        id,
        Box::new(move |result, cx| {
            entity.update(cx, |this, cx| f(this, result, cx)).ok();
        }),
    );
    backend.query(id, query);
}

/// Deliver a reply. Deferred, so the callback may update any entity,
/// including the one that is delivering it.
pub fn deliver(id: QueryId, result: Result<QueryReply, String>, cx: &mut App) {
    let callback = cx.default_global::<Queries>().pending.remove(&id);
    if let Some(callback) = callback {
        cx.defer(move |cx| callback(result, cx));
    }
}
