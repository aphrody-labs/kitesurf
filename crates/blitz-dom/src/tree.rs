//! Versioned storage for the nodes of the DOM tree.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::mem::MaybeUninit;
use std::ops::{Index, IndexMut};
use std::sync::atomic::{AtomicU64, Ordering};

use blitz_traits::node_id::NodeId;

use crate::Node;

/// Set in a `slot_versions` entry whose slot is free, on top of the slot's last version.
const FREE: u32 = 0x8000_0000;

/// The versioned slab in which the nodes of the DOM tree are stored. Nodes are addressed by
/// [`NodeId`], which carries the slot's version in addition to its index: when a node is
/// dropped and its slot reused, ids referring to the dropped node no longer resolve
/// ([`NodeTree::get`] returns `None`, and indexing panics) instead of aliasing the new occupant.
pub struct NodeTree {
    /// Node storage in fixed chunks, so growing never copies nodes or reserves past the next
    /// chunk. A slot is initialized exactly where its version lacks `FREE`. Slot 0 is a
    /// permanently free sentinel, so the first node lives in slot 1 as it did under slotmap;
    /// kitesurf-core's handle table relies on it.
    chunks: Vec<Box<[MaybeUninit<Node>; CHUNK_SLOTS]>>,
    /// Each slot's current version (the high half of its raw `NodeId`), or its last version
    /// with `FREE` set for a free slot; never 0. One box per chunk, apart from the nodes, so
    /// `version_at` reads none of a chunk's node memory. Backs `id_from_opaque`.
    versions: Vec<Box<[u32; CHUNK_SLOTS]>>,
    /// Slots handed out so far; every slot past it is free and has never held a node.
    used: u32,
    /// Free slot indexes, reused last-freed first. Each reuse bumps the slot's 31-bit version.
    free: Vec<u32>,
    /// The number of live nodes.
    live: usize,
    /// Bumped whenever box geometry may have moved (layout ran or a scroll
    /// offset changed). Values derived from `final_layout()` (e.g. hoisted
    /// paint child offsets) are memoised against it. Starts at 1 so that a
    /// stamp of 0 is never current.
    geometry_generation: AtomicU64,
    /// Scroll offset of the viewport (the root element scrolls the viewport,
    /// so its offset lives here rather than on the node).
    viewport_scroll: crate::Point<f64>,
    /// The page scroll fixed boxes stay anchored at while a clipped
    /// paint moves `viewport_scroll` to translate the scene. `None` means the
    /// fixed boxes follow `viewport_scroll` as usual.
    fixed_anchor: Option<crate::Point<f64>>,
    /// The owning document's shared style lock, reached by nodes through their tree pointer.
    pub(crate) guard: style::shared_lock::SharedRwLock,
    /// What every evicted inline root rebuilds with, kept once here rather
    /// than in each root's eviction state.
    pub(crate) inline_rebuild: crate::node::InlineRebuild,
    /// Layout parent slot by node slot, for the few nodes whose layout parent is neither none
    /// nor their DOM parent (anonymous-block children and the like); see `Node::layout_parent_id`.
    layout_parents: RefCell<LayoutParents>,
}

const CHUNK_SHIFT: u32 = 10;
const CHUNK_SLOTS: usize = 1 << CHUNK_SHIFT;

/// A chunk of uninitialized node slots, allocated in place: it is too large for the stack.
fn new_chunk() -> Box<[MaybeUninit<Node>; CHUNK_SLOTS]> {
    // SAFETY: an array of `MaybeUninit` needs no initialization.
    unsafe { Box::<[MaybeUninit<Node>; CHUNK_SLOTS]>::new_uninit().assume_init() }
}

#[inline(always)]
fn split(index: usize) -> (usize, usize) {
    (index >> CHUNK_SHIFT, index & (CHUNK_SLOTS - 1))
}

pub(crate) type LayoutParents = HashMap<u32, u32, BuildHasherDefault<SlotHasher>>;

/// Hashes a slot index with one multiply; slot indexes are dense and need no DoS resistance.
#[derive(Default)]
pub(crate) struct SlotHasher(u64);

impl Hasher for SlotHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 << 8 | u64::from(byte)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.0 = u64::from(value).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

/// The tree's layout parent side table, reached without a `&NodeTree`. Safety: as for
/// `version_at`; the table is outside the node slots, and nodes borrow it only briefly.
#[inline(always)]
pub(crate) unsafe fn layout_parents<'a>(tree: *const NodeTree) -> &'a RefCell<LayoutParents> {
    // SAFETY: the caller guarantees `tree` is live; `addr_of!` avoids a whole-tree reference.
    unsafe { &*std::ptr::addr_of!((*tree).layout_parents) }
}

