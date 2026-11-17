//! Kitesurf: a place outside the heap where an embedder keeps bytes that compiled code
//! rarely reads (source text for `toString`, bytecode that has not run), fetched on demand.

use std::{cell::RefCell, rc::Rc};

/// Kitesurf: an embedder's append-only store for cold code bytes. A refused `put` keeps the
/// bytes resident, and a failed `read` falls back the way its reader chooses.
pub trait CodeStore {
    /// Kitesurf: source texts smaller than this stay resident, since their write and later
    /// delete cost more than the memory they would free. Bytecode has a floor of its own.
    fn min_bytes(&self) -> usize;

    /// Kitesurf: appends `bytes` and returns the key to read them back with, or None to keep
    /// them resident. Small values share storage, so a put may only reach it at `flush`.
    fn put(&self, bytes: &[u8]) -> Option<u32>;

    /// Kitesurf: replaces `out` with `len` bytes of the value `key` from `offset` on. Returns
    /// false when they cannot be read; `out` is then unspecified.
    fn read(&self, key: u32, offset: usize, len: usize, out: &mut Vec<u8>) -> bool;

    /// Kitesurf: the `len` bytes stored under `key` lost their owner. Called at most once
    /// per key, and possibly while the thread tears its state down.
    fn release(&self, key: u32, len: usize);

    /// Kitesurf: persists what earlier puts left pending. Called at the end of every swap
    /// round, so the store is never asked to hold an unbounded amount in waiting.
    fn flush(&self);
}

thread_local! {
    static STORE: RefCell<Option<Rc<dyn CodeStore>>> = const { RefCell::new(None) };
}

/// Kitesurf: installs (or, with None, removes) this thread's code store. Values already
/// stored keep the store they went to; code made from now on may be swapped to this one.
pub fn ks_set_code_store(store: Option<Rc<dyn CodeStore>>) {
    let _ = STORE.try_with(|s| {
        if let Ok(mut slot) = s.try_borrow_mut() {
            *slot = store;
        }
    });
}

pub(crate) fn current() -> Option<Rc<dyn CodeStore>> {
    STORE
        .try_with(|s| s.try_borrow().ok().and_then(|s| s.clone()))
        .ok()
        .flatten()
}
