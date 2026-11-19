//! A node's DOM child list in one word: three in four parents have exactly one child, which is
//! stored inline as a tagged slot index instead of in a heap list of its own.

use std::mem::ManuallyDrop;
use thin_vec::ThinVec;

/// Children as slot indexes. Low bit 1: one child, `index << 1 | 1`. Low bit 0: a `ThinVec`
/// (its pointer is at least 4-aligned), never holding exactly one entry. Empty is the empty
/// `ThinVec`, which does not allocate.
pub(crate) struct ChildList {
    repr: Repr,
}

union Repr {
    many: ManuallyDrop<ThinVec<u32>>,
    raw: usize,
}

const _: () = assert!(std::mem::size_of::<ThinVec<u32>>() == std::mem::size_of::<usize>());
const _: () = assert!(std::mem::align_of::<ThinVec<u32>>() == std::mem::align_of::<usize>());

/// The largest index an inline child can hold, with one bit kept for the tag.
const MAX_INLINE: u32 = (usize::MAX >> 1) as u32;

/// Borrowed contents of a [`ChildList`].
#[derive(Clone, Copy)]
pub(crate) enum ChildSlots<'a> {
    One(u32),
    Many(&'a [u32]),
}

impl ChildList {
    pub(crate) fn new() -> Self {
        Self::from_vec(ThinVec::new())
    }

    fn one(index: u32) -> Self {
        assert!(index <= MAX_INLINE, "slot index {index} too large for an inline child");
        Self {
            repr: Repr {
                raw: (index as usize) << 1 | 1,
            },
        }
    }

    fn from_vec(vec: ThinVec<u32>) -> Self {
        Self {
            repr: Repr {
                many: ManuallyDrop::new(vec),
            },
        }
    }

    /// The canonical list for `vec`: inline when it holds one child.
    fn normalized(vec: ThinVec<u32>) -> Self {
        if vec.len() == 1 { Self::one(vec[0]) } else { Self::from_vec(vec) }
    }

    #[inline]
    fn inline(&self) -> Option<u32> {
        // SAFETY: both fields are one word, and every bit pattern is a valid `usize`.
        let raw = unsafe { self.repr.raw };
        (raw & 1 == 1).then_some((raw >> 1) as u32)
    }

    #[inline]
    pub(crate) fn slots(&self) -> ChildSlots<'_> {
        match self.inline() {
            Some(index) => ChildSlots::One(index),
            // SAFETY: an even word is the `ThinVec` variant.
            None => ChildSlots::Many(unsafe { &self.repr.many }),
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        match self.slots() {
            ChildSlots::One(_) => 1,
            ChildSlots::Many(slots) => slots.len(),
        }
    }

    /// Moves the contents out as a `ThinVec`, leaving the list empty.
    pub(crate) fn take_vec(&mut self) -> ThinVec<u32> {
        match self.inline() {
            Some(index) => {
                *self = Self::new();
                let mut vec = ThinVec::with_capacity(1);
                vec.push(index);
                vec
            }
            None => {
                // SAFETY: an even word is the `ThinVec` variant; it is replaced before `self` is
                // used again, so it is never dropped twice.
                let vec = unsafe { ManuallyDrop::take(&mut self.repr.many) };
                self.repr = Repr {
                    many: ManuallyDrop::new(ThinVec::new()),
                };
                vec
            }
        }
    }

    /// Applies `f` to the contents as a `ThinVec`, then stores the result canonically.
    pub(crate) fn edit<R>(&mut self, f: impl FnOnce(&mut ThinVec<u32>) -> R) -> R {
        let mut vec = self.take_vec();
        let result = f(&mut vec);
        *self = Self::normalized(vec);
        result
    }

    pub(crate) fn push(&mut self, index: u32) {
        match self.slots() {
            ChildSlots::Many([]) => *self = Self::one(index),
            ChildSlots::One(first) => {
                let mut vec = ThinVec::with_capacity(4);
                vec.push(first);
                vec.push(index);
                *self = Self::from_vec(vec);
            }
            // SAFETY: an even word is the `ThinVec` variant.
            ChildSlots::Many(_) => unsafe { (*self.repr.many).push(index) },
        }
    }

    pub(crate) fn insert(&mut self, i: usize, index: u32) {
        if i == self.len() {
            return self.push(index);
        }
        self.edit(|vec| vec.insert(i, index));
    }

    pub(crate) fn remove(&mut self, i: usize) -> u32 {
        self.edit(|vec| vec.remove(i))
    }

    pub(crate) fn retain(&mut self, keep: impl FnMut(&u32) -> bool) {
        self.edit(|vec| vec.retain(keep));
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::new();
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        if self.inline().is_none() {
            // SAFETY: an even word is the `ThinVec` variant.
            unsafe { (*self.repr.many).shrink_to_fit() };
        }
    }
}

impl Default for ChildList {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ChildList {
    fn drop(&mut self) {
        if self.inline().is_none() {
            // SAFETY: an even word is the `ThinVec` variant, dropped exactly once here.
            unsafe { ManuallyDrop::drop(&mut self.repr.many) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents(list: &ChildList) -> Vec<u32> {
        match list.slots() {
            ChildSlots::One(index) => vec![index],
            ChildSlots::Many(slots) => slots.to_vec(),
        }
    }

    #[test]
    fn one_child_is_inline_and_every_edit_stays_canonical() {
        let mut list = ChildList::new();
        assert!(matches!(list.slots(), ChildSlots::Many([])));
        list.push(7);
        assert!(matches!(list.slots(), ChildSlots::One(7)));
        list.push(9);
        list.insert(0, 3);
        assert_eq!(contents(&list), [3, 7, 9]);
        assert_eq!(list.remove(1), 7);
        list.retain(|&index| index != 3);
        assert!(matches!(list.slots(), ChildSlots::One(9)), "one left goes back inline");
        list.insert(1, 4);
        list.insert(0, 2);
        assert_eq!(contents(&list), [2, 9, 4]);
        assert_eq!(contents(&ChildList::normalized(list.take_vec())), [2, 9, 4]);
        assert!(matches!(list.slots(), ChildSlots::Many([])));
        list.push(MAX_INLINE);
        assert!(matches!(list.slots(), ChildSlots::One(MAX_INLINE)));
        list.clear();
        assert_eq!(list.len(), 0);
    }
}
