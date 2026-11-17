use arrayvec::ArrayVec;
use itertools::Itertools;
use std::{cell::Cell, fmt};

use boa_gc::GcRefCell;
use boa_macros::{Finalize, Trace};

use crate::{
    JsString,
    object::shape::{Shape, WeakShape, slot::Slot},
};

#[cfg(test)]
mod tests;

pub(crate) const PIC_CAPACITY: usize = 4;

/// A cached shape-to-slot mapping for a polymorphic inline cache.
#[derive(Clone, Debug, Trace, Finalize)]
pub(crate) struct CacheEntry {
    /// A weak reference is kept to the shape to avoid the shape preventing deallocation.
    pub(crate) shape: WeakShape,
    #[unsafe_ignore_trace]
    pub(crate) slot: Slot,
}

/// An inline cache entry for a property access.
#[derive(Clone, Debug, Trace, Finalize)]
pub(crate) struct InlineCache {
    pub(crate) name: JsString,

    // The first entry inline and the polymorphic overflow boxed, so a site
    // that never runs or stays monomorphic costs one entry instead of PIC_CAPACITY.
    first: GcRefCell<Option<CacheEntry>>,
    rest: GcRefCell<Option<Box<ArrayVec<CacheEntry, { PIC_CAPACITY - 1 }>>>>,

    #[unsafe_ignore_trace]
    pub(crate) megamorphic: Cell<bool>,
}

impl fmt::Display for InlineCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(name:{} entries:", self.name.display_escaped())?;

        if self.megamorphic.get() {
            return write!(f, "(megamorphic))");
        }

        let first = self.first.borrow();
        let rest = self.rest.borrow();
        let entries = first
            .iter()
            .chain(rest.iter().flat_map(|r| r.iter()))
            .map(|e| e.shape.to_addr_usize())
            .format(", ");

        write!(f, "({entries:#x}))")
    }
}

impl InlineCache {
    pub(crate) fn new(name: JsString) -> Self {
        Self {
            name,
            first: GcRefCell::new(None),
            rest: GcRefCell::new(None),
            megamorphic: Cell::new(false),
        }
    }

    pub(crate) fn set(&self, shape: &Shape, slot: Slot) {
        if self.megamorphic.get() {
            return;
        }
        let entry = CacheEntry {
            shape: shape.into(),
            slot,
        };

        let mut first = self.first.borrow_mut();
        if first.is_none() {
            *first = Some(entry);
            return;
        }
        let mut rest = self.rest.borrow_mut();
        // Add a new entry if there's space; a full cache goes megamorphic.
        if rest
            .get_or_insert_with(Box::default)
            .try_push(entry)
            .is_err()
        {
            self.megamorphic.set(true);
            *first = None;
            *rest = None;
        }
    }

    /// Every live entry, first one first (the layout the PIC tests inspect).
    #[cfg(test)]
    pub(crate) fn entries(&self) -> Vec<CacheEntry> {
        let first = self.first.borrow();
        let rest = self.rest.borrow();
        first
            .iter()
            .chain(rest.iter().flat_map(|r| r.iter()))
            .cloned()
            .collect()
    }

    /// Returns the cached `(Shape, Slot)` if a matching shape exists in the inline cache.
    ///
    /// Opportunistically cleans up stale weak shape references during lookup.
    pub(crate) fn get(&self, shape: &Shape) -> Option<(Shape, Slot)> {
        if self.megamorphic.get() {
            return None;
        }
        let shape_addr = shape.to_addr_usize();

        {
            let mut first = self.first.borrow_mut();
            if let Some(entry) = first.as_ref() {
                match entry.shape.upgrade() {
                    Some(upgraded) if upgraded.to_addr_usize() == shape_addr => {
                        return Some((upgraded, entry.slot));
                    }
                    Some(_) => {}
                    // Opportunistically clean up a stale weak shape.
                    None => *first = None,
                }
            }
        }

        let mut rest = self.rest.borrow_mut();
        let entries = rest.as_mut()?;
        let mut i = 0;
        while i < entries.len() {
            if let Some(upgraded) = entries[i].shape.upgrade() {
                if upgraded.to_addr_usize() == shape_addr {
                    return Some((upgraded, entries[i].slot));
                }
                i += 1;
            } else {
                // Opportunistically clean up stale weak shapes.
                entries.swap_remove(i);
            }
        }
        None
    }
}
