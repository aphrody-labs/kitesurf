use std::cell::UnsafeCell;
use std::fmt;
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, Ordering};
use style::data::{ElementDataMut, ElementDataRef, ElementDataWrapper};
use style::servo_arc::Arc;

use crate::layout::damage::ALL_DAMAGE;

/// Per-node atomic state word of element and document nodes.
///
/// Bits 12 to 15 hold whether the node's [`StyloCell`] is initialized, so the cell needs no
/// `Option`, and the traversal flags; `element.rs` owns the rest of the word. Writes are
/// atomic read-modify-writes, so setting one bit never loses a concurrent write to another.
pub struct NodeStateFlags(pub(super) AtomicU32);

impl NodeStateFlags {
    /// The node's [`StyloCell`] holds initialized element data.
    const STYLO_DATA: u32 = 1 << 12;
    /// Some descendant of this node needs restyling. Used by Stylo's incremental style
    /// traversal to skip unchanged subtrees.
    const DIRTY_DESCENDANTS: u32 = 1 << 13;
    /// This node or some descendant may carry `RestyleDamage`. Used by the damage
    /// propagation pass to skip unchanged subtrees.
    const DAMAGED_DESCENDANTS: u32 = 1 << 14;
    /// Stylo has processed this node's snapshot.
    const SNAPSHOT_HANDLED: u32 = 1 << 15;

    /// The word bits owned by this block.
    pub(super) const STATE_MASK: u32 = Self::STYLO_DATA
        | Self::DIRTY_DESCENDANTS
        | Self::DAMAGED_DESCENDANTS
        | Self::SNAPSHOT_HANDLED;

    /// No stylo data, dirty and damaged descendants set, snapshot not handled.
    pub(super) const INITIAL_STATE: u32 = Self::DIRTY_DESCENDANTS | Self::DAMAGED_DESCENDANTS;

    #[inline]
    fn load(&self, bit: u32, order: Ordering) -> bool {
        self.0.load(order) & bit != 0
    }

    #[inline]
    fn store(&self, bit: u32, value: bool, order: Ordering) {
        if value {
            self.0.fetch_or(bit, order);
        } else {
            self.0.fetch_and(!bit, order);
        }
    }

    /// Sets `bit`, returning whether it was already set.
    #[inline]
    fn swap_set(&self, bit: u32, order: Ordering) -> bool {
        self.0.fetch_or(bit, order) & bit != 0
    }

    #[inline]
    fn has_stylo_data(&self) -> bool {
        self.load(Self::STYLO_DATA, Ordering::Acquire)
    }

    #[inline]
    fn set_stylo_data(&self, value: bool) {
        self.store(Self::STYLO_DATA, value, Ordering::Release);
    }

    #[inline]
    pub(crate) fn dirty_descendants(&self) -> bool {
        self.load(Self::DIRTY_DESCENDANTS, Ordering::Relaxed)
    }

    #[inline]
    pub(crate) fn set_dirty_descendants(&self, value: bool) {
        self.store(Self::DIRTY_DESCENDANTS, value, Ordering::Relaxed);
    }

    /// Sets `dirty_descendants`, returning whether it was already set.
    #[inline]
    pub(crate) fn swap_set_dirty_descendants(&self) -> bool {
        self.swap_set(Self::DIRTY_DESCENDANTS, Ordering::Relaxed)
    }

    #[inline]
    pub(crate) fn damaged_descendants(&self) -> bool {
        self.load(Self::DAMAGED_DESCENDANTS, Ordering::Relaxed)
    }

    #[inline]
    pub(crate) fn set_damaged_descendants(&self, value: bool) {
        self.store(Self::DAMAGED_DESCENDANTS, value, Ordering::Relaxed);
    }

    /// Sets `damaged_descendants`, returning whether it was already set.
    #[inline]
    pub(crate) fn swap_set_damaged_descendants(&self) -> bool {
        self.swap_set(Self::DAMAGED_DESCENDANTS, Ordering::Relaxed)
    }

    #[inline]
    pub(crate) fn snapshot_handled(&self) -> bool {
        self.load(Self::SNAPSHOT_HANDLED, Ordering::SeqCst)
    }

