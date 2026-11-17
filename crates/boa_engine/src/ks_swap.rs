//! Kitesurf: swap rounds, which hand cold code to the embedder's code store (ks_code_store)
//! so it leaves memory that, in wasm, never shrinks.

use std::cell::{Cell, RefCell};

use boa_gc::{Gc, WeakGc};
use rustc_hash::FxHashSet;

use crate::vm::{CodeBlock, Constant};

// Kitesurf: nested functions this deep are left alone; the walk recurses on the shadow stack
// because a forced round must not allocate a worklist.
const MAX_DEPTH: usize = 512;

thread_local! {
    // Kitesurf: compile roots since the last idle round, held weakly so a script the GC has
    // collected costs no store write; whether a root's own block is a function to keep.
    static ROOTS: RefCell<Vec<(WeakGc<CodeBlock>, bool)>> = const { RefCell::new(Vec::new()) };
    // Kitesurf: blocks whose executable bytes a forced round may drop, by address: a block
    // never moves out of its GcBox, and its drop takes it out of here first.
    static FLUSHABLE: RefCell<Option<FxHashSet<usize>>> = const { RefCell::new(None) };
    // Kitesurf: set once a block left FLUSHABLE could not be removed (the set was borrowed),
    // after which an address in it may dangle, so no round flushes again.
    static POISONED: Cell<bool> = const { Cell::new(false) };
    static IN_ROUND: Cell<bool> = const { Cell::new(false) };
}

/// Kitesurf: a compile's top-level block, for the next idle round to walk; `is_function`
/// when it is a function (dynamic Function), not run-once top-level code. Kept only with a store.
pub(crate) fn remember_root(root: &Gc<CodeBlock>, is_function: bool) {
    if crate::ks_code_store::current().is_none() {
        return;
    }
    let weak = WeakGc::new(root);
    let _ = ROOTS.try_with(|r| {
        if let Ok(mut r) = r.try_borrow_mut() {
            r.push((weak, is_function));
        }
    });
}

pub(crate) fn remember_flushable(addr: usize) {
    let _ = FLUSHABLE.try_with(|f| {
        if let Ok(mut f) = f.try_borrow_mut() {
            if f.get_or_insert_with(FxHashSet::default).insert(addr) {
                crate::ks_code_stats::update(|s| s.flushable_blocks += 1);
            }
        }
    });
}

pub(crate) fn forget_flushable(addr: usize) {
    let removed = FLUSHABLE.try_with(|f| match f.try_borrow_mut() {
        Ok(mut f) => Some(f.as_mut().is_some_and(|f| f.remove(&addr))),
        Err(_) => None,
    });
    match removed {
        Ok(Some(true)) => crate::ks_code_stats::update(|s| s.flushable_blocks -= 1),
        Ok(Some(false)) => {}
        _ => {
            let _ = POISONED.try_with(|p| p.set(true));
        }
    }
}

// Kitesurf: marks a round as running for its lifetime, unwinding included: a forced round
// that lands inside any round would see that round's half-made state.
struct RoundGuard;

impl RoundGuard {
    fn enter() -> Option<Self> {
        IN_ROUND
            .try_with(|r| (!r.replace(true)).then_some(Self))
            .ok()
            .flatten()
    }
}

impl Drop for RoundGuard {
    fn drop(&mut self) {
        let _ = IN_ROUND.try_with(|r| r.set(false));
    }
}

// Kitesurf: swaps every packed block of the tree and stores a copy of each function that
// already ran, which then counts as flushable; run-once top-level code is not worth one.
fn walk_packed(block: &CodeBlock, store: &dyn crate::CodeStore, keep_expanded: bool, depth: usize) -> usize {
    if depth > MAX_DEPTH {
        return 0;
    }
    let mut freed = block.bytecode.swap_packed(store);
    if keep_expanded {
        block.bytecode.store_expanded(store);
    }
    let Some(body) = block.resident_body() else {
        return freed;
    };
    for constant in &body.constants {
        if let Constant::Function(inner) = constant {
            freed += walk_packed(inner, store, true, depth + 1);
        }
    }
    freed
}

