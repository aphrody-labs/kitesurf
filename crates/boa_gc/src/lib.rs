//! Boa's **`boa_gc`** crate implements a garbage collector.
//!
//! # Crate Overview
//! **`boa_gc`** is a mark-sweep garbage collector that implements a [`Trace`] and [`Finalize`] trait
//! for garbage collected values.
#![doc = include_str!("../ABOUT.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/boa-dev/boa/main/assets/logo_black.svg",
    html_favicon_url = "https://raw.githubusercontent.com/boa-dev/boa/main/assets/logo_black.svg"
)]
#![cfg_attr(not(test), forbid(clippy::unwrap_used))]
#![allow(
    clippy::module_name_repetitions,
    clippy::redundant_pub_crate,
    clippy::let_unit_value
)]

extern crate self as boa_gc;

mod cell;
mod pointers;
mod trace;

pub(crate) mod internals;

use internals::{EphemeronBox, ErasedEphemeronBox, ErasedWeakMapBox, WeakMapBox};
use pointers::{NonTraceable, RawWeakMap};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    mem,
    ptr::NonNull,
};

pub use crate::trace::{Finalize, Trace, Tracer};
pub use boa_macros::{Finalize, Trace};
pub use cell::{GcRef, GcRefCell, GcRefMut};
pub use internals::GcBox;
pub use pointers::{Ephemeron, Gc, GcErased, WeakGc, WeakMap};

type GcErasedPointer = NonNull<GcBox<NonTraceable>>;
type EphemeronPointer = NonNull<dyn ErasedEphemeronBox>;
type ErasedWeakMapBoxPointer = NonNull<dyn ErasedWeakMapBox>;

thread_local!(static GC_DROPPING: Cell<bool> = const { Cell::new(false) });
thread_local!(static BOA_GC: RefCell<BoaGc> = RefCell::new( BoaGc {
    config: GcConfig::default(),
    runtime: GcRuntimeData::default(),
    strongs: Vec::default(),
    weaks: Vec::default(),
    weak_maps: Vec::default(),
}));

#[derive(Debug, Clone, Copy)]
struct GcConfig {
    /// The threshold at which the garbage collector will trigger a collection.
    threshold: usize,
    /// The percentage of used space at which the garbage collector will trigger a collection.
    used_space_percentage: usize,
}

// Setting the defaults to an arbitrary value currently.
//
// TODO: Add a configure later
impl Default for GcConfig {
    fn default() -> Self {
        Self {
            // Start at 1MB, the nursary size for V8 is ~1-8MB and SM can be up to 16MB
            threshold: 1_048_576,
            used_space_percentage: 70,
        }
    }
}

#[derive(Default, Debug, Clone, Copy)]
struct GcRuntimeData {
    collections: usize,
    bytes_allocated: usize,
}

#[derive(Debug)]
struct BoaGc {
    config: GcConfig,
    runtime: GcRuntimeData,
    strongs: Vec<GcErasedPointer>,
    weaks: Vec<EphemeronPointer>,
    weak_maps: Vec<ErasedWeakMapBoxPointer>,
}

impl Drop for BoaGc {
    fn drop(&mut self) {
        Collector::dump(self);
    }
}

// Whether or not the thread is currently in the sweep phase of garbage collection.
// During this phase, attempts to dereference a `Gc<T>` pointer will trigger a panic.
/// `DropGuard` flags whether the Collector is currently running `Collector::sweep()` or `Collector::dump()`
///
/// While the `DropGuard` is active, all `GcBox`s must not be dereferenced or accessed as it could cause Undefined Behavior
#[derive(Debug, Clone)]
struct DropGuard;

impl DropGuard {
    fn new() -> Self {
        GC_DROPPING.with(|dropping| dropping.set(true));
        Self
    }
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        GC_DROPPING.with(|dropping| dropping.set(false));
    }
}

/// Returns `true` if it is safe for a type to run [`Finalize::finalize`].
#[must_use]
#[inline]
pub fn finalizer_safe() -> bool {
    GC_DROPPING.with(|dropping| !dropping.get())
}

/// The Allocator handles allocation of garbage collected values.
///
/// The allocator can trigger a garbage collection.
#[derive(Debug, Clone, Copy)]
struct Allocator;