/// The slot index of a raw id: its low half, with the version in the high half.
#[inline(always)]
fn slot_index(raw: u64) -> usize {
    (raw & 0xffff_ffff) as usize
}

/// The live version in a `slot_versions` entry, 0 if the slot is free.
#[inline(always)]
fn live_version(entry: u32) -> u32 {
    if entry & FREE == 0 { entry } else { 0 }
}

/// The version a free slot's next occupant gets: one past the last, within 31 bits, never 0.
#[inline(always)]
fn next_version(free_entry: u32) -> u32 {
    ((free_entry & !FREE).wrapping_add(1) & !FREE).max(1)
}

#[inline(always)]
pub(crate) fn id_from_parts(index: u32, version: u32) -> NodeId {
    NodeId::from_u64(u64::from(version) << 32 | u64::from(index))
}

/// Slot `index`'s version (0 if free), read without a `&NodeTree` so it is
/// usable while a `&mut Node` of the same tree is live. Safety: `tree` must be
/// live with no version being written. A node's tree pointer is: the boxed
/// tree outlives its nodes, and versions are written only through `&mut NodeTree`.
#[inline(always)]
pub(crate) unsafe fn version_at(tree: *const NodeTree, index: u32) -> u32 {
    // SAFETY: the caller guarantees `tree` is live; `addr_of!` projects to
    // the field without creating a reference to the whole tree.
    let versions = unsafe { &*std::ptr::addr_of!((*tree).versions) };
    let (chunk, offset) = split(index as usize);
    versions.get(chunk).map_or(0, |chunk| live_version(chunk[offset]))
}

impl NodeTree {
    pub(crate) fn new(
        guard: style::shared_lock::SharedRwLock,
        inline_rebuild: crate::node::InlineRebuild,
    ) -> Self {
        Self {
            chunks: Vec::new(),
            versions: Vec::new(),
            // Slot 0 is the free sentinel: never handed out, its version always reads `FREE`.
            used: 1,
            free: Vec::new(),
            live: 0,
            geometry_generation: AtomicU64::new(1),
            viewport_scroll: crate::Point::ZERO,
            fixed_anchor: None,
            guard,
            inline_rebuild,
            layout_parents: RefCell::default(),
        }
    }

    /// The live node behind a stylo `OpaqueNode` (`BlitzNode::opaque`).
    ///
    /// `OpaqueNode` wraps a `usize`, so on wasm32 it keeps only the slot
    /// index and loses the version. `NodeId::from_u64(opaque)` then rebuilt
    /// the key at version 1, which misses every node living in a reused slot.
    /// Stylo's animation sweep took that miss for "not in the document" and
    /// cleared the node's transitions on every resolve, so they never
    /// advanced: a span that lands in the slot a previous innerHTML's
    /// fragment root freed stayed at its start opacity. The chunks map each slot index
    /// to its current version, so the lookup works whatever the pointer width.
    pub fn id_from_opaque(&self, opaque: usize) -> Option<NodeId> {
        let raw = opaque as u64;
        let index = slot_index(raw);
        let version = live_version(self.entry(index)?);
        if version == 0 {
            return None;
        }
        let id = NodeId::from_u64(u64::from(version) << 32 | index as u64);
        // A 64-bit opaque still has its version: reject a stale one.
        let stale = raw >> 32 != 0 && raw != id.as_u64();
        (!stale && self.contains_key(id)).then_some(id)
    }

    /// The id of the node living in slot `index`. Nodes store their tree
    /// links as bare slot indexes and rebuild full ids through this.
    #[inline]
    pub fn id_at(&self, index: u32) -> NodeId {
        // SAFETY: `self` is a live tree, so the pointer is valid for the read.
        let version = unsafe { version_at(self, index) };
        debug_assert_ne!(version, 0, "slot {index} holds no live node");
        id_from_parts(index, version)
    }

    pub fn viewport_scroll(&self) -> crate::Point<f64> {
        self.viewport_scroll
    }

    /// Memoised paint geometry may depend on the viewport scroll, so this
    /// bumps the geometry generation like any other scroll write.
    pub(crate) fn set_viewport_scroll(&mut self, scroll: crate::Point<f64>) {
        self.viewport_scroll = scroll;
        self.fixed_anchor = None;
        self.bump_geometry_generation();
    }

