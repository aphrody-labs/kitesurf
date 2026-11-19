//! A cache for storing the results of layout computation

#![allow(clippy::unusual_byte_groupings)]

#[cfg(feature = "content_size")]
use crate::geometry::Rect;
use crate::geometry::{Point, Size};
use crate::style::AvailableSpace;
use crate::tree::{
    Baselines, CollapsibleMarginSet, LayoutInput, LayoutOutput, OofCandidates, OofPositioningArea, RunMode, SizingMode,
};
use crate::RequestedAxis;

#[cfg(all(feature = "alloc", not(feature = "std")))]
use alloc::boxed::Box;

/// The number of cache entries for each node in the tree
const CACHE_SIZE: usize = 9;

// Manually written-out results of float to u32 bit casts because
// `f32::to_bits` is not yet const at our MSRV.

/// `f32::INFINITY` as a u32
const INFINITY_BITS: u32 = 0b_0_11111111_00000000000000000000000_u32;
/// `f32::NEG_INFINITY` as a u32
const NEG_INFINITY_BITS: u32 = 0b_1_11111111_00000000000000000000000_u32;

// The `CacheKey` encodes two f32s as a u64. We know that the f32s will always be
// non-negative, so we pack two extra bits encoding the `RequestedAxis` into the
// sign bits of the f32s. These constants help to encode and decode those bits.

/// The sign bit of the first f32
const SIGN_BIT_1: u64 = 1u64 << 63;
/// The sign bit of the second f32
const SIGN_BIT_2: u64 = 1u64 << 31;
/// Mask of both sign bits (used to compute NON_SIGN_BITS_MASK)
const BOTH_SIGN_BITS_MASK: u64 = SIGN_BIT_1 | SIGN_BIT_2;
/// Mask of excluding the sign bits (used when setting/getting the size excluding the packed bits)
const NON_SIGN_BITS_MASK: u64 = !BOTH_SIGN_BITS_MASK;

/// Mask which includes only the bits which encode the x-axis value that we can use to ignore the
/// y-axis value when comparing a cache key.
const X_AXIS_VALUE_MASK: u64 = (u32::MAX as u64) << 32;

/// Pack `Option<f32>` into `u32`
#[inline(always)]
fn option_cache_key(input: Option<f32>) -> u32 {
    match input {
        Some(value) => value.to_bits(),
        None => INFINITY_BITS,
    }
}

/// Pack `Size<Option<f32>>` into `u64`
#[inline(always)]
fn size_option_cache_key(input: Size<Option<f32>>) -> u64 {
    (option_cache_key(input.width) as u64) << 32 | option_cache_key(input.height) as u64
}

/// Pack `AvailableSpace` into `u32`
#[inline(always)]
fn available_space_cache_key(input: AvailableSpace) -> u32 {
    match input {
        AvailableSpace::Definite(value) => (-value).to_bits(),
        AvailableSpace::MinContent => NEG_INFINITY_BITS,
        AvailableSpace::MaxContent => INFINITY_BITS,
    }
}

/// Pack `Size<AvailableSpace>` into `u64`
#[inline(always)]
#[allow(dead_code)]
fn size_available_space_cache_key(input: Size<AvailableSpace>) -> u64 {
    (available_space_cache_key(input.width) as u64) << 32 | available_space_cache_key(input.height) as u64
}

/// Encodes combination of a `known_dimension` (Option<f32>) and `AvailableSpace` in
/// a single dimension into a cache key in a single dimension.
#[inline(always)]
fn mixed_cache_key(kd: Option<f32>, avs: AvailableSpace) -> u32 {
    kd.map(|kd| kd.to_bits()).unwrap_or_else(|| available_space_cache_key(avs))
}

/// Encodes combination of a `known_dimension` (Option<f32>) and `AvailableSpace` in
/// two dimensions into a cache key in a single dimension.
#[inline(always)]
fn size_mixed_cache_key(kd: Size<Option<f32>>, avs: Size<AvailableSpace>) -> u64 {
    (mixed_cache_key(kd.width, avs.width) as u64) << 32 | mixed_cache_key(kd.height, avs.height) as u64
}

/// Space-optimised cache key that packs bits into as small a size as possible
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
// Four-byte alignment removes tail padding without narrowing either packed key.
#[repr(C, packed(4))]
struct CacheKey {
    /// The initial cached size of the node itself
    kd_available_space: u64,
    /// The initial cached size of the parent's node
    parent_size: u64,
    /// Whether each known dimension is definite. Normalized such that an axis
    /// without a known dimension is always `true`.
    known_dimensions_are_definite: Size<bool>,
}

impl CacheKey {
    #[inline(always)]
    #[allow(dead_code)]
    /// Return the parent size with the extra bits that encode the requested axis masked out
    fn parent_size(&self) -> u64 {
        self.parent_size & NON_SIGN_BITS_MASK
    }

    /// Return the parent size with the extra bits that encode the requested axis masked out
    /// And the y-axis value masked out
    fn x_axis_parent_size(&self) -> u64 {
        self.parent_size & (X_AXIS_VALUE_MASK & NON_SIGN_BITS_MASK)
    }