// Kitesurf: as `walk_packed`, but a nested block that never ran leaves its whole body to the
// store (crate::ks_body); `outside` gathers the scopes of the blocks walked through, which ran.
fn walk(
    block: &CodeBlock,
    store: &dyn crate::CodeStore,
    keep_expanded: bool,
    depth: usize,
    outside: &mut FxHashSet<usize>,
) -> usize {
    if depth > MAX_DEPTH {
        return 0;
    }
    let mut freed = block.bytecode.swap_packed(store);
    if keep_expanded {
        block.bytecode.store_expanded(store);
    }
    let Some(body) = block.resident_body() else {
        return freed;
    };
    for constant in &body.constants {
        if let Constant::Scope(scope) = constant {
            let mut cur = Some(scope.clone());
            while let Some(s) = cur {
                if !outside.insert(s.ks_ptr()) {
                    break;
                }
                cur = s.outer().cloned();
            }
        }
    }
    for constant in &body.constants {
        let Constant::Function(inner) = constant else { continue };
        if inner.ks_ran() {
            freed += walk(inner, store, true, depth + 1, outside);
        } else if inner.resident_body().is_some() {
            freed += inner.bytecode.swap_packed(store);
            let moved = crate::ks_body::swap_out(inner, store, outside);
            freed += if moved > 0 { moved } else { walk_packed(inner, store, true, depth + 1) };
        }
    }
    freed
}

/// Kitesurf: swaps what outlived its eval since the last call and flushes the store; call it
/// at quiescent points. Returns how many values went to the store.
pub fn ks_swap_at_idle() -> usize {
    let Some(store) = crate::ks_code_store::current() else {
        return 0;
    };
    let Some(_round) = RoundGuard::enter() else {
        return 0;
    };
    let mut swapped = crate::spanned_source_text::swap_source_texts();
    let roots = ROOTS
        .try_with(|r| r.try_borrow_mut().map(|mut r| std::mem::take(&mut *r)).unwrap_or_default())
        .unwrap_or_default();
    for (weak, is_function) in &roots {
        if let Some(root) = weak.upgrade() {
            let mut outside = FxHashSet::default();
            swapped += usize::from(walk(&root, &*store, *is_function, 0, &mut outside) > 0);
        }
    }
    if swapped > 0 || !roots.is_empty() {
        store.flush();
    }
    drop(roots);
    swapped
}

/// Kitesurf: the embedder's answer to memory pressure, safe at an arbitrary allocation:
/// it neither runs nor touches the GC. Returns the bytes it moved out of memory.
pub fn ks_swap_under_pressure() -> usize {
    let Some(store) = crate::ks_code_store::current() else {
        return 0;
    };
    let Some(_round) = RoundGuard::enter() else {
        return 0;
    };
    crate::ks_code_stats::update(|s| s.forced_rounds += 1);
    crate::spanned_source_text::swap_source_texts();
    let mut freed = 0;
    // The roots wait for the idle round: upgrading one here would touch the GC, which this
    // allocation may be in the middle of.
    if !POISONED.try_with(Cell::get).unwrap_or(true) {
        let _ = FLUSHABLE.try_with(|f| {
            let Ok(mut f) = f.try_borrow_mut() else {
                return;
            };
            let Some(set) = f.as_mut() else {
                return;
            };
            let before = set.len();
            set.retain(|&addr| {
                // SAFETY: an address is in the set only while its block is alive (its drop
                // removes it, or poisons the set), and blocks never move out of their GcBox.
                let bytecode = unsafe { &*(addr as *const crate::vm::opcode::Bytecode) };
                let n = bytecode.flush_executable();
                freed += n;
                n == 0
            });
            let gone = (before - set.len()) as i64;
            crate::ks_code_stats::update(|s| s.flushable_blocks -= gone);
        });
    }
    store.flush();
    freed
}