    #[inline]
    pub(crate) fn set_snapshot_handled(&self, value: bool) {
        self.store(Self::SNAPSHOT_HANDLED, value, Ordering::SeqCst);
    }
}

impl fmt::Debug for NodeStateFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeStateFlags")
            .field("stylo_data", &self.has_stylo_data())
            .field("dirty_descendants", &self.dirty_descendants())
            .field("damaged_descendants", &self.damaged_descendants())
            .field("snapshot_handled", &self.snapshot_handled())
            .finish()
    }
}

/// Interior-mutable storage for a node's `ElementDataWrapper`; accessed through the
/// [`StyloData`] and [`StyloDataMut`] views, which pair it with the node's
/// [`NodeStateFlags`].
///
/// Invariant: while the `STYLO_DATA` flag is clear the cell holds
/// `ElementDataWrapper::default()`, which retains no styles.
pub struct StyloCell {
    inner: UnsafeCell<ElementDataWrapper>,
}

impl Default for StyloCell {
    fn default() -> Self {
        Self {
            inner: UnsafeCell::new(ElementDataWrapper::default()),
        }
    }
}

impl fmt::Debug for StyloCell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StyloCell").finish_non_exhaustive()
    }
}

/// Shared view of a node's stylo element data: the [`StyloCell`] plus the flag recording
/// whether it is initialized.
///
/// Encapsulates the `UnsafeCell` so that access sites don't need raw `unsafe` blocks.
///
/// Safety relies on:
///   - Regular static-borrow checking for regular access, and on:
///   - Stylo having exclusive access to nodes during style traversals
///   - Stylo's thread-safe traversal model: `init`/`clear` only happen during exclusive-access phases
#[derive(Clone, Copy)]
pub struct StyloData<'a> {
    cell: &'a StyloCell,
    flags: &'a NodeStateFlags,
}

impl fmt::Debug for StyloData<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StyloData").finish_non_exhaustive()
    }
}

impl<'a> StyloData<'a> {
    #[inline]
    pub(crate) fn new(cell: &'a StyloCell, flags: &'a NodeStateFlags) -> Self {
        Self { cell, flags }
    }

    /// Shared reference to the wrapper, initialized or not.
    #[inline]
    fn wrapper_unchecked(self) -> &'a ElementDataWrapper {
        // SAFETY: the cell is only written by `ensure_init`/`clear`, whose callers guarantee
        // no outstanding borrows, or through `&mut StyloCell`.
        unsafe { &*self.cell.inner.get() }
    }

    #[inline]
    fn wrapper(self) -> Option<&'a ElementDataWrapper> {
        self.has_data().then(|| self.wrapper_unchecked())
    }

    /// Whether element data has been initialized.
    #[inline]
    pub fn has_data(self) -> bool {
        self.flags.has_stylo_data()
    }

    /// Borrow the element data immutably, if present.
    pub fn get(self) -> Option<ElementDataRef<'a>> {
        self.wrapper().map(|w| w.borrow())
    }

    pub fn primary_styles(self) -> Option<StyleDataRef<'a>> {
        // One borrow, not two: paint calls this per element and per glyph
        // run, and the discarded first borrow was an atomic round trip.
        let data = self.get()?;
        if data.styles.get_primary().is_some() {
            Some(StyleDataRef(data))
        } else {
            None
        }
    }

    /// Borrow the primary computed style directly (rather than the `Arc` containing it).
    pub fn computed_styles(self) -> Option<ComputedStyleRef<'a>> {
        self.primary_styles().map(ComputedStyleRef)
    }

    /// Get a mutable reference to the data
    pub unsafe fn unsafe_stylo_only_mut(self) -> Option<ElementDataMut<'a>> {
        self.wrapper().map(|w| w.borrow_mut())
    }

    /// Initialize the element data ready for use (if it is not already initialized)
    ///
    /// SAFETY:
    /// There must be no outstanding borrows to this container or anything contained within it
    /// when this method is called
    pub unsafe fn ensure_init(self) -> ElementDataMut<'a> {
        let initialized = self.has_data();
        if !initialized {
            unsafe { *self.cell.inner.get() = ElementDataWrapper::default() };
            self.flags.set_stylo_data(true);
        }
        let mut data_mut = self.wrapper_unchecked().borrow_mut();
        if !initialized {
            data_mut.damage = ALL_DAMAGE;
        }
        data_mut
    }

    /// Clear the element data, returning to the uninitialized state.
    ///
    /// SAFETY:
    /// There must be no outstanding borrows to this container or anything contained within it
    /// when this method is called
    pub unsafe fn clear(self) {
        // Replacing the wrapper drops the styles it held, as `= None` did.
        unsafe { *self.cell.inner.get() = ElementDataWrapper::default() };
        self.flags.set_stylo_data(false);
    }
}