    /// Return the bits that encode the requested axis
    fn requested_axis_bits(&self) -> u64 {
        self.parent_size & BOTH_SIGN_BITS_MASK
    }

    /// Whether a cached entry with this key contains a valid size for the axis requested by `other`.
    /// Sizes computed for a single axis may contain garbage values in the other axis, so an entry
    /// is only usable if it was computed for the same axis (or for both axes).
    fn size_is_valid_for(&self, other: &CacheKey) -> bool {
        let entry_axis = self.requested_axis_bits();
        entry_axis == BOTH_SIGN_BITS_MASK || entry_axis == other.requested_axis_bits()
    }
}

impl From<&LayoutInput> for CacheKey {
    fn from(input: &LayoutInput) -> Self {
        // Pack axis enum into spare bits in the known_dimensions and available_space values
        let extra_bits = match input.axis {
            RequestedAxis::Horizontal => SIGN_BIT_1,
            RequestedAxis::Vertical => SIGN_BIT_2,
            RequestedAxis::Both => SIGN_BIT_1 | SIGN_BIT_2,
        };

        Self {
            kd_available_space: size_mixed_cache_key(input.known_dimensions, input.available_space),
            parent_size: (size_option_cache_key(input.parent_size) & NON_SIGN_BITS_MASK) | extra_bits,
            known_dimensions_are_definite: input
                .known_dimensions_are_definite
                .zip_map(input.known_dimensions, |is_definite, kd| is_definite || kd.is_none()),
        }
    }
}

/// The layout inputs that affect a node's layout but are not part of the [`CacheKey`]: the
/// [`SizingMode`] and whether each of the node's vertical margins is collapsible, packed into a `u8`.
///
/// These are determined by the layout algorithm of the node's parent and by the node's own style,
/// so they only change for a given node when one of those changes. Rather than keying every entry
/// on them, a [`Cache`] records the single mode that all of its entries were computed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
struct CacheMode(u8);

impl CacheMode {
    /// Set if the sizing mode is `SizingMode::InherentSize`
    const INHERENT_SIZE_BIT: u8 = 0b001;
    /// Set if the node's top margin is collapsible
    const COLLAPSIBLE_START_MARGIN_BIT: u8 = 0b010;
    /// Set if the node's bottom margin is collapsible
    const COLLAPSIBLE_END_MARGIN_BIT: u8 = 0b100;
}

impl From<&LayoutInput> for CacheMode {
    #[inline(always)]
    fn from(input: &LayoutInput) -> Self {
        let inherent_size = matches!(input.sizing_mode, SizingMode::InherentSize) as u8;
        let collapsible = input.vertical_margins_are_collapsible;
        Self(
            (inherent_size * Self::INHERENT_SIZE_BIT)
                | (collapsible.start as u8 * Self::COLLAPSIBLE_START_MARGIN_BIT)
                | (collapsible.end as u8 * Self::COLLAPSIBLE_END_MARGIN_BIT),
        )
    }
}

