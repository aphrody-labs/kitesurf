//! Typed slot pools for the per-node boxes a document holds hundreds of thousands of.
//!
//! A `PoolBox<T>` is a `Box<T>` whose slot comes from a pool of same-typed slots carved from
//! large chunks: no allocator header or rounding per value, and a freed slot is reused only by
//! another `T`, which outlives the short-lived allocations that fragment the general heap.
//! Chunks are never returned, so a slot pointer stays valid wherever its box is dropped.

use std::alloc::{Layout, alloc, handle_alloc_error};
use std::marker::PhantomData;
use std::mem::{align_of, size_of};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::Mutex;

/// Bytes per chunk: enough that the chunk headers are noise, small enough that a document
/// with few values of a type strands little.
const CHUNK_BYTES: usize = 64 << 10;

pub struct Pool {
    /// Freed slots, linked through their first word.
    free: *mut u8,
    /// The unused tail of the newest chunk.
    bump: *mut u8,
    end: *mut u8,
}

// SAFETY: the raw pointers only address chunk memory that is never freed; the pool is only
// touched under its Mutex.
unsafe impl Send for Pool {}

impl Pool {
    pub const fn new() -> Self {
        Self {
            free: std::ptr::null_mut(),
            bump: std::ptr::null_mut(),
            end: std::ptr::null_mut(),
        }
    }
}

/// A type whose boxes come from a pool. `slot_layout` must cover `T` and hold a pointer, and the
/// pool must only ever be used by this one type.
pub unsafe trait Pooled: Sized {
    fn pool() -> &'static Mutex<Pool>;
}

const fn slot_layout<T>() -> Layout {
    let size = if size_of::<T>() > size_of::<usize>() { size_of::<T>() } else { size_of::<usize>() };
    let align = if align_of::<T>() > align_of::<usize>() { align_of::<T>() } else { align_of::<usize>() };
    match Layout::from_size_align(size, align) {
        Ok(layout) => layout.pad_to_align(),
        Err(_) => panic!("pooled type has an invalid layout"),
    }
}

fn take_slot<T: Pooled>() -> NonNull<T> {
    let layout = slot_layout::<T>();
    let mut pool = T::pool().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = NonNull::new(pool.free) {
        // SAFETY: a free slot holds the next free pointer in its first word (see `give_slot`).
        pool.free = unsafe { slot.as_ptr().cast::<*mut u8>().read() };
        return slot.cast();
    }
    if (pool.end as usize) - (pool.bump as usize) < layout.size() {
        let chunk = Layout::from_size_align(CHUNK_BYTES.max(layout.size()), layout.align())
            .unwrap_or(layout);
        // SAFETY: `chunk` has a non-zero size.
        let start = unsafe { alloc(chunk) };
        if start.is_null() {
            handle_alloc_error(chunk);
        }
        pool.bump = start;
        // SAFETY: `start + chunk.size()` is one past the end of the allocation.
        pool.end = unsafe { start.add(chunk.size()) };
    }
    let slot = pool.bump;
    // SAFETY: the check above leaves at least one slot between `bump` and `end`.
    pool.bump = unsafe { slot.add(layout.size()) };
    // SAFETY: `slot` is inside a live chunk, so it is not null.
    unsafe { NonNull::new_unchecked(slot) }.cast()
}

/// # Safety
/// `slot` came from `take_slot::<T>()` and holds no live value.
unsafe fn give_slot<T: Pooled>(slot: NonNull<T>) {
    let mut pool = T::pool().lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: a slot is at least pointer-sized and pointer-aligned (`slot_layout`).
    unsafe { slot.as_ptr().cast::<*mut u8>().write(pool.free) };
    pool.free = slot.as_ptr().cast();
}

pub struct PoolBox<T: Pooled> {
    ptr: NonNull<T>,
    _owns: PhantomData<T>,
}

impl<T: Pooled> PoolBox<T> {
    pub fn new(value: T) -> Self {
        let ptr = take_slot::<T>();
        // SAFETY: the slot is fresh, sized and aligned for `T`.
        unsafe { ptr.as_ptr().write(value) };
        Self { ptr, _owns: PhantomData }
    }

    pub fn into_inner(this: Self) -> T {
        let ptr = this.ptr;
        std::mem::forget(this);
        // SAFETY: the value is live, and the slot is released without dropping it again.
        let value = unsafe { ptr.as_ptr().read() };
        // SAFETY: the value was moved out above.
        unsafe { give_slot(ptr) };
        value
    }
}

impl<T: Pooled> Drop for PoolBox<T> {
    fn drop(&mut self) {
        // SAFETY: the box owns a live value in its slot, dropped exactly once here.
        unsafe { std::ptr::drop_in_place(self.ptr.as_ptr()) };
        // SAFETY: the value was dropped above.
        unsafe { give_slot(self.ptr) };
    }
}

impl<T: Pooled> Deref for PoolBox<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the box owns a live value.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: Pooled> DerefMut for PoolBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the box owns a live value and `&mut self` is unique.
        unsafe { self.ptr.as_mut() }
    }
}

impl<T: Pooled + Clone> Clone for PoolBox<T> {
    fn clone(&self) -> Self {
        Self::new(T::clone(self))
    }
}

impl<T: Pooled + Default> Default for PoolBox<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: Pooled + std::fmt::Debug> std::fmt::Debug for PoolBox<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        T::fmt(self, f)
    }
}

// SAFETY: a PoolBox owns its value like a Box; the pool it returns the slot to is a Mutex.
unsafe impl<T: Pooled + Send> Send for PoolBox<T> {}
// SAFETY: as for Box, shared access only hands out `&T`.
unsafe impl<T: Pooled + Sync> Sync for PoolBox<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct Probe(u64, String);
    unsafe impl Pooled for Probe {
        fn pool() -> &'static Mutex<Pool> {
            static POOL: Mutex<Pool> = Mutex::new(Pool::new());
            &POOL
        }
    }

    #[test]
    fn freed_slots_are_reused_and_values_survive() {
        let boxes: Vec<_> = (0..5000).map(|i| PoolBox::new(Probe(i, i.to_string()))).collect();
        for (i, b) in boxes.iter().enumerate() {
            assert_eq!(**b, Probe(i as u64, i.to_string()));
        }
        let addr = &*boxes[10] as *const Probe as usize;
        drop(boxes);
        let again = PoolBox::new(Probe(7, "x".into()));
        let reused: Vec<_> = (0..5000).map(|i| PoolBox::new(Probe(i, String::new()))).collect();
        assert!(reused.iter().chain([&again]).any(|b| &**b as *const Probe as usize == addr));
        assert_eq!(PoolBox::into_inner(again), Probe(7, "x".into()));
    }
}