impl Allocator {
    /// Allocate a new garbage collected value to the Garbage Collector's heap.
    fn alloc_gc<T: Trace>(value: GcBox<T>) -> NonNull<GcBox<T>> {
        let element_size = size_of_val::<GcBox<T>>(&value);
        BOA_GC.with(|st| {
            let mut gc = st.borrow_mut();

            Self::manage_state(&mut gc);
            // Safety: value cannot be a null pointer, since `Box` cannot return null pointers.
            let ptr = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(value))) };
            let erased: NonNull<GcBox<NonTraceable>> = ptr.cast();

            gc.strongs.push(erased);
            gc.runtime.bytes_allocated += element_size;

            ptr
        })
    }

    fn alloc_ephemeron<K: Trace + ?Sized, V: Trace>(
        value: EphemeronBox<K, V>,
    ) -> NonNull<EphemeronBox<K, V>> {
        let element_size = size_of_val::<EphemeronBox<K, V>>(&value);
        BOA_GC.with(|st| {
            let mut gc = st.borrow_mut();

            Self::manage_state(&mut gc);
            // Safety: value cannot be a null pointer, since `Box` cannot return null pointers.
            let ptr = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(value))) };
            let erased: NonNull<dyn ErasedEphemeronBox> = ptr;

            gc.weaks.push(erased);
            gc.runtime.bytes_allocated += element_size;

            ptr
        })
    }

    fn alloc_weak_map<K: Trace + ?Sized, V: Trace>() -> WeakMap<K, V> {
        let weak_map = WeakMap {
            inner: Gc::new(GcRefCell::new(RawWeakMap::new())),
        };
        let weak = WeakGc::new(&weak_map.inner);

        BOA_GC.with(|st| {
            let mut gc = st.borrow_mut();

            let weak_box = WeakMapBox { map: weak };

            // Safety: value cannot be a null pointer, since `Box` cannot return null pointers.
            let ptr = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(weak_box))) };
            let erased: ErasedWeakMapBoxPointer = ptr;

            gc.weak_maps.push(erased);

            weak_map
        })
    }

    fn manage_state(gc: &mut BoaGc) {
        if gc.runtime.bytes_allocated > gc.config.threshold {
            Collector::collect(gc);

            // Post collection check
            // If the allocated bytes are still above the threshold, increase the threshold.
            if gc.runtime.bytes_allocated
                > gc.config.threshold / 100 * gc.config.used_space_percentage
            {
                gc.config.threshold =
                    gc.runtime.bytes_allocated / gc.config.used_space_percentage * 100;
            }
        }
    }
}

struct Unreachables {
    strong: Vec<GcErasedPointer>,
    weak: Vec<NonNull<dyn ErasedEphemeronBox>>,
}

/// This collector currently functions in four main phases
///
/// Mark -> Finalize -> Mark -> Sweep
///
/// 1. Mark nodes as reachable.
/// 2. Finalize the unreachable nodes.
/// 3. Mark again because `Finalize::finalize` can potentially resurrect dead nodes.
/// 4. Sweep and drop all dead nodes.
///
/// A better approach in a more concurrent structure may be to reorder.
///
/// Mark -> Sweep -> Finalize
struct Collector;