#[cfg(all(debug_assertions, feature = "std"))]
std::thread_local! {
    /// See [`cache_mode_change_evictions`]
    static MODE_CHANGE_EVICTIONS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

/// The number of times that the current thread has stored a result into a non-empty [`Cache`]
/// whose entries were computed with a different `sizing_mode` or `vertical_margins_are_collapsible`,
/// dropping those entries.
///
/// This is expected when the layout algorithm of a node's parent has changed since the node was
/// last laid out. It must not happen while laying out a tree whose caches started out empty: that
/// would mean that Taffy lays a node out with more than one mode in a single pass, with each
/// change of mode throwing away the node's cache. Taffy's test suite uses this function to check
/// that. Only available in debug builds.
#[doc(hidden)]
#[cfg(all(debug_assertions, feature = "std"))]
pub fn cache_mode_change_evictions() -> usize {
    MODE_CHANGE_EVICTIONS.with(|count| count.get())
}

/// Cached intermediate layout results
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub(crate) struct CacheEntry<T> {
    /// The key for the cache entry
    key: CacheKey,
    /// The cached size and baselines of the item
    content: T,
}

/// A [`LayoutOutput`] stored without its `Option` discriminants, which cost 4 bytes each.
/// Presence lives in `presence` bits rather than in a NaN sentinel, so `Some(NaN)` round-trips.
/// `margins_can_collapse_through` stays a `bool` to give `Option<CacheEntry<_>>` its niche.
#[derive(Debug, Clone)]
struct CachedLayout {
    /// See [`LayoutOutput::size`]
    size: Size<f32>,
    /// See [`LayoutOutput::scrollable_overflow_rect`]
    #[cfg(feature = "content_size")]
    scrollable_overflow_rect: Rect<f32>,
    /// [`Baselines::first`], or `0.0` when the `FIRST_BASELINE` bit is clear
    first_baseline: f32,
    /// [`Baselines::last`], or `0.0` when the `LAST_BASELINE` bit is clear
    last_baseline: f32,
    /// See [`LayoutOutput::top_margin`]
    top_margin: CollapsibleMarginSet,
    /// See [`LayoutOutput::bottom_margin`]
    bottom_margin: CollapsibleMarginSet,
    /// See [`LayoutOutput::oof_candidates`]
    oof_candidates: OofCandidates,
    /// [`LayoutOutput::oof_positioning_area`] when it differs from the area a container without
    /// border or scrollbar insets produces, which is `size` at the origin and needs no storage.
    /// Whether the output had one at all is the `OOF_AREA` bit, so `None` here is not "no area".
    oof_area: Option<Box<OofPositioningArea>>,
    /// See [`LayoutOutput::margins_can_collapse_through`]
    margins_can_collapse_through: bool,
    /// Which of the optional fields are present
    presence: u8,
}

impl CachedLayout {
    /// Set if `first_baseline` holds a baseline
    const FIRST_BASELINE: u8 = 0b001;
    /// Set if `last_baseline` holds a baseline
    const LAST_BASELINE: u8 = 0b010;
    /// Set if the output had a positioning area; the area is either boxed or rebuilt from `size`
    const OOF_AREA: u8 = 0b100;

    /// Return the optional value if `bit` is set in `presence`
    #[inline(always)]
    fn present<T>(&self, bit: u8, value: T) -> Option<T> {
        if self.presence & bit != 0 {
            Some(value)
        } else {
            None
        }
    }

    /// Rebuild the [`LayoutOutput`] that this entry was created from
    fn to_layout_output(&self) -> LayoutOutput {
        LayoutOutput {
            size: self.size,
            #[cfg(feature = "content_size")]
            scrollable_overflow_rect: self.scrollable_overflow_rect,
            baselines: Baselines {
                first: self.present(Self::FIRST_BASELINE, self.first_baseline),
                last: self.present(Self::LAST_BASELINE, self.last_baseline),
            },
            top_margin: self.top_margin,
            bottom_margin: self.bottom_margin,
            margins_can_collapse_through: self.margins_can_collapse_through,
            oof_candidates: self.oof_candidates.clone(),
            oof_positioning_area: self.oof_positioning_area(),
        }
    }

    /// [`LayoutOutput::oof_positioning_area`], rebuilt from `size` when it was the default area
    fn oof_positioning_area(&self) -> Option<OofPositioningArea> {
        if self.presence & Self::OOF_AREA == 0 {
            return None;
        }
        Some(
            self.oof_area
                .as_deref()
                .copied()
                .unwrap_or(OofPositioningArea { size: self.size, offset: Point::ZERO }),
        )
    }
}

/// Whether `area` is what a container with no border or scrollbar insets produces: the output's
/// own size at the origin. Compared by bit pattern so NaN and `-0.0` are never folded.
fn is_default_oof_area(area: &OofPositioningArea, size: Size<f32>) -> bool {
    area.size.width.to_bits() == size.width.to_bits()
        && area.size.height.to_bits() == size.height.to_bits()
        && area.offset.x.to_bits() == 0.0f32.to_bits()
        && area.offset.y.to_bits() == 0.0f32.to_bits()
}

/// Equal when the [`LayoutOutput`]s they were built from compare equal. The stored bits differ
/// from the output's `PartialEq` (an offset of `-0.0` is boxed, `0.0` is folded), so comparing
/// the fields directly would make a [`Cache`] unequal to a cache holding an equal output.
impl PartialEq for CachedLayout {
    fn eq(&self, other: &Self) -> bool {
        #[cfg(feature = "content_size")]
        let overflow_rects_match = self.scrollable_overflow_rect == other.scrollable_overflow_rect;
        #[cfg(not(feature = "content_size"))]
        let overflow_rects_match = true;

        self.size == other.size
            && self.present(Self::FIRST_BASELINE, self.first_baseline)
                == other.present(Self::FIRST_BASELINE, other.first_baseline)
            && self.present(Self::LAST_BASELINE, self.last_baseline)
                == other.present(Self::LAST_BASELINE, other.last_baseline)
            && self.top_margin == other.top_margin
            && self.bottom_margin == other.bottom_margin
            && self.margins_can_collapse_through == other.margins_can_collapse_through
            && self.oof_candidates == other.oof_candidates
            && self.oof_positioning_area() == other.oof_positioning_area()
            && overflow_rects_match
    }
}

impl From<LayoutOutput> for CachedLayout {
    fn from(output: LayoutOutput) -> Self {
        let presence = (output.baselines.first.is_some() as u8 * Self::FIRST_BASELINE)
            | (output.baselines.last.is_some() as u8 * Self::LAST_BASELINE)
            | (output.oof_positioning_area.is_some() as u8 * Self::OOF_AREA);
        // Only an area that differs from the default needs its own allocation; the rest rebuild
        // from `size` on a cache hit.
        let oof_area = output
            .oof_positioning_area
            .filter(|area| !is_default_oof_area(area, output.size))
            .map(Box::new);
        Self {
            size: output.size,
            #[cfg(feature = "content_size")]
            scrollable_overflow_rect: output.scrollable_overflow_rect,
            first_baseline: output.baselines.first.unwrap_or(0.0),
            last_baseline: output.baselines.last.unwrap_or(0.0),
            top_margin: output.top_margin,
            bottom_margin: output.bottom_margin,
            oof_candidates: output.oof_candidates,
            oof_area,
            margins_can_collapse_through: output.margins_can_collapse_through,
            presence,
        }
    }
}

/// The heap-allocated payload of a [`Cache`]. Boxed so an empty cache costs one
/// pointer inside every DOM node instead of the full measure-entry array.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
struct CacheInner {
    /// The cache entry for the node's final layout
    #[cfg_attr(feature = "serde", serde(serialize_with = "serialize_final_layout_entry"))]
    final_layout_entry: Option<CacheEntry<CachedLayout>>,
    /// The cache entries for the node's preliminary size measurements
    #[cfg_attr(feature = "serde", serde(serialize_with = "serialize_measure_entries"))]
    measure_entries: crate::util::sys::Vec<CacheEntry<Size<f32>>>,
    /// Tracks which measure entries have been used since the eviction cursor last passed them
    recently_used_entries: u16,
    /// The next measure entry to consider replacing
    next_measure_entry: u8,
    /// The `sizing_mode` and `vertical_margins_are_collapsible` inputs that all of the cache's
    /// entries were computed with. Results are only retrieved for inputs that have the same mode,
    /// and storing a result that was computed with a different mode drops the existing entries.
    mode: CacheMode,
}

impl CacheInner {
    fn new(mode: CacheMode) -> Self {
        Self {
            final_layout_entry: None,
            measure_entries: crate::util::sys::Vec::new(),
            recently_used_entries: 0,
            next_measure_entry: 0,
            mode,
        }
    }

    /// Store a measurement under `key`, replacing the entry that already has that key or, failing
    /// that, the slot the second-chance clock points at.
    fn store_measurement(&mut self, key: CacheKey, size: Size<f32>) {
        if let Some(index) = self.measure_entries.iter().position(|entry| entry.key == key) {
            self.measure_entries[index].content = size;
            self.recently_used_entries |= 1 << index;
            return;
        }

        // An entry read since the cursor last passed it gets one second chance: clear the used
        // bits up to the first slot that has not been read, which is the one to overwrite.
        while self.recently_used_entries & (1 << self.next_measure_entry) != 0 {
            self.recently_used_entries &= !(1 << self.next_measure_entry);
            self.advance_measure_cursor();
        }

        let entry_index = self.next_measure_entry as usize;
        let entry = CacheEntry { key, content: size };
        if entry_index == self.measure_entries.len() {
            self.grow_measure_entries();
            self.measure_entries.push(entry);
        } else {
            self.measure_entries[entry_index] = entry;
        }
        self.recently_used_entries |= 1 << entry_index;
        self.advance_measure_cursor();
    }

    /// Move to the next slot, wrapping at `CACHE_SIZE`
    fn advance_measure_cursor(&mut self) {
        self.next_measure_entry = (self.next_measure_entry + 1) % CACHE_SIZE as u8;
    }

    /// Reserve the slot the cursor is about to hand out.
    fn grow_measure_entries(&mut self) {
        // Slots fill in cursor order before the clock can wrap. Keep their indices stable:
        // lookup order and second chances depend on them.
        let len = self.measure_entries.len();
        if len < self.measure_entries.capacity() {
            return;
        }

        // Double while small, then grow by one: most caches that pass four entries settle at
        // five, which doubling would round up to eight.
        let target_capacity = (if len < 4 { (len * 2).max(1) } else { len + 1 }).min(CACHE_SIZE);
        self.measure_entries.reserve_exact(target_capacity - len);
    }
}

// Requests above 48 bytes round up to 8-byte classes, so a byte over 96 costs 8 more bytes on
// every node that caches a layout.
#[cfg(target_pointer_width = "32")]
const _: () = assert!(core::mem::size_of::<CacheInner>() <= 96);

/// Serialize the final entry with the shape of a `CacheEntry<LayoutOutput>`
#[cfg(feature = "serde")]
fn serialize_final_layout_entry<S: serde::Serializer>(
    entry: &Option<CacheEntry<CachedLayout>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::Serialize;
    entry
        .as_ref()
        .map(|entry| CacheEntry { key: entry.key, content: entry.content.to_layout_output() })
        .serialize(serializer)
}

#[cfg(feature = "serde")]
fn serialize_measure_entries<S: serde::Serializer>(
    entries: &[CacheEntry<Size<f32>>],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeTuple;
    let mut sequence = serializer.serialize_tuple(CACHE_SIZE)?;
    for index in 0..CACHE_SIZE {
        sequence.serialize_element(&entries.get(index))?;
    }
    sequence.end()
}

/// A cache for caching the results of a sizing a Grid Item or Flexbox Item
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct Cache {
    /// Lazily allocated entries; `None` until the first result is stored.
    inner: Option<Box<CacheInner>>,
}

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}