    /// The scroll that viewport-fixed boxes cancel when painted:
    /// the anchor set by `set_viewport_scroll_with_fixed_anchor`, else the
    /// viewport scroll.
    pub fn fixed_anchor_scroll(&self) -> crate::Point<f64> {
        self.fixed_anchor.unwrap_or(self.viewport_scroll)
    }

    pub(crate) fn set_viewport_scroll_with_fixed_anchor(
        &mut self,
        scroll: crate::Point<f64>,
        anchor: crate::Point<f64>,
    ) {
        self.viewport_scroll = scroll;
        self.fixed_anchor = Some(anchor);
        self.bump_geometry_generation();
    }

    /// See the `geometry_generation` field.
    pub fn geometry_generation(&self) -> u64 {
        self.geometry_generation.load(Ordering::Relaxed)
    }

    pub(crate) fn bump_geometry_generation(&self) {
        self.geometry_generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Slot `index`'s versions entry; `None` past the last chunk.
    #[inline(always)]
    fn entry(&self, index: usize) -> Option<u32> {
        let (chunk, offset) = split(index);
        Some(self.versions.get(chunk)?[offset])
    }

    #[inline(always)]
    fn entry_mut(&mut self, index: usize) -> &mut u32 {
        let (chunk, offset) = split(index);
        &mut self.versions[chunk][offset]
    }

    #[inline(always)]
    fn slot_mut(&mut self, index: usize) -> &mut MaybeUninit<Node> {
        let (chunk, offset) = split(index);
        &mut self.chunks[chunk][offset]
    }

    /// The number of live nodes in the tree.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// The slot holding the live node `id`, if any.
    #[inline(always)]
    fn live_slot(&self, id: NodeId) -> Option<usize> {
        let raw = id.as_u64();
        let index = slot_index(raw);
        let version = (raw >> 32) as u32;
        // A live entry never has `FREE` set, so a forged `FREE` version cannot match a free slot.
        (version & FREE == 0 && self.entry(index) == Some(version)).then_some(index)
    }

    /// Whether `id` resolves to a live node.
    pub fn contains_key(&self, id: NodeId) -> bool {
        self.live_slot(id).is_some()
    }

    /// Get a reference to the node with the given id, if it is still live.
    #[inline]
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        let (chunk, offset) = split(self.live_slot(id)?);
        // SAFETY: `live_slot` found a live entry, so its chunk exists and the slot is
        // initialized.
        Some(unsafe { self.chunks.get_unchecked(chunk)[offset].assume_init_ref() })
    }

    /// Get a mutable reference to the node with the given id, if it is still live.
    #[inline]
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        let (chunk, offset) = split(self.live_slot(id)?);
        // SAFETY: as in `get`.
        Some(unsafe { self.chunks.get_unchecked_mut(chunk)[offset].assume_init_mut() })
    }

    /// Insert a node constructed with knowledge of its own id.
    pub(crate) fn insert_with_key(&mut self, f: impl FnOnce(NodeId) -> Node) -> NodeId {
        // Commit the slot only after `f` returns, so a panicking `f` leaves the tree unchanged.
        let reused = self.free.last().copied();
        let (index, version) = match reused {
            Some(index) => (index, next_version(*self.entry_mut(index as usize))),
            // A node keeps its slot index in the low bits of its flags word.
            None if self.used <= crate::node::SLOT_INDEX_MASK => (self.used, 1),
            None => panic!("node slot space exhausted"),
        };
        let id = id_from_parts(index, version);
        let node = f(id);
        if reused.is_some() {
            self.free.pop();
        } else {
            if split(index as usize).0 == self.chunks.len() {
                self.chunks.push(new_chunk());
                self.versions.push(Box::new([FREE; CHUNK_SLOTS]));
            }
            self.used += 1;
        }
        self.slot_mut(index as usize).write(node);
        *self.entry_mut(index as usize) = version;
        self.live += 1;
        id
    }

    /// Remove the node with the given id, returning it if it was still live.
    pub(crate) fn remove(&mut self, id: NodeId) -> Option<Node> {
        let index = self.live_slot(id)?;
        *self.entry_mut(index) |= FREE;
        self.free.push(index as u32);
        self.live -= 1;
        // SAFETY: the slot was live, so it is initialized; it is now marked free, so it is
        // never read or dropped again before `insert_with_key` overwrites it.
        let mut node = unsafe { self.slot_mut(index).assume_init_read() };
        if node.forget_side_table_layout_parent() {
            // The slot's next node must not inherit the entry; the removed node reads none.
            self.layout_parents.get_mut().remove(&(index as u32));
        }
        Some(node)
    }

    /// Iterate over all live `(NodeId, &Node)` pairs, in ascending slot order.
    pub fn iter(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        let versions = self.versions.iter().flat_map(|chunk| chunk.iter());
        versions
            .zip(self.chunks.iter().flat_map(|chunk| chunk.iter()))
            .enumerate()
            .filter(|(_, (version, _))| **version & FREE == 0)
            .map(|(index, (&version, slot))| {
                // SAFETY: the filter kept only slots whose entry lacks `FREE`: initialized ones.
                let node = unsafe { slot.assume_init_ref() };
                (id_from_parts(index as u32, version), node)
            })
    }

    /// Iterate over all live `(NodeId, &mut Node)` pairs, in ascending slot order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (NodeId, &mut Node)> {
        let versions = self.versions.iter().flat_map(|chunk| chunk.iter());
        versions
            .zip(self.chunks.iter_mut().flat_map(|chunk| chunk.iter_mut()))
            .enumerate()
            .filter(|(_, (version, _))| **version & FREE == 0)
            .map(|(index, (&version, slot))| {
                // SAFETY: the filter kept only slots whose entry lacks `FREE`: initialized ones.
                let node = unsafe { slot.assume_init_mut() };
                (id_from_parts(index as u32, version), node)
            })
    }
}