impl Collector {
    /// Run a collection on the full heap.
    fn collect(gc: &mut BoaGc) {
        gc.runtime.collections += 1;

        Self::trace_non_roots(gc);

        let mut tracer = Tracer::new();

        let unreachables = Self::mark_heap(&mut tracer, &gc.strongs, &gc.weaks, &gc.weak_maps);

        assert!(tracer.is_empty(), "The queue should be empty");

        // Only finalize if there are any unreachable nodes.
        if !unreachables.strong.is_empty() || !unreachables.weak.is_empty() {
            // Finalize all the unreachable nodes.
            // SAFETY: All passed pointers are valid, since we won't deallocate until `Self::sweep`.
            unsafe { Self::finalize(unreachables) };

            // Reuse the tracer's already allocated capacity.
            let _final_unreachables =
                Self::mark_heap(&mut tracer, &gc.strongs, &gc.weaks, &gc.weak_maps);
        }

        // SAFETY: The head of our linked list is always valid per the invariants of our GC.
        unsafe {
            Self::sweep(
                &mut gc.strongs,
                &mut gc.weaks,
                &mut gc.runtime.bytes_allocated,
            );
        }

        // Weak maps have to be cleared after the sweep, since the process dereferences GcBoxes.
        gc.weak_maps.retain(|w| {
            // SAFETY: The caller must ensure the validity of every node of `heap_start`.
            let node_ref = unsafe { w.as_ref() };

            if node_ref.is_live() {
                node_ref.clear_dead_entries();

                true
            } else {
                // SAFETY:
                // The `Allocator` must always ensure its start node is a valid, non-null pointer that
                // was allocated by `Box::from_raw(Box::new(..))`.
                let _unmarked_node = unsafe { Box::from_raw(w.as_ptr()) };

                false
            }
        });

        gc.strongs.shrink_to(gc.strongs.len() >> 2);
        gc.weaks.shrink_to(gc.weaks.len() >> 2);
        gc.weak_maps.shrink_to(gc.weak_maps.len() >> 2);
    }

    fn trace_non_roots(gc: &BoaGc) {
        // Count all the handles located in GC heap.
        // Then, we can find whether there is a reference from other places, and they are the roots.
        for node in &gc.strongs {
            // SAFETY: node must be valid as this phase cannot drop any node.
            let trace_non_roots_fn = unsafe { node.as_ref() }.trace_non_roots_fn();

            // SAFETY: The function pointer is appropriate for this node type because we extract it from it's VTable.
            unsafe {
                trace_non_roots_fn(*node);
            }
        }

        for eph in &gc.weaks {
            // SAFETY: node must be valid as this phase cannot drop any node.
            let eph_ref = unsafe { eph.as_ref() };
            eph_ref.trace_non_roots();
        }
    }

    /// Walk the heap and mark any nodes deemed reachable
    fn mark_heap(
        tracer: &mut Tracer,
        strongs: &[GcErasedPointer],
        weaks: &[EphemeronPointer],
        weak_maps: &[ErasedWeakMapBoxPointer],
    ) -> Unreachables {
        // Walk the list, tracing and marking the nodes
        let mut strong_dead = Vec::new();
        let mut pending_ephemerons = Vec::new();

        // === Preliminary mark phase ===
        //
        // 0. Get the naive list of possibly dead nodes.
        for node in strongs {
            // SAFETY: node must be valid as this phase cannot drop any node.
            let node_ref = unsafe { node.as_ref() };
            if node_ref.is_rooted() {
                tracer.enqueue(*node);

                // SAFETY: all nodes must be valid as this phase cannot drop any node.
                unsafe {
                    tracer.trace_until_empty();
                }
            } else if !node_ref.is_marked() {
                strong_dead.push(*node);
            }
        }

        // 0.1. Early return if there are no ephemerons in the GC
        if weaks.is_empty() {
            strong_dead.retain_mut(|node| {
                // SAFETY: node must be valid as this phase cannot drop any node.
                unsafe { !node.as_ref().is_marked() }
            });
            return Unreachables {
                strong: strong_dead,
                weak: Vec::new(),
            };
        }

        // === Weak mark phase ===
        //
        //
        // 1. Get the naive list of ephemerons that are supposedly dead or their key is dead and
        // trace all the ephemerons that have roots and their keys are live. Also remove from
        // this list the ephemerons that are marked but their value is dead.
        for eph in weaks {
            // SAFETY: node must be valid as this phase cannot drop any node.
            let eph_ref = unsafe { eph.as_ref() };
            let header = eph_ref.header();
            if header.is_rooted() {
                header.mark();
            }
            // SAFETY: the garbage collector ensures `eph_ref` always points to valid data.
            if unsafe { !eph_ref.trace(tracer) } {
                pending_ephemerons.push(*eph);
            }

            // SAFETY: all nodes must be valid as this phase cannot drop any node.
            unsafe {
                tracer.trace_until_empty();
            }
        }

        // 2. Trace all the weak pointers in the live weak maps to make sure they do not get swept.
        for w in weak_maps {
            // SAFETY: node must be valid as this phase cannot drop any node.
            let node_ref = unsafe { w.as_ref() };

            // SAFETY: The garbage collector ensures that all nodes are valid.
            unsafe { node_ref.trace(tracer) };

            // SAFETY: all nodes must be valid as this phase cannot drop any node.
            unsafe {
                tracer.trace_until_empty();
            }
        }

        // 3. Iterate through all pending ephemerons, removing the ones which have been successfully
        // traced. If there are no changes in the pending ephemerons list, it means that there are no
        // more reachable ephemerons from the remaining ephemeron values.
        let mut previous_len = pending_ephemerons.len();
        loop {
            pending_ephemerons.retain_mut(|eph| {
                // SAFETY: node must be valid as this phase cannot drop any node.
                let eph_ref = unsafe { eph.as_ref() };
                // SAFETY: the garbage collector ensures `eph_ref` always points to valid data.
                let is_key_marked = unsafe { !eph_ref.trace(tracer) };

                // SAFETY: all nodes must be valid as this phase cannot drop any node.
                unsafe {
                    tracer.trace_until_empty();
                }

                is_key_marked
            });

            if previous_len == pending_ephemerons.len() {
                break;
            }

            previous_len = pending_ephemerons.len();
        }

        // 4. The remaining list should contain the ephemerons that are either unreachable or its key
        // is dead. Cleanup the strong pointers since this procedure could have marked some more strong
        // pointers.
        strong_dead.retain_mut(|node| {
            // SAFETY: node must be valid as this phase cannot drop any node.
            unsafe { !node.as_ref().is_marked() }
        });

        Unreachables {
            strong: strong_dead,
            weak: pending_ephemerons,
        }
    }