impl Cache {
    /// Create a new empty cache
    pub const fn new() -> Self {
        Self { inner: None }
    }

    /// Try to retrieve a cached result from the cache
    #[inline]
    pub fn get(&mut self, input: &LayoutInput) -> Option<LayoutOutput> {
        let inner = self.inner.as_deref_mut()?;
        if inner.mode != CacheMode::from(input) {
            return None;
        }
        let key = CacheKey::from(input);
        match input.run_mode {
            RunMode::PerformLayout => {
                inner.final_layout_entry.as_ref().filter(|entry| entry.key == key).map(|e| e.content.to_layout_output())
            }
            RunMode::ComputeSize => {
                for (index, entry) in inner.measure_entries.iter().enumerate() {
                    if entry.key.kd_available_space == key.kd_available_space
                        && entry.key.known_dimensions_are_definite == key.known_dimensions_are_definite
                        && (entry.key.x_axis_parent_size() == key.x_axis_parent_size())
                        && entry.key.size_is_valid_for(&key)
                    {
                        inner.recently_used_entries |= 1 << index;
                        return Some(LayoutOutput::from_outer_size(entry.content));
                    }
                }

                None
            }
            RunMode::PerformHiddenLayout => None,
        }
    }

    /// Store a computed size in the cache
    pub fn store(&mut self, input: &LayoutInput, layout_output: LayoutOutput) {
        if input.run_mode == RunMode::PerformHiddenLayout {
            return;
        }
        let mode = CacheMode::from(input);
        if self.inner.as_ref().is_some_and(|inner| inner.mode != mode) {
            // A live box is never empty, so every mode change here drops entries
            #[cfg(all(debug_assertions, feature = "std"))]
            MODE_CHANGE_EVICTIONS.with(|count| count.set(count.get() + 1));
            self.clear();
        }
        let key = CacheKey::from(input);
        match input.run_mode {
            RunMode::PerformLayout => {
                let inner = self.inner.get_or_insert_with(|| Box::new(CacheInner::new(mode)));
                inner.final_layout_entry = Some(CacheEntry { key, content: CachedLayout::from(layout_output) })
            }
            RunMode::ComputeSize => {
                // Measure entries only store the size, and cache hits are reconstructed with
                // `LayoutOutput::from_outer_size`, which resets the margin-collapse metadata
                // (`top_margin`, `bottom_margin`, `margins_can_collapse_through`). Results that
                // carry such metadata cannot be reconstructed from their size, so don't cache them.
                if layout_output.margins_can_collapse_through
                    || layout_output.top_margin != CollapsibleMarginSet::ZERO
                    || layout_output.bottom_margin != CollapsibleMarginSet::ZERO
                {
                    return;
                }
                let inner = self.inner.get_or_insert_with(|| Box::new(CacheInner::new(mode)));
                inner.store_measurement(key, layout_output.size);
            }
            RunMode::PerformHiddenLayout => {}
        }
    }