/// Exclusive view of a node's stylo element data; see [`StyloData`].
pub struct StyloDataMut<'a> {
    cell: &'a mut StyloCell,
    flags: &'a NodeStateFlags,
}

impl<'a> StyloDataMut<'a> {
    #[inline]
    pub(crate) fn new(cell: &'a mut StyloCell, flags: &'a NodeStateFlags) -> Self {
        Self { cell, flags }
    }

    /// Whether element data has been initialized.
    #[inline]
    pub fn has_data(&self) -> bool {
        self.flags.has_stylo_data()
    }

    /// Borrow the element data mutably, if present.
    pub fn get_mut(self) -> Option<ElementDataMut<'a>> {
        if !self.has_data() {
            return None;
        }
        Some(self.cell.inner.get_mut().borrow_mut())
    }

    /// Initialize the element data ready for use (if it is not already initialized)
    pub fn ensure_init_mut(self) -> ElementDataMut<'a> {
        // Exclusive access to the cell (implied by `&mut StyloCell`) makes it safe to mutate.
        let initialized = self.has_data();
        let wrapper = self.cell.inner.get_mut();
        if !initialized {
            *wrapper = ElementDataWrapper::default();
            self.flags.set_stylo_data(true);
        }
        let mut data_mut = wrapper.borrow_mut();
        if !initialized {
            data_mut.damage = ALL_DAMAGE;
        }
        data_mut
    }
}

pub struct StyleDataRef<'a>(ElementDataRef<'a>);

impl Deref for StyleDataRef<'_> {
    type Target = Arc<style::properties::ComputedValues>;

    fn deref(&self) -> &Self::Target {
        self.0.styles.get_primary().unwrap()
    }
}

/// Borrow of a node's stylo element data that `Deref`s directly to the primary
/// [`ComputedValues`](style::properties::ComputedValues), suitable for wrapping
/// in [`stylo_taffy::TaffyStyloStyle`].
pub struct ComputedStyleRef<'a>(StyleDataRef<'a>);