    /// # Safety
    ///
    /// Passing a `strong` or a `weak` vec with invalid pointers will result in Undefined Behaviour.
    unsafe fn finalize(unreachables: Unreachables) {
        for node in unreachables.strong {
            // SAFETY: The caller must ensure all pointers inside `unreachables.strong` are valid.
            let node_ref = unsafe { node.as_ref() };
            let run_finalizer_fn = node_ref.run_finalizer_fn();

            // SAFETY: The function pointer is appropriate for this node type because we extract it from it's VTable.
            unsafe {
                run_finalizer_fn(node);
            }
        }
        for node in unreachables.weak {
            // SAFETY: The caller must ensure all pointers inside `unreachables.weak` are valid.
            let node = unsafe { node.as_ref() };
            node.finalize_and_clear();
        }
    }

    /// # Safety
    ///
    /// - Providing an invalid pointer in the `heap_start` or in any of the headers of each
    ///   node will result in Undefined Behaviour.
    /// - Providing a list of pointers that weren't allocated by `Box::into_raw(Box::new(..))`
    ///   will result in Undefined Behaviour.
    unsafe fn sweep(
        strong: &mut Vec<GcErasedPointer>,
        weak: &mut Vec<EphemeronPointer>,
        total_allocated: &mut usize,
    ) {
        let _guard = DropGuard::new();

        strong.retain(|node| {
            // SAFETY: The caller must ensure the validity of every node of `heap_start`.
            let node_ref = unsafe { node.as_ref() };
            if node_ref.is_marked() {
                node_ref.header.unmark();
                node_ref.reset_non_root_count();

                true
            } else {
                // SAFETY: The algorithm ensures only unmarked/unreachable pointers are dropped.
                // The caller must ensure all pointers were allocated by `Box::into_raw(Box::new(..))`.
                let drop_fn = node_ref.drop_fn();
                let size = node_ref.size();
                *total_allocated -= size;

                // SAFETY: The function pointer is appropriate for this node type because we extract it from it's VTable.
                unsafe {
                    drop_fn(*node);
                }

                false
            }
        });

        weak.retain(|eph| {
            // SAFETY: The caller must ensure the validity of every node of `heap_start`.
            let eph_ref = unsafe { eph.as_ref() };
            let header = eph_ref.header();
            if header.is_marked() {
                header.unmark();
                header.reset_non_root_count();

                true
            } else {
                // SAFETY: The algorithm ensures only unmarked/unreachable pointers are dropped.
                // The caller must ensure all pointers were allocated by `Box::into_raw(Box::new(..))`.
                let unmarked_eph = unsafe { Box::from_raw(eph.as_ptr()) };
                let unallocated_bytes = size_of_val(&*unmarked_eph);
                *total_allocated -= unallocated_bytes;

                false
            }
        });
    }