    /// Clear all cache entries and reports clear operation outcome ([`ClearState`])
    pub fn clear(&mut self) -> ClearState {
        match self.inner.take() {
            Some(_) => ClearState::Cleared,
            None => ClearState::AlreadyEmpty,
        }
    }

    /// Returns true if all cache entries are None, else false
    pub fn is_empty(&self) -> bool {
        self.inner.is_none()
    }
}

/// Clear operation outcome. See [`Cache::clear`]
pub enum ClearState {
    /// Cleared some values
    Cleared,
    /// Everything was already cleared
    AlreadyEmpty,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Line;
    use crate::tree::SizingMode;

    fn input(width: f32) -> LayoutInput {
        LayoutInput {
            run_mode: RunMode::ComputeSize,
            sizing_mode: SizingMode::InherentSize,
            axis: RequestedAxis::Both,
            known_dimensions: Size { width: Some(width), height: None },
            known_dimensions_are_definite: Size { width: true, height: true },
            parent_size: Size::NONE,
            available_space: Size { width: AvailableSpace::MaxContent, height: AvailableSpace::MaxContent },
            vertical_margins_are_collapsible: Line::FALSE,
        }
    }

    fn output(width: f32) -> LayoutOutput {
        LayoutOutput::from_outer_size(Size { width, height: width })
    }

    #[test]
    fn recently_used_measure_entries_get_a_second_chance() {
        let mut cache = Cache::new();
        for width in 0..CACHE_SIZE {
            cache.store(&input(width as f32), output(width as f32));
        }
        cache.store(&input(CACHE_SIZE as f32), output(CACHE_SIZE as f32));

        assert_eq!(cache.get(&input(1.0)), Some(output(1.0)));
        cache.store(&input((CACHE_SIZE + 1) as f32), output((CACHE_SIZE + 1) as f32));

        assert_eq!(cache.get(&input(1.0)), Some(output(1.0)));
        assert_eq!(cache.get(&input(2.0)), None);
    }