impl Drop for NodeTree {
    fn drop(&mut self) {
        // Like the slotmap this replaced, nodes drop while every version still reads as live.
        for (versions, chunk) in self.versions.iter().zip(&mut self.chunks) {
            for (version, slot) in versions.iter().zip(chunk.iter_mut()) {
                if version & FREE == 0 {
                    // SAFETY: the slot is live, hence initialized, and the tree is going away, so
                    // nothing reads or drops it afterwards.
                    unsafe { slot.assume_init_drop() };
                }
            }
        }
    }
}

impl Index<NodeId> for NodeTree {
    type Output = Node;

    #[track_caller]
    #[inline]
    fn index(&self, id: NodeId) -> &Node {
        match self.get(id) {
            Some(node) => node,
            None => panic!("invalid NodeId {id:?}: no live node"),
        }
    }
}

impl IndexMut<NodeId> for NodeTree {
    #[track_caller]
    #[inline]
    fn index_mut(&mut self, id: NodeId) -> &mut Node {
        match self.get_mut(id) {
            Some(node) => node,
            None => panic!("invalid NodeId {id:?}: no live node"),
        }
    }
}

#[cfg(test)]
mod tests {
    use markup5ever::{QualName, local_name, ns};

    fn span(doc: &mut crate::BaseDocument) -> blitz_traits::node_id::NodeId {
        let name = QualName::new(None, ns!(html), local_name!("span"));
        doc.mutate().create_element(name, Vec::new())
    }

    #[test]
    fn opaque_ids_follow_slot_reuse_and_reject_free_slots() {
        let mut doc = crate::BaseDocument::new(Default::default());
        let first = span(&mut doc);
        let index = (first.as_u64() & 0xffff_ffff) as usize;
        assert_eq!(doc.nodes.id_from_opaque(index), Some(first));
        doc.mutate().remove_and_drop_node(first);
        assert_eq!(doc.nodes.id_from_opaque(index), None);
        let second = span(&mut doc);
        assert_eq!(second.as_u64() & 0xffff_ffff, index as u64);
        assert_ne!(second, first);
        assert_eq!(doc.nodes.id_from_opaque(index), Some(second));
        assert_eq!(doc.nodes.id_from_opaque(first.as_u64() as usize), None);
    }