    // Clean up the heap when BoaGc is dropped
    fn dump(gc: &mut BoaGc) {
        // Weak maps have to be dropped first, since the process dereferences GcBoxes.
        // This can be done without initializing a dropguard since no GcBox's are being dropped.
        for node in mem::take(&mut gc.weak_maps) {
            // SAFETY:
            // The `Allocator` must always ensure its start node is a valid, non-null pointer that
            // was allocated by `Box::from_raw(Box::new(..))`.
            let _unmarked_node = unsafe { Box::from_raw(node.as_ptr()) };
        }

        // Not initializing a dropguard since this should only be invoked when BOA_GC is being dropped.
        let _guard = DropGuard::new();

        for node in mem::take(&mut gc.strongs) {
            // SAFETY:
            // The `Allocator` must always ensure its start node is a valid, non-null pointer that
            // was allocated by `Box::from_raw(Box::new(..))`.
            let drop_fn = unsafe { node.as_ref() }.drop_fn();

            // SAFETY: The function pointer is appropriate for this node type because we extract it from it's VTable.
            unsafe {
                drop_fn(node);
            }
        }

        for node in mem::take(&mut gc.weaks) {
            // SAFETY:
            // The `Allocator` must always ensure its start node is a valid, non-null pointer that
            // was allocated by `Box::from_raw(Box::new(..))`.
            let _unmarked_node = unsafe { Box::from_raw(node.as_ptr()) };
        }
    }
}

/// Returns the current heap occupancy as `(bytes_allocated, threshold)`: how
/// much garbage the collector tolerates between cycles.
#[must_use]
pub fn heap_stats() -> (usize, usize) {
    BOA_GC.with(|current| {
        let gc = current.borrow();
        (gc.runtime.bytes_allocated, gc.config.threshold)
    })
}

/// A non-rooting heap address, valid only while its allocation remains live.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CensusKey(GcErasedPointer);

impl<T: Trace + ?Sized> Gc<T> {
    /// Returns this allocation's non-rooting census key.
    #[must_use]
    pub fn census_key(&self) -> CensusKey {
        CensusKey(self.as_erased_pointer())
    }
}

/// A mark-only pass started by [`census`].
#[allow(missing_debug_implementations)]
pub struct Census {
    tracer: Tracer,
    // Entries of reached weak maps whose key the census left unmarked. Script
    // reaches such a value through `map.get(key)` whenever it reaches the key,
    // so `children` reports it as one of the key's edges.
    values_by_key: HashMap<GcErasedPointer, Vec<EphemeronPointer>>,
}

impl Census {
    /// Was `key` reached from a root other than the discounted handles?
    ///
    /// # Safety
    /// `key` must name an allocation kept live for the whole census.
    #[must_use]
    pub unsafe fn is_marked(&self, key: CensusKey) -> bool {
        // SAFETY: the caller keeps the allocation live.
        unsafe { key.0.as_ref() }.is_marked()
    }

    /// Returns strong children without marking them.
    ///
    /// # Safety
    /// `key` must name an allocation kept live for the whole census.
    pub unsafe fn children(&mut self, key: CensusKey) -> Vec<CensusKey> {
        self.tracer.record = Some(Vec::new());
        // SAFETY: the caller keeps the allocation live; its own vtable supplies trace.
        let trace_fn = unsafe { key.0.as_ref() }.trace_fn();
        unsafe { trace_fn(key.0, &mut self.tracer) }
        if let Some(entries) = self.values_by_key.get(&key.0) {
            for eph in entries {
                // SAFETY: nothing is swept during a census, so the entry is live.
                unsafe { eph.as_ref().census_trace_value(&mut self.tracer) }
            }
        }
        self.tracer
            .record
            .take()
            .unwrap_or_default()
            .into_iter()
            .map(CensusKey)
            .collect()
    }
}

// A callback may unwind; never leave census marks or discounted root counts
// behind for the next real collection.
struct CensusReset<'a>(&'a BoaGc);

