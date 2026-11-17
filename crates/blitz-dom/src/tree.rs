//! Versioned storage for the nodes of the DOM tree.

use std::ops::{Index, IndexMut};
use std::sync::atomic::{AtomicU64, Ordering};

use blitz_traits::node_id::NodeId;
use slotmap::{Key as _, KeyData, SlotMap};

use crate::Node;

slotmap::new_key_type! {
    /// The internal [`slotmap`] key for node storage. Only used at the
    /// storage boundary: all public APIs use [`NodeId`].
    struct NodeKey;
}

#[inline(always)]
fn to_key(id: NodeId) -> NodeKey {
    NodeKey::from(KeyData::from_ffi(id.as_u64()))
}

#[inline(always)]
fn to_id(key: NodeKey) -> NodeId {
    NodeId::from_u64(key.data().as_ffi())
}

/// The versioned map in which the nodes of the DOM tree are stored, backed by
/// a [`slotmap::SlotMap`].
///
/// Nodes are addressed by [`NodeId`], which carries the slot's version in
/// addition to its index: when a node is dropped and its slot reused, ids
/// referring to the dropped node no longer resolve ([`NodeTree::get`] returns
/// `None`, and indexing panics) instead of aliasing the new occupant.
pub struct NodeTree {
    map: SlotMap<NodeKey, Node>,
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
    /// Each slot index's current id (raw `NodeId`), 0 for a free
    /// slot. Backs `id_from_opaque`.
    slot_ids: Vec<u64>,
}

/// The slot index of an id: the low half of the slotmap FFI encoding, which
/// keeps the version in the high half.
#[inline(always)]
fn slot_index(raw: u64) -> usize {
    (raw & 0xffff_ffff) as usize
}

impl NodeTree {
    pub(crate) fn new() -> Self {
        Self {
            map: SlotMap::with_key(),
            geometry_generation: AtomicU64::new(1),
            viewport_scroll: crate::Point::ZERO,
            fixed_anchor: None,
            slot_ids: Vec::new(),
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
    /// fragment root freed stayed at its start opacity. `slot_ids` maps each slot index to its current id,
    /// so the lookup works whatever the pointer width.
    pub fn id_from_opaque(&self, opaque: usize) -> Option<NodeId> {
        let raw = opaque as u64;
        let id = NodeId::from_u64(*self.slot_ids.get(slot_index(raw))?);
        // A 64-bit opaque still has its version: reject a stale one.
        let stale = raw >> 32 != 0 && raw != id.as_u64();
        (!stale && self.contains_key(id)).then_some(id)
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

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let mut tree = Self::new();
        tree.map = SlotMap::with_capacity_and_key(capacity);
        tree
    }

    /// The number of live nodes in the map.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Whether `id` resolves to a live node.
    pub fn contains_key(&self, id: NodeId) -> bool {
        self.map.contains_key(to_key(id))
    }

    /// Get a reference to the node with the given id, if it is still live.
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.map.get(to_key(id))
    }

    /// Get a mutable reference to the node with the given id, if it is still live.
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.map.get_mut(to_key(id))
    }

    /// Insert a node constructed with knowledge of its own id.
    pub(crate) fn insert_with_key(&mut self, f: impl FnOnce(NodeId) -> Node) -> NodeId {
        let id = to_id(self.map.insert_with_key(|key| f(to_id(key))));
        let index = slot_index(id.as_u64());
        if self.slot_ids.len() <= index {
            self.slot_ids.resize(index + 1, 0);
        }
        self.slot_ids[index] = id.as_u64();
        id
    }

    /// Remove the node with the given id, returning it if it was still live.
    pub(crate) fn remove(&mut self, id: NodeId) -> Option<Node> {
        let node = self.map.remove(to_key(id))?;
        self.slot_ids[slot_index(id.as_u64())] = 0;
        Some(node)
    }

    /// Iterate over all live `(NodeId, &Node)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        self.map.iter().map(|(key, node)| (to_id(key), node))
    }

    /// Iterate over all live `(NodeId, &mut Node)` pairs.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (NodeId, &mut Node)> {
        self.map.iter_mut().map(|(key, node)| (to_id(key), node))
    }
}

impl Index<NodeId> for NodeTree {
    type Output = Node;

    #[track_caller]
    #[inline]
    fn index(&self, id: NodeId) -> &Node {
        &self.map[to_key(id)]
    }
}

impl IndexMut<NodeId> for NodeTree {
    #[track_caller]
    #[inline]
    fn index_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.map[to_key(id)]
    }
}