impl Deref for ComputedStyleRef<'_> {
    type Target = style::properties::ComputedValues;

    fn deref(&self) -> &Self::Target {
        self.0.0.styles.get_primary().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::NodeStateFlags;
    use crate::layout::damage::ALL_DAMAGE;
    use crate::node::{DocumentData, ElementData};
    use style::properties::ComputedValues;
    use style::properties::style_structs::Font;
    use style::selector_parser::RestyleDamage;
    use style::servo_arc::Arc;

    fn element() -> ElementData {
        ElementData::new(crate::qual_name!("div", html), Vec::new())
    }

    fn style() -> Arc<ComputedValues> {
        ComputedValues::initial_values_with_font_override(Font::initial_values()).to_arc()
    }

    /// (stylo data, dirty, damaged, snapshot handled)
    fn state(flags: &NodeStateFlags) -> (bool, bool, bool, bool) {
        (
            flags.has_stylo_data(),
            flags.dirty_descendants(),
            flags.damaged_descendants(),
            flags.snapshot_handled(),
        )
    }

    #[test]
    fn fresh_element_has_no_stylo_data_and_starts_dirty_and_damaged() {
        let el = element();
        assert!(!el.stylo_data().has_data());
        assert!(el.stylo_data().get().is_none());
        assert!(el.stylo_data().primary_styles().is_none());
        assert_eq!(state(&el.state_flags), (false, true, true, false));
        assert_eq!(
            state(&DocumentData::new().state_flags),
            (false, true, true, false)
        );
    }

    #[test]
    fn ensure_init_sets_all_damage_only_on_first_init() {
        let mut el = element();
        assert_eq!(el.stylo_data_mut().ensure_init_mut().damage, ALL_DAMAGE);
        assert!(el.stylo_data().has_data());
        el.stylo_data_mut().get_mut().unwrap().damage = RestyleDamage::empty();
        assert_eq!(
            el.stylo_data_mut().ensure_init_mut().damage,
            RestyleDamage::empty()
        );

        let other = element();
        // SAFETY: no borrows of `other`'s stylo data are outstanding.
        let damage = unsafe { other.stylo_data().ensure_init() }.damage;
        assert_eq!(damage, ALL_DAMAGE);
        assert!(other.stylo_data().get().is_some());
    }

    #[test]
    fn traversal_flags_and_stylo_data_bit_are_independent() {
        let el = element();
        let flags = &el.state_flags;
        for initialized in [false, true] {
            if initialized {
                // SAFETY: no borrows of `el`'s stylo data are outstanding.
                drop(unsafe { el.stylo_data().ensure_init() });
            }
            flags.set_dirty_descendants(false);
            assert_eq!(state(flags), (initialized, false, true, false));
            flags.set_damaged_descendants(false);
            assert_eq!(state(flags), (initialized, false, false, false));
            flags.set_snapshot_handled(true);
            assert_eq!(state(flags), (initialized, false, false, true));
            assert!(!flags.swap_set_dirty_descendants());
            assert!(flags.swap_set_dirty_descendants());
            assert!(!flags.swap_set_damaged_descendants());
            flags.set_snapshot_handled(false);
            assert_eq!(state(flags), (initialized, true, true, false));
        }
        flags.set_snapshot_handled(true);
        flags.set_dirty_descendants(false);
        // SAFETY: no borrows of `el`'s stylo data are outstanding.
        unsafe { el.stylo_data().clear() };
        assert_eq!(state(flags), (false, false, true, true));
    }

    #[test]
    fn clear_drops_the_stored_styles() {
        let mut el = element();
        let primary = style();
        assert!(primary.is_unique());
        el.stylo_data_mut().ensure_init_mut().styles.primary = Some(primary.clone());
        assert!(!primary.is_unique());
        assert!(Arc::ptr_eq(
            &el.stylo_data().primary_styles().unwrap(),
            &primary
        ));

        // SAFETY: no borrows of `el`'s stylo data are outstanding.
        unsafe { el.stylo_data().clear() };
        assert!(primary.is_unique(), "clear must release the primary style");
        assert!(!el.stylo_data().has_data());
        assert!(el.stylo_data().get().is_none());
        assert!(el.stylo_data_mut().get_mut().is_none());
        // Reinitialising starts from empty styles, not the cleared ones.
        let data = el.stylo_data_mut().ensure_init_mut();
        assert!(data.styles.primary.is_none());
        assert_eq!(data.damage, ALL_DAMAGE);
    }

    #[test]
    fn clone_resets_stylo_data_and_traversal_flags() {
        let mut el = element();
        el.stylo_data_mut().ensure_init_mut().styles.primary = Some(style());
        el.state_flags.set_dirty_descendants(false);
        el.state_flags.set_damaged_descendants(false);
        el.state_flags.set_snapshot_handled(true);
        let clone = el.clone();
        assert_eq!(state(&clone.state_flags), (false, true, true, false));
        assert!(clone.stylo_data().get().is_none());
        assert_eq!(state(&el.state_flags), (true, false, false, true));

        let mut doc = DocumentData::new();
        doc.stylo_data_mut().ensure_init_mut();
        doc.state_flags.set_dirty_descendants(false);
        doc.state_flags.set_snapshot_handled(true);
        assert_eq!(state(&doc.clone().state_flags), (false, true, true, false));
    }
}