impl Drop for CensusReset<'_> {
    fn drop(&mut self) {
        for node in &self.0.strongs {
            // SAFETY: nothing is swept during a census.
            let node = unsafe { node.as_ref() };
            node.header.unmark();
            node.reset_non_root_count();
        }
        for eph in &self.0.weaks {
            // SAFETY: nothing is swept during a census.
            let header = unsafe { eph.as_ref() }.header();
            header.unmark();
            header.reset_non_root_count();
        }
    }
}

/// Marks the heap as a collection would, discounting one external handle for
/// each entry in `discounted`. Nothing is finalized or freed. A reached
/// WeakRef target counts as reachable because script can recover it; weak map
/// entries keep their usual ephemeron semantics.
///
/// # Safety
/// Every key must remain live throughout this call. `f` must not allocate,
/// drop, or clone GC handles, or otherwise change the heap or its edges.
pub unsafe fn census<R>(discounted: &[CensusKey], f: impl FnOnce(&mut Census) -> R) -> R {
    BOA_GC.with(|current| {
        let gc = current.borrow_mut();
        let _reset = CensusReset(&gc);
        Collector::trace_non_roots(&gc);
        for key in discounted {
            // SAFETY: the caller keeps each allocation live.
            unsafe { key.0.as_ref() }.header.inc_non_root_count();
        }
        let mut census = Census {
            tracer: Tracer::new(),
            values_by_key: HashMap::new(),
        };
        drop(Collector::mark_heap(
            &mut census.tracer,
            &gc.strongs,
            &gc.weaks,
            &gc.weak_maps,
        ));
        // Script can deref a WeakRef, so a reached one roots its target. A
        // WeakMap or WeakSet never yields its keys: its entries keep their
        // values only while the key is reached, as in a collection.
        let mut map_list = Vec::new();
        for map in &gc.weak_maps {
            // SAFETY: every entry in the heap list is live.
            unsafe { map.as_ref() }.census_entries(&mut map_list);
        }
        let map_entries: HashSet<NonNull<u8>> =
            map_list.iter().map(|eph| eph.cast::<u8>()).collect();
        // Externally rooted ephemerons are bridge identity caches, not
        // script-visible weak references. Each pass drops the entries it
        // settles, so a rescan only visits what is still pending.
        let mut pending: Vec<EphemeronPointer> = gc
            .weaks
            .iter()
            .copied()
            // SAFETY: every entry in the heap list is live.
            .filter(|eph| !unsafe { eph.as_ref() }.header().is_rooted())
            .collect();
        loop {
            let before = pending.len();
            pending.retain(|eph| {
                // SAFETY: every entry in the heap list is live, and the census
                // never sweeps or mutates the heap.
                unsafe {
                    let eph_ref = eph.as_ref();
                    if !eph_ref.header().is_marked() {
                        return true;
                    }
                    let settled = if map_entries.contains(&eph.cast::<u8>()) {
                        eph_ref.trace(&mut census.tracer)
                    } else {
                        eph_ref.census_root(&mut census.tracer);
                        true
                    };
                    census.tracer.trace_until_empty();
                    !settled
                }
            });
            if pending.len() == before {
                break;
            }
        }
        for eph in map_list {
            // SAFETY: every entry in the heap list is live.
            let eph_ref = unsafe { eph.as_ref() };
            if !eph_ref.header().is_marked() {
                continue;
            }
            let Some(key) = eph_ref.census_key() else {
                continue;
            };
            // SAFETY: a live entry's key is live.
            if !unsafe { key.as_ref() }.is_marked() {
                census.values_by_key.entry(key).or_default().push(eph);
            }
        }
        f(&mut census)
    })
}

/// Forcefully runs a garbage collection of all inaccessible nodes.
pub fn force_collect() {
    BOA_GC.with(|current| {
        let mut gc = current.borrow_mut();

        if gc.runtime.bytes_allocated > 0 {
            Collector::collect(&mut gc);
        }
    });
}

#[cfg(test)]
mod test;

/// Returns `true` is any weak maps are currently allocated.
#[cfg(test)]
#[must_use]
pub fn has_weak_maps() -> bool {
    BOA_GC.with(|current| {
        let gc = current.borrow();

        !gc.weak_maps.is_empty()
    })
}