    #[test]
    fn storing_an_existing_measurement_updates_it_in_place() {
        let mut cache = Cache::new();
        cache.store(&input(1.0), output(1.0));
        cache.store(&input(2.0), output(2.0));
        cache.store(&input(1.0), output(3.0));

        assert_eq!(cache.inner.as_ref().unwrap().measure_entries.len(), 2);
        assert_eq!(cache.get(&input(1.0)), Some(output(3.0)));
    }

    #[test]
    #[cfg(feature = "serde")]
    fn unused_measure_entries_keep_the_serialized_array_shape() {
        let mut cache = Cache::new();
        cache.store(&perform_layout_input(1.0, 2.0), output(2.0));

        let serialized = serde_json::to_value(cache).unwrap();
        assert_eq!(
            serialized["inner"]["measure_entries"],
            serde_json::json!([null, null, null, null, null, null, null, null, null]),
        );
    }

    #[test]
    fn measurements_with_margin_collapse_metadata_are_not_cached() {
        let mut cache = Cache::new();

        let mut collapse_through = output(1.0);
        collapse_through.margins_can_collapse_through = true;
        cache.store(&input(1.0), collapse_through);
        assert_eq!(cache.get(&input(1.0)), None);

        let mut carried_margin = output(2.0);
        carried_margin.top_margin = CollapsibleMarginSet::from_margin(10.0);
        cache.store(&input(2.0), carried_margin);
        assert_eq!(cache.get(&input(2.0)), None);
    }

    #[test]
    fn retrieving_a_measurement_only_marks_its_slot_as_used() {
        let mut cache = Cache::new();
        cache.store(&input(1.0), output(1.0));
        cache.store(&input(2.0), output(2.0));
        cache.inner.as_mut().unwrap().recently_used_entries = 0;
        let entries = cache.inner.as_ref().unwrap().measure_entries.clone();

        assert_eq!(cache.get(&input(1.0)), Some(output(1.0)));
        assert_eq!(cache.inner.as_ref().unwrap().measure_entries, entries);
        assert_ne!(cache.inner.as_ref().unwrap().recently_used_entries, 0);
    }

    fn perform_layout_input(width: f32, height: f32) -> LayoutInput {
        LayoutInput {
            run_mode: RunMode::PerformLayout,
            known_dimensions: Size { width: Some(width), height: Some(height) },
            parent_size: Size { width: Some(width), height: Some(height) },
            ..input(width)
        }
    }

    /// Inputs that differ from `input`/`perform_layout_input` only in their mode
    fn other_modes(input: LayoutInput) -> [LayoutInput; 4] {
        [
            LayoutInput { sizing_mode: SizingMode::ContentSize, ..input },
            LayoutInput { vertical_margins_are_collapsible: Line::TRUE, ..input },
            LayoutInput { vertical_margins_are_collapsible: Line { start: true, end: false }, ..input },
            LayoutInput { vertical_margins_are_collapsible: Line { start: false, end: true }, ..input },
        ]
    }

    #[test]
    fn cache_mode_distinguishes_every_combination_of_inputs() {
        let mut modes = [CacheMode(0); 8];
        for (index, mode) in modes.iter_mut().enumerate() {
            *mode = CacheMode::from(&LayoutInput {
                sizing_mode: if index & 1 == 0 { SizingMode::ContentSize } else { SizingMode::InherentSize },
                vertical_margins_are_collapsible: Line { start: index & 2 != 0, end: index & 4 != 0 },
                ..input(1.0)
            });
        }
        for (index, mode) in modes.iter().enumerate() {
            assert!(!modes[..index].contains(mode));
        }
    }

    #[test]
    fn retrieving_with_a_different_mode_misses_and_keeps_the_entries() {
        let mut cache = Cache::new();
        cache.store(&perform_layout_input(100.0, 50.0), output(1.0));
        cache.store(&input(1.0), output(2.0));

        for other in other_modes(perform_layout_input(100.0, 50.0)) {
            assert_eq!(cache.get(&other), None);
        }
        for other in other_modes(input(1.0)) {
            assert_eq!(cache.get(&other), None);
        }

        assert_eq!(cache.get(&perform_layout_input(100.0, 50.0)), Some(output(1.0)));
        assert_eq!(cache.get(&input(1.0)), Some(output(2.0)));
    }