    #[test]
    fn nodes_across_chunk_boundaries_resolve_iterate_and_reuse_slots() {
        let mut doc = crate::BaseDocument::new(Default::default());
        let ids: Vec<_> = (0..3 * super::CHUNK_SLOTS).map(|_| span(&mut doc)).collect();
        let before = doc.nodes.len();
        for &id in &ids {
            assert_eq!(doc.nodes[id].id(), id);
            assert_eq!(doc.nodes.id_at(id.as_u64() as u32), id);
        }
        let walked: Vec<_> = doc.nodes.iter().map(|(id, _)| id).collect();
        assert!(walked.windows(2).all(|w| (w[0].as_u64() as u32) < (w[1].as_u64() as u32)));
        assert!(ids.iter().all(|id| walked.contains(id)));
        let gone = ids[super::CHUNK_SLOTS];
        doc.mutate().remove_and_drop_node(gone);
        assert_eq!(doc.nodes.len(), before - 1);
        assert!(doc.nodes.get(gone).is_none());
        let reused = span(&mut doc);
        assert_eq!(reused.as_u64() as u32, gone.as_u64() as u32);
        assert_eq!(doc.nodes.iter_mut().filter(|(id, _)| *id == reused).count(), 1);
        let past_end = (4 * super::CHUNK_SLOTS) as u32;
        assert_eq!(unsafe { super::version_at(&*doc.nodes, past_end) }, 0);
    }

    fn version(id: blitz_traits::node_id::NodeId) -> u32 {
        (id.as_u64() >> 32) as u32
    }

    #[test]
    fn reinsert_reuses_the_slot_and_the_stale_id_never_resolves() {
        let mut doc = crate::BaseDocument::new(Default::default());
        let live_before = doc.nodes.len();
        let first = span(&mut doc);
        assert_eq!(doc.nodes.len(), live_before + 1);
        assert_eq!(doc.nodes[first].id(), first);
        doc.mutate().remove_and_drop_node(first);
        assert_eq!(doc.nodes.len(), live_before);

        let second = span(&mut doc);
        assert_eq!(second.as_u64() as u32, first.as_u64() as u32, "slot reused");
        assert!(version(second) > version(first));
        assert_eq!(doc.nodes[second].id(), second);
        assert!(doc.nodes.get(first).is_none());
        assert!(doc.nodes.get_mut(first).is_none());
        assert!(!doc.nodes.contains_key(first));
        assert!(doc.nodes.iter().all(|(id, _)| id != first));
        let index_stale = std::panic::AssertUnwindSafe(|| doc.nodes[first].id());
        assert!(std::panic::catch_unwind(index_stale).is_err());
        if usize::BITS == 64 {
            assert_eq!(doc.nodes.id_from_opaque(first.as_u64() as usize), None);
        }
    }

    #[test]
    fn iteration_is_ascending_and_skips_free_slots() {
        let mut doc = crate::BaseDocument::new(Default::default());
        let [a, b, c] = [span(&mut doc), span(&mut doc), span(&mut doc)];
        doc.mutate().remove_and_drop_node(b);

        let ids: Vec<_> = doc.nodes.iter().map(|(id, _)| id).collect();
        let indexes: Vec<u32> = ids.iter().map(|id| id.as_u64() as u32).collect();
        assert!(indexes.windows(2).all(|pair| pair[0] < pair[1]), "{indexes:?}");
        assert!(ids.contains(&a) && ids.contains(&c) && !ids.contains(&b));
        assert_eq!(ids.len(), doc.nodes.len());
        assert!(doc.nodes.iter().all(|(id, node)| node.id() == id));
        let mut_ids: Vec<_> = doc.nodes.iter_mut().map(|(id, _)| id).collect();
        assert_eq!(mut_ids, ids);
    }

    #[test]
    fn slot_zero_is_never_issued_and_the_default_id_never_resolves() {
        let mut doc = crate::BaseDocument::new(Default::default());
        let id = span(&mut doc);
        assert!(doc.nodes.iter().all(|(id, _)| id.as_u64() as u32 != 0));
        assert_ne!(id.as_u64() as u32, 0);
        let default = blitz_traits::node_id::NodeId::default();
        assert!(!doc.nodes.contains_key(default));
        assert_eq!(doc.nodes.id_from_opaque(0), None);
    }

    #[test]
    fn reused_versions_wrap_within_31_bits_and_skip_zero() {
        assert_eq!(super::next_version(super::FREE | 1), 2);
        assert_eq!(super::next_version(super::FREE | 0x7fff_fffe), 0x7fff_ffff);
        assert_eq!(super::next_version(super::FREE | 0x7fff_ffff), 1);
    }

    #[test]
    fn dropping_a_tree_with_live_and_freed_slots_runs_clean() {
        let mut doc = crate::BaseDocument::new(Default::default());
        let ids: Vec<_> = (0..64).map(|_| span(&mut doc)).collect();
        for &id in ids.iter().step_by(3) {
            doc.mutate().remove_and_drop_node(id);
        }
        let live = doc.nodes.len();
        assert_eq!(doc.nodes.iter().count(), live);
        drop(doc);
    }
}