    #[test]
    fn storing_with_a_different_mode_drops_the_existing_entries() {
        for other in other_modes(input(3.0)) {
            let mut cache = Cache::new();
            cache.store(&perform_layout_input(100.0, 50.0), output(1.0));
            cache.store(&input(1.0), output(2.0));

            cache.store(&other, output(3.0));
            assert_eq!(cache.get(&other), Some(output(3.0)));
            assert!(cache.inner.as_ref().unwrap().final_layout_entry.is_none());
            assert_eq!(cache.inner.as_ref().unwrap().measure_entries.len(), 1);

            // The old entries are gone, even for inputs that have the mode that the cache had before
            cache.store(&input(4.0), output(4.0));
            assert_eq!(cache.get(&perform_layout_input(100.0, 50.0)), None);
            assert_eq!(cache.get(&input(1.0)), None);
            assert_eq!(cache.get(&other), None);
            assert_eq!(cache.get(&input(4.0)), Some(output(4.0)));
        }

        for other in other_modes(perform_layout_input(100.0, 50.0)) {
            let mut cache = Cache::new();
            cache.store(&perform_layout_input(100.0, 50.0), output(1.0));
            cache.store(&input(1.0), output(2.0));

            cache.store(&other, output(3.0));
            assert_eq!(cache.get(&other), Some(output(3.0)));
            assert!(cache.inner.as_ref().unwrap().measure_entries.is_empty());
            assert_eq!(cache.get(&perform_layout_input(100.0, 50.0)), None);
        }
    }

    #[test]
    fn hidden_layouts_do_not_affect_the_cache() {
        let mut cache = Cache::new();
        cache.store(&input(1.0), output(1.0));
        cache.store(&LayoutInput::HIDDEN, output(2.0));
        assert_eq!(cache.get(&input(1.0)), Some(output(1.0)));
    }

    #[test]
    fn growing_measurements_keep_first_matching_slot_and_clock_order() {
        let mut cache = Cache::new();
        let both = input(1.0);
        let horizontal = LayoutInput { axis: RequestedAxis::Horizontal, ..both };
        cache.store(&horizontal, output(11.0));
        cache.store(&both, output(12.0));
        for width in 2..CACHE_SIZE {
            cache.store(&input(width as f32), output(width as f32));
        }
        assert_eq!(cache.get(&horizontal), Some(output(11.0)));
        assert_eq!(cache.get(&both), Some(output(12.0)));
        cache.store(&input(20.0), output(20.0));
        assert_eq!(cache.get(&horizontal), Some(output(12.0)));
        assert_eq!(cache.get(&both), Some(output(12.0)));
        cache.clear();
        assert_eq!(cache.get(&horizontal), None);
        cache.store(&both, output(13.0));
        assert_eq!(cache.get(&horizontal), Some(output(13.0)));
    }

    fn round_trip(layout: LayoutOutput) -> LayoutOutput {
        CachedLayout::from(layout).to_layout_output()
    }

    fn baseline_bits(baselines: Baselines) -> (Option<u32>, Option<u32>) {
        (baselines.first.map(f32::to_bits), baselines.last.map(f32::to_bits))
    }

    #[test]
    fn final_layouts_without_optional_fields_round_trip() {
        let layout = output(3.0);
        assert_eq!(layout.baselines, Baselines::NONE);
        assert_eq!(layout.oof_positioning_area, None);
        assert_eq!(round_trip(layout.clone()), layout);
    }

    #[test]
    fn final_layout_baselines_round_trip_bit_for_bit() {
        for (first, last) in [
            (Some(f32::NAN), None),
            (None, Some(f32::NAN)),
            (Some(-0.0), Some(0.0)),
            (Some(0.0), None),
            (None, Some(-0.0)),
            (Some(4.5), Some(f32::NAN)),
        ] {
            let mut layout = output(3.0);
            layout.baselines = Baselines { first, last };
            let restored = round_trip(layout.clone());
            assert_eq!(baseline_bits(restored.baselines), baseline_bits(layout.baselines));
            layout.baselines = Baselines::NONE;
            assert_eq!(LayoutOutput { baselines: Baselines::NONE, ..restored }, layout);
        }
    }

    #[test]
    fn final_layout_positioning_areas_and_margins_round_trip() {
        let mut layout = output(3.0);
        layout.oof_positioning_area = Some(OofPositioningArea { size: Size::ZERO, offset: Point::ZERO });
        assert_eq!(round_trip(layout.clone()), layout);

        layout.oof_positioning_area =
            Some(OofPositioningArea { size: Size { width: 7.0, height: 8.0 }, offset: Point { x: -1.0, y: 2.5 } });
        layout.top_margin = CollapsibleMarginSet::from_margin(10.0);
        layout.bottom_margin = CollapsibleMarginSet::from_margin(-4.0);
        layout.margins_can_collapse_through = true;
        assert_eq!(round_trip(layout.clone()), layout);
    }

    #[test]
    fn final_layout_oof_candidates_round_trip() {
        use crate::style::Position;
        use crate::tree::{AxisStaticEdge, AxisStaticPosition, NodeId, OofCandidate};
        let mut layout = output(3.0);
        for (node, position) in [(1u64, Position::Absolute), (2u64, Position::Fixed)] {
            layout.oof_candidates.push(OofCandidate {
                node: NodeId::from(node),
                order: node as u32,
                position,
                static_position: Point {
                    x: AxisStaticPosition::from_edge(node as f32, AxisStaticEdge::Start),
                    y: AxisStaticPosition::from_edge(-(node as f32), AxisStaticEdge::End),
                },
            });
        }
        let restored = round_trip(layout.clone());
        assert_eq!(restored.oof_candidates.as_slice().len(), 2);
        assert_eq!(restored, layout);
    }

    #[test]
    fn the_default_positioning_area_needs_no_allocation() {
        let mut layout = output(3.0);
        layout.oof_positioning_area = Some(OofPositioningArea { size: layout.size, offset: Point::ZERO });
        let cached = CachedLayout::from(layout.clone());
        assert!(cached.oof_area.is_none(), "the default area rebuilds from size");
        assert_eq!(cached.to_layout_output(), layout);
    }

    #[test]
    fn a_negative_zero_offset_is_not_folded_into_the_default_area() {
        let mut layout = output(3.0);
        layout.oof_positioning_area =
            Some(OofPositioningArea { size: layout.size, offset: Point { x: -0.0, y: 0.0 } });
        assert!(CachedLayout::from(layout.clone()).oof_area.is_some());
        let area = round_trip(layout).oof_positioning_area.expect("area present");
        assert_eq!(area.offset.x.to_bits(), (-0.0f32).to_bits());
        assert_eq!(area.offset.y.to_bits(), 0.0f32.to_bits());
    }

    #[test]
    fn a_nan_positioning_area_round_trips_by_bits() {
        let mut layout = output(3.0);
        layout.oof_positioning_area =
            Some(OofPositioningArea { size: Size { width: f32::NAN, height: 3.0 }, offset: Point::ZERO });
        let area = round_trip(layout).oof_positioning_area.expect("area present");
        assert_eq!(area.size.width.to_bits(), f32::NAN.to_bits());
        assert_eq!(area.size.height.to_bits(), 3.0f32.to_bits());
    }

    #[test]
    fn equal_layout_outputs_make_equal_caches() {
        // An offset of `-0.0` is boxed and `0.0` is folded, so the stored bits differ even though
        // the outputs compare equal; `Cache` equality must still agree with `LayoutOutput`'s.
        let mut signed_zero = output(3.0);
        signed_zero.oof_positioning_area =
            Some(OofPositioningArea { size: signed_zero.size, offset: Point { x: -0.0, y: 0.0 } });
        let mut positive_zero = output(3.0);
        positive_zero.oof_positioning_area = Some(OofPositioningArea { size: positive_zero.size, offset: Point::ZERO });
        assert_eq!(signed_zero, positive_zero);
        let store = |layout: LayoutOutput| {
            let mut cache = Cache::new();
            cache.store(&perform_layout_input(100.0, 50.0), layout);
            cache
        };
        assert_eq!(store(signed_zero), store(positive_zero));
    }

    #[test]
    fn stored_final_layouts_are_returned_unchanged() {
        let mut layout = output(3.0);
        layout.baselines = Baselines { first: Some(1.0), last: None };
        layout.oof_positioning_area =
            Some(OofPositioningArea { size: Size { width: 3.0, height: 3.0 }, offset: Point::ZERO });
        layout.top_margin = CollapsibleMarginSet::from_margin(2.0);
        let mut cache = Cache::new();
        cache.store(&perform_layout_input(100.0, 50.0), layout.clone());
        assert_eq!(cache.get(&perform_layout_input(100.0, 50.0)), Some(layout.clone()));
        assert_eq!(cache.get(&perform_layout_input(100.0, 50.0)), Some(layout));
    }

    #[test]
    fn measure_slots_double_to_four_then_grow_by_one() {
        let mut cache = Cache::new();
        let mut capacities = crate::util::sys::Vec::new();
        for width in 0..CACHE_SIZE {
            cache.store(&input(width as f32), output(width as f32));
            capacities.push(cache.inner.as_ref().unwrap().measure_entries.capacity());
        }
        assert_eq!(capacities, [1, 2, 4, 4, 5, 6, 7, 8, 9]);
        for width in 0..CACHE_SIZE {
            assert_eq!(cache.get(&input(width as f32)), Some(output(width as f32)));
        }
        let entries = &cache.inner.as_ref().unwrap().measure_entries;
        for (index, entry) in entries.iter().enumerate() {
            assert_eq!(entry.content, output(index as f32).size);
        }
    }

    #[test]
    #[cfg(all(debug_assertions, feature = "std"))]
    fn mode_change_evictions_are_counted_for_non_empty_caches_only() {
        let content_size = LayoutInput { sizing_mode: SizingMode::ContentSize, ..input(1.0) };
        let evictions = cache_mode_change_evictions();

        let mut cache = Cache::new();
        cache.store(&content_size, output(1.0));
        cache.store(&LayoutInput { sizing_mode: SizingMode::ContentSize, ..input(2.0) }, output(2.0));
        assert_eq!(cache_mode_change_evictions(), evictions);

        cache.store(&input(1.0), output(1.0));
        assert_eq!(cache_mode_change_evictions(), evictions + 1);

        cache.clear();
        cache.store(&content_size, output(1.0));
        assert_eq!(cache_mode_change_evictions(), evictions + 1);
    }
}
