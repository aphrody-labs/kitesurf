use crate::Document;
use crate::pool::PoolBox;
use crate::layout::paint_tree::{HoistedPaintChild, StackingContext};
use bitflags::bitflags;
use blitz_traits::events::{
    BlitzPointerEvent, BlitzPointerId, DomEventData, HitResult, PointerCoords,
};
use blitz_traits::node_id::NodeId;
use blitz_traits::shell::ShellProvider;
use euclid::{Point2D, Rect, Size2D};
use keyboard_types::Modifiers;
use kurbo::{Affine, Rect as KurboRect};
use markup5ever::{LocalName, local_name};
use parley::{BreakReason, Cluster, ClusterSide};
use std::cell::{Cell, OnceCell, Ref, RefCell, RefMut};
use std::fmt::Write;
use std::ops::Deref;
use std::sync::Arc;
#[cfg(feature = "writing-mode")]
use style::computed_values::flex_direction::T as FlexDirection;
use style::computed_values::isolation::T as Isolation;
use style::computed_values::white_space_collapse::T as WhiteSpaceCollapse;
use style::invalidation::element::restyle_hints::RestyleHint;
use style::properties::ComputedValues;
use style::properties::generated::longhands::position::computed_value::T as Position;
use style::selector_parser::RestyleDamage;
use style::servo_arc::Arc as ServoArc;
use style::shared_lock::SharedRwLock;
use style::author_styles::AuthorStyles;
use style::stylesheets::{DocumentStyleSheet, UrlExtraData};
use style::values::computed::CSSPixelLength;
use style::values::computed::Display as StyloDisplay;
use style::values::computed::Rotate;
use style::values::generics::transform::{Scale, Translate};
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use style_dom::ElementState;
use style_traits::values::ToCss;
use taffy::{Cache, prelude::Layout};
use thin_vec::ThinVec;

use super::stylo_data::{ComputedStyleRef, NodeStateFlags, StyloData, StyloDataMut};
use super::{Attribute, DocumentData, ElementData, ElementStateBits, LayoutData, SelectorFlagsRef};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayOuter {
    Block,
    Inline,
    None,
}

/// Low bits of a node's `NodeFlags` word that hold its own slot index. Wasm32 memory fits at
/// most 2^32 / 36 B ≈ 119M node slots, under 2^27, so any page's index fits beside the flags.
pub(crate) const SLOT_INDEX_BITS: u32 = 27;
pub(crate) const SLOT_INDEX_MASK: u32 = (1 << SLOT_INDEX_BITS) - 1;

bitflags! {
    /// A node's flags in the high bits; the low `SLOT_INDEX_BITS` hold its slot index, which
    /// `insert`, `remove`, `set` and `contains` leave alone.
    #[derive(Clone, Copy, PartialEq)]
    pub struct NodeFlags: u32 {
        /// Whether the node is the root node of an Inline Formatting Context
        const IS_INLINE_ROOT = 1 << 29;
        /// Whether the node is the root node of an Table formatting context
        const IS_TABLE_ROOT = 1 << 30;
        /// Whether the node is "in the document" (~= has a parent and isn't a template node)
        const IS_IN_DOCUMENT = 1 << 31;
    }
}

const _: () = assert!(NodeFlags::all().bits() & SLOT_INDEX_MASK == 0);

impl NodeFlags {
    #[inline(always)]
    fn slot_index(&self) -> u32 {
        self.bits() & SLOT_INDEX_MASK
    }

    #[inline(always)]
    pub fn is_inline_root(&self) -> bool {
        self.contains(Self::IS_INLINE_ROOT)
    }

    #[inline(always)]
    pub fn is_table_root(&self) -> bool {
        self.contains(Self::IS_TABLE_ROOT)
    }

    #[inline(always)]
    pub fn is_in_document(&self) -> bool {
        self.contains(Self::IS_IN_DOCUMENT)
    }

    #[inline(always)]
    pub fn reset_construction_flags(&mut self) {
        self.remove(Self::IS_INLINE_ROOT);
        self.remove(Self::IS_TABLE_ROOT);
    }
}

/// A nullable full-generation node ID. The all-ones null key is never issued
/// for a live node (`NodeTree` never uses slot `u32::MAX` or a `FREE` version).
/// In particular, zero is a valid represented `Some(NodeId::default())`.
/// Holds a `NodeId` rather than its `u64` so it keeps the id's 4-byte alignment.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct OptNodeId(NodeId);

impl std::fmt::Debug for OptNodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.get(), f)
    }
}

impl OptNodeId {
    const NONE: u64 = u64::MAX;

    pub fn from_option(id: Option<NodeId>) -> Self {
        Self(match id {
            Some(id) => {
                assert_ne!(id.as_u64(), Self::NONE, "reserved null-slot node ID");
                id
            }
            None => NodeId::from_u64(Self::NONE),
        })
    }

    pub fn get(self) -> Option<NodeId> {
        (self.0.as_u64() != Self::NONE).then_some(self.0)
    }

    pub fn set(&mut self, id: Option<NodeId>) {
        *self = Self::from_option(id);
    }

    pub fn take(&mut self) -> Option<NodeId> {
        std::mem::take(self).get()
    }
}

impl From<Option<NodeId>> for OptNodeId {
    fn from(id: Option<NodeId>) -> Self {
        Self::from_option(id)
    }
}

impl Default for OptNodeId {
    fn default() -> Self {
        Self::from_option(None)
    }
}

pub struct NodeIdCell(Cell<OptNodeId>);

impl std::fmt::Debug for NodeIdCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.get(), f)
    }
}

impl Default for NodeIdCell {
    fn default() -> Self {
        Self::new(None)
    }
}

impl NodeIdCell {
    pub fn new(id: Option<NodeId>) -> Self {
        Self(Cell::new(OptNodeId::from_option(id)))
    }

    pub fn get(&self) -> Option<NodeId> {
        self.0.get().get()
    }

    pub fn set(&self, id: Option<NodeId>) {
        self.0.set(OptNodeId::from_option(id));
    }

    pub fn replace(&self, id: Option<NodeId>) -> Option<NodeId> {
        self.0.replace(OptNodeId::from_option(id)).get()
    }

    pub fn take(&self) -> Option<NodeId> {
        self.replace(None)
    }
}

/// A node's tree links in one word. The low `SLOT_INDEX_BITS` hold the parent's slot index, 0
/// (the tree's never-live sentinel slot) for none. Bits 27-28 say whether the layout parent is
/// none, the parent itself (nine in ten nodes), or another node in the tree's side table.
const LAYOUT_KIND_MASK: u32 = 0b11 << SLOT_INDEX_BITS;
const LAYOUT_NONE: u32 = 0;
const LAYOUT_SAME: u32 = 0b01 << SLOT_INDEX_BITS;
const LAYOUT_OTHER: u32 = 0b10 << SLOT_INDEX_BITS;
const _: () = assert!(LAYOUT_KIND_MASK & NodeFlags::all().bits() == 0);

/// The slot index half of a node id.
#[inline(always)]
fn slot_index(id: NodeId) -> u32 {
    id.as_u64() as u32
}

/// Rebuilds the id of the live node in slot `index` of `tree`.
#[inline(always)]
fn link_id(tree: *const crate::NodeTree, index: u32) -> NodeId {
    // SAFETY: `tree` is always a node's own tree pointer (see `version_at`).
    let version = unsafe { crate::tree::version_at(tree, index) };
    debug_assert_ne!(version, 0, "tree link to free slot {index}");
    crate::tree::id_from_parts(index, version)
}

/// The slot index to store for a link to `id`, which must be live in `tree`.
#[inline(always)]
fn link_index(tree: *const crate::NodeTree, id: NodeId) -> u32 {
    let index = slot_index(id);
    debug_assert_eq!(
        // SAFETY: `tree` is always a node's own tree pointer (see `version_at`).
        unsafe { crate::tree::version_at(tree, index) },
        (id.as_u64() >> 32) as u32,
        "tree link to a node that is not live: {id:?}"
    );
    index
}

/// A read view of a child list, yielding full [`NodeId`]s: either a node's
/// DOM children (stored as slot indexes) or a slot's assigned nodes.
#[derive(Clone, Copy)]
pub struct ChildIds<'a>(ChildRepr<'a>);

#[derive(Clone, Copy)]
enum ChildRepr<'a> {
    One(*const crate::NodeTree, u32),
    Links(*const crate::NodeTree, &'a [u32]),
    Ids(&'a [NodeId]),
}

impl<'a> ChildIds<'a> {
    fn of(tree: *const crate::NodeTree, list: &'a super::child_list::ChildList) -> Self {
        use super::child_list::ChildSlots;
        ChildIds(match list.slots() {
            ChildSlots::One(index) => ChildRepr::One(tree, index),
            ChildSlots::Many(slots) => ChildRepr::Links(tree, slots),
        })
    }

    #[inline]
    pub fn len(&self) -> usize {
        match self.0 {
            ChildRepr::One(..) => 1,
            ChildRepr::Links(_, slots) => slots.len(),
            ChildRepr::Ids(ids) => ids.len(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn iter(&self) -> ChildIdsIter<'a> {
        ChildIdsIter(match self.0 {
            ChildRepr::One(tree, index) => ChildIterRepr::One(tree, Some(index)),
            ChildRepr::Links(tree, slots) => ChildIterRepr::Links(tree, slots.iter()),
            ChildRepr::Ids(ids) => ChildIterRepr::Ids(ids.iter()),
        })
    }

    #[inline]
    pub fn get(&self, i: usize) -> Option<NodeId> {
        match self.0 {
            ChildRepr::One(tree, index) => (i == 0).then(|| link_id(tree, index)),
            ChildRepr::Links(tree, slots) => slots.get(i).map(|&index| link_id(tree, index)),
            ChildRepr::Ids(ids) => ids.get(i).copied(),
        }
    }

    #[inline]
    pub fn first(&self) -> Option<NodeId> {
        self.get(0)
    }

    #[inline]
    pub fn last(&self) -> Option<NodeId> {
        self.len().checked_sub(1).and_then(|i| self.get(i))
    }

    pub fn contains(&self, id: NodeId) -> bool {
        self.position(id).is_some()
    }

    pub fn position(&self, id: NodeId) -> Option<usize> {
        self.iter().position(|child| child == id)
    }

    pub fn to_vec(&self) -> Vec<NodeId> {
        self.iter().collect()
    }
}

impl<'a> IntoIterator for ChildIds<'a> {
    type Item = NodeId;
    type IntoIter = ChildIdsIter<'a>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl std::fmt::Debug for ChildIds<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// A child list moved out by `Node::take_detached_children`.
pub(crate) struct DetachedChildren {
    tree: *const crate::NodeTree,
    slots: super::child_list::ChildList,
}

impl DetachedChildren {
    pub(crate) fn ids(&self) -> ChildIds<'_> {
        ChildIds::of(self.tree, &self.slots)
    }
}

/// Iterator over a [`ChildIds`] view.
#[derive(Clone)]
pub struct ChildIdsIter<'a>(ChildIterRepr<'a>);

#[derive(Clone)]
enum ChildIterRepr<'a> {
    One(*const crate::NodeTree, Option<u32>),
    Links(*const crate::NodeTree, std::slice::Iter<'a, u32>),
    Ids(std::slice::Iter<'a, NodeId>),
}

impl Iterator for ChildIdsIter<'_> {
    type Item = NodeId;

    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        match &mut self.0 {
            ChildIterRepr::One(tree, index) => index.take().map(|index| link_id(*tree, index)),
            ChildIterRepr::Links(tree, slots) => slots.next().map(|&index| link_id(*tree, index)),
            ChildIterRepr::Ids(ids) => ids.next().copied(),
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.len();
        (len, Some(len))
    }
}

impl DoubleEndedIterator for ChildIdsIter<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<NodeId> {
        match &mut self.0 {
            ChildIterRepr::One(tree, index) => index.take().map(|index| link_id(*tree, index)),
            ChildIterRepr::Links(tree, slots) => {
                slots.next_back().map(|&index| link_id(*tree, index))
            }
            ChildIterRepr::Ids(ids) => ids.next_back().copied(),
        }
    }
}

impl ExactSizeIterator for ChildIdsIter<'_> {
    #[inline]
    fn len(&self) -> usize {
        match &self.0 {
            ChildIterRepr::One(_, index) => usize::from(index.is_some()),
            ChildIterRepr::Links(_, slots) => slots.len(),
            ChildIterRepr::Ids(ids) => ids.len(),
        }
    }
}

impl std::iter::FusedIterator for ChildIdsIter<'_> {}

#[cfg(test)]
mod compact_node_tests {
    use super::*;

    #[test]
    fn flag_writes_keep_the_slot_index_packed_beside_them() {
        let mut flags = NodeFlags::from_bits_retain(SLOT_INDEX_MASK);
        flags.insert(NodeFlags::IS_IN_DOCUMENT | NodeFlags::IS_INLINE_ROOT);
        flags.set(NodeFlags::IS_TABLE_ROOT, true);
        flags.reset_construction_flags();
        flags.remove(NodeFlags::IS_IN_DOCUMENT);
        flags.set(NodeFlags::IS_IN_DOCUMENT, true);
        assert_eq!(flags.slot_index(), SLOT_INDEX_MASK);
        assert!(flags.is_in_document() && !flags.is_inline_root() && !flags.is_table_root());

        let mut doc = crate::BaseDocument::new(Default::default());
        let element = doc.mutate().create_element(crate::qual_name!("div"), vec![]);
        let root = doc.root_node().id();
        doc.mutate().append_children(root, &[element]);
        doc.nodes[element].flags.set(NodeFlags::IS_IN_DOCUMENT, false);
        doc.nodes[element].flags.insert(NodeFlags::IS_INLINE_ROOT);
        assert_eq!(doc.nodes[element].id(), element);
    }

    #[test]
    fn box_child_lists_preserve_empty_state_and_reset_on_clone() {
        let mut doc = crate::BaseDocument::new(Default::default());
        let element = doc.mutate().create_element(crate::qual_name!("div"), vec![]);
        let text = doc.create_text_node("text");
        let node = &doc.nodes[element];
        assert!(node.layout_children().is_none());
        node.set_layout_children(None);
        node.set_paint_children(None);
        assert!(node.element_data().unwrap().layout_data.get().is_none(), "clearing allocates");
        node.set_layout_children(Some(ThinVec::new()));
        node.set_paint_children(Some([text].into_iter().collect()));
        let layout = node.layout_children().unwrap();
        let paint = node.paint_children().unwrap();
        assert_eq!(layout.len(), 0);
        assert_eq!(&paint[..], &[text]);
        drop(paint);
        drop(layout);

        let clone = doc.deep_clone_node(element);
        assert!(doc.nodes[clone].layout_children().is_none());
        assert!(doc.nodes[clone].paint_children().is_none());
        assert_eq!(doc.nodes[element].take_layout_children(), Some(ThinVec::new()));
        assert_eq!(doc.nodes[element].take_layout_children(), None);
        assert!(doc.nodes[text].layout_children().is_none());
        assert!(doc.nodes[text].paint_children().is_none());
        doc.nodes[text].take_layout_children();
        doc.nodes[text].take_paint_children();
        assert!(doc.nodes[text].rare.get().is_none());
    }

    #[test]
    fn laying_out_in_flow_boxes_keeps_rare_storage_cold() {
        use crate::{Attribute, DocumentConfig, qual_name};
        use blitz_traits::shell::{ColorScheme, Viewport};

        let mut doc = crate::BaseDocument::new(DocumentConfig {
            viewport: Some(Viewport::new(400, 300, 1.0, ColorScheme::Light)),
            ..Default::default()
        });
        let root_id = doc.root_node().id();
        let style = |value: &str| Attribute::new(qual_name!("style"), value.to_string());
        let mut mutator = doc.mutate();
        let html = mutator.create_element(qual_name!("html"), vec![]);
        let body = mutator.create_element(qual_name!("body"), vec![]);
        let outer = mutator.create_element(qual_name!("div"), vec![style("display:flex")]);
        let inner = mutator.create_element(qual_name!("div"), vec![style("height:10px")]);
        mutator.append_children(outer, &[inner]);
        mutator.append_children(body, &[outer]);
        mutator.append_children(html, &[body]);
        mutator.append_children(root_id, &[html]);
        drop(mutator);
        doc.resolve(0.0);

        // The root element holds the root stacking context; nothing else needs rare storage.
        assert!(doc.nodes[html].stacking_context().is_some());
        let warm: Vec<_> = [body, outer, inner]
            .into_iter()
            .filter(|&id| doc.nodes[id].rare.get().is_some())
            .collect();
        assert_eq!(warm, []);
    }

    #[test]
    fn slot_links_resolve_to_the_current_version_of_a_reused_slot() {
        let mut doc = crate::BaseDocument::new(Default::default());
        let root = doc.root_node().id();
        let element = doc.mutate().create_element(crate::qual_name!("div"), vec![]);
        let dropped = doc.create_text_node("dropped");
        doc.mutate().append_children(element, &[dropped]);
        doc.mutate().remove_and_drop_node(dropped);

        // The freed slot is reused at a newer version, so a stale index-only link would alias.
        let first = doc.create_text_node("first");
        assert_eq!(first.as_u64() as u32, dropped.as_u64() as u32);
        assert_ne!(first, dropped);
        let second = doc.create_text_node("second");
        let mut mutator = doc.mutate();
        mutator.append_children(element, &[first, second]);
        mutator.append_children(root, &[element]);
        drop(mutator);

        assert_eq!(doc.nodes[first].parent_id(), Some(element));
        assert_eq!(doc.nodes[second].parent_id(), Some(element));
        assert_eq!(doc.nodes[element].parent_id(), Some(root));
        let children = doc.nodes[element].children();
        assert_eq!(children.iter().collect::<Vec<_>>(), vec![first, second]);
        assert_eq!(children.iter().rev().collect::<Vec<_>>(), vec![second, first]);
        assert_eq!((children.first(), children.last()), (Some(first), Some(second)));
        assert_eq!(children.position(second), Some(1));
        assert!(!children.contains(dropped));
        assert!(doc.nodes[root].children().contains(element));

        doc.nodes[first].set_layout_parent_id(Some(element));
        assert_eq!(doc.nodes[first].layout_parent_id(), Some(element));
        assert_eq!(doc.nodes[first].take_layout_parent_id(), Some(element));
        assert_eq!(doc.nodes[first].layout_parent_id(), None);

        // A layout parent that was the DOM parent stays put when the DOM parent changes, and
        // one in the side table leaves no entry behind once it is the parent or none again.
        let table_len = |doc: &crate::BaseDocument| {
            unsafe { crate::tree::layout_parents(doc.tree()) }.borrow().len()
        };
        doc.nodes[first].set_layout_parent_id(Some(element));
        doc.nodes[first].set_parent_id(Some(root));
        assert_eq!(doc.nodes[first].parent_id(), Some(root));
        assert_eq!(doc.nodes[first].layout_parent_id(), Some(element));
        assert_eq!(table_len(&doc), 1);
        doc.nodes[first].set_parent_id(Some(element));
        assert_eq!(doc.nodes[first].layout_parent_id(), Some(element));
        assert_eq!(table_len(&doc), 0);
        doc.nodes[second].set_layout_parent_id(Some(root));
        assert_eq!(doc.nodes[second].replace_layout_parent_id(None), Some(root));
        assert_eq!(table_len(&doc), 0);
        doc.nodes[second].set_layout_parent_id(Some(root));
        doc.mutate().remove_and_drop_node(second);
        assert_eq!(table_len(&doc), 0, "a dropped node's entry goes with it");
        assert_eq!(doc.nodes[element].parent_id(), Some(root));
    }

    #[test]
    fn dropping_a_wrapped_node_leaves_no_link_to_its_slot() {
        use crate::{DocumentConfig, qual_name};
        use blitz_traits::shell::{ColorScheme, Viewport};
        let mut doc = crate::BaseDocument::new(DocumentConfig {
            viewport: Some(Viewport::new(400, 300, 1.0, ColorScheme::Light)),
            ..Default::default()
        });
        let root_id = doc.root_node().id();
        let mut m = doc.mutate();
        let html = m.create_element(qual_name!("html"), vec![]);
        let body = m.create_element(qual_name!("body"), vec![]);
        let span = m.create_element(qual_name!("span"), vec![]);
        let style = crate::Attribute::new(qual_name!("style"), "display:block".to_string());
        let block = m.create_element(qual_name!("div"), vec![style]);
        m.append_children(html, &[body]);
        m.append_children(root_id, &[html]);
        drop(m);
        let text = doc.create_text_node("inline");
        let mut m = doc.mutate();
        m.append_children(body, &[text, span, block]);
        drop(m);
        doc.resolve(0.0);
        assert!(!doc.nodes[body].anonymous_blocks().is_empty());

        // Dropping a node an anonymous block wraps takes it off the block's list at once, so
        // no link names a freed slot, nor the node that next reuses it.
        let wrapper = doc.nodes[text].layout_parent_id().unwrap();
        doc.mutate().remove_and_drop_node(text);
        assert!(!doc.nodes[wrapper].children().contains(text));
        doc.resolve(0.0);
        let wrapper = doc.nodes[span].layout_parent_id().unwrap();
        doc.mutate().remove_and_drop_node(span);
        assert!(doc.nodes[wrapper].children().is_empty());
        let reused = doc.create_text_node("reused");
        assert_eq!(reused.as_u64() as u32, span.as_u64() as u32);
        doc.resolve(0.0);
        assert_eq!(doc.nodes[body].children().to_vec(), vec![block]);
    }

    #[test]
    fn nullable_ids_preserve_zero_and_full_generations() {
        let high_generation = NodeId::from_u64(0xffff_fffe_ffff_fffe);
        for id in [NodeId::default(), high_generation] {
            let mut packed = OptNodeId::from_option(Some(id));
            assert_eq!(packed.get(), Some(id));
            assert_eq!(packed.take(), Some(id));
            assert_eq!(packed.get(), None);

            let cell = NodeIdCell::new(Some(id));
            assert_eq!(cell.replace(None), Some(id));
            assert_eq!(cell.take(), None);
            cell.set(Some(id));
            assert_eq!(cell.get(), Some(id));
        }

        assert_eq!(OptNodeId::default().get(), None);
        assert!(
            std::panic::catch_unwind(|| {
                OptNodeId::from_option(Some(NodeId::from_u64(u64::MAX)))
            })
            .is_err()
        );
    }

    #[test]
    fn ordinary_node_access_and_empty_resets_keep_rare_storage_cold() {
        const MAX_COLD_NODE_BYTES: usize = 192;
        assert!(std::mem::size_of::<Node>() <= MAX_COLD_NODE_BYTES);

        let mut doc = crate::BaseDocument::new(Default::default());
        let root = doc.root_node_mut();
        assert!(root.rare.get().is_none());
        assert_eq!(root.parent_id(), None);
        assert_eq!(root.assigned_slot(), None);
        assert!(root.assigned_nodes().is_none());
        assert_eq!(root.paint_parent_override(), None);
        assert!(root.rehomed_paint_children().is_none());
        assert!(root.hoisted_children().is_none());
        assert_eq!(root.oof_containing_block(), None);
        assert!(!root.placed_floats());
        assert_eq!(root.rehomed_reach(), None);
        assert!(root.stacking_context().is_none());
        assert!(root.sc_contribution_cache().is_none());

        root.set_assigned_slot(None);
        root.set_assigned_nodes(None);
        assert!(root.take_assigned_nodes().is_none());
        root.set_paint_parent_override(None);
        root.clear_rehomed_paint_children();
        root.clear_hoisted_children();
        assert!(root.take_hoisted_children().is_empty());
        root.set_hoisted_children(ThinVec::new());
        root.set_oof_containing_block(None);
        root.set_placed_floats(false);
        root.set_rehomed_reach(None);
        root.set_stacking_context(None);
        assert!(root.take_stacking_context().is_none());
        root.set_sc_contribution_cache(ThinVec::new());
        root.clear_sc_contribution_cache();
        assert!(root.take_sc_contribution_cache().is_empty());
        assert!(root.rare.get().is_none());
    }

    #[test]
    fn rare_sidecars_keep_id_and_computed_empty_slot_state() {
        let mut doc = crate::BaseDocument::new(Default::default());
        let node = doc.root_node_mut();
        let id = NodeId::default();
        node.set_assigned_slot(Some(id));
        node.set_paint_parent_override(Some(id));
        node.set_oof_containing_block(Some(id));
        assert_eq!(node.assigned_slot(), Some(id));
        assert_eq!(node.paint_parent_override(), Some(id));
        assert_eq!(node.oof_containing_block(), Some(id));

        let mut hoisted = node.hoisted_children_mut();
        let mut rehomed = node.rehomed_paint_children_mut();
        hoisted.push(id);
        rehomed.push(id);
        assert_eq!(hoisted.iter().copied().collect::<Vec<_>>(), vec![id]);
        assert_eq!(rehomed.iter().copied().collect::<Vec<_>>(), vec![id]);
        drop(rehomed);
        drop(hoisted);
        assert!(node.rare.get().is_some());

        node.set_assigned_nodes(Some(ThinVec::new()));
        assert_eq!(node.assigned_nodes().map(|nodes| nodes.len()), Some(0));
        assert_eq!(node.take_assigned_nodes().map(|nodes| nodes.len()), Some(0));
        assert!(node.assigned_nodes().is_none());
    }

    #[test]
    fn unchanged_scroll_offset_keeps_layout_state_cold() {
        let mut doc = crate::BaseDocument::new(Default::default());
        let id = doc.mutate().create_element(
            markup5ever::QualName::new(
                None,
                markup5ever::ns!(html),
                markup5ever::local_name!("span"),
            ),
            Vec::new(),
        );
        let node = doc.get_node_mut(id).unwrap();
        let _ = node.final_layout();
        let _ = node.cache();
        node.clear_layout_cache();
        assert!(node.try_layout_data_mut().is_none());
        node.set_scroll_offset(crate::Point::ZERO);
        assert!(node.try_layout_data_mut().is_none());
        let offset = crate::Point { x: 4.0, y: 9.0 };
        node.set_scroll_offset(offset);
        assert_eq!(*node.scroll_offset(), offset);
        assert!(node.try_layout_data_mut().is_some());
    }
}

#[derive(Default)]
struct NodeRare {
    /// Flat-tree edge recomputed by `flush_slot_assignments`.
    assigned_slot: Cell<OptNodeId>,
    /// `None` is not a slot; `Some(empty)` is computed and renders fallback.
    assigned_nodes: Option<ThinVec<NodeId>>,
    paint_parent_override: Cell<OptNodeId>,
    rehomed_paint_children: RefCell<ThinVec<NodeId>>,
    hoisted_children: RefCell<ThinVec<NodeId>>,
    oof_containing_block: NodeIdCell,
    placed_floats: Cell<bool>,
    rehomed_reach: RefCell<Option<Box<(f32, f32, f32, f32)>>>,
    stacking_context: RefCell<Option<Box<StackingContext>>>,
    sc_contribution_cache: RefCell<ThinVec<HoistedPaintChild>>,
    /// Anonymous block boxes created for this node during layout construction.
    ///
    /// Anonymous blocks live only in the slab (they are not part of the DOM
    /// `children` list), so we track the ones we own here to be able to
    /// deallocate them when this node is reconstructed.
    anonymous_blocks: ThinVec<NodeId>,
}

/// A box's derived layout and paint children. Kept in its `LayoutData`: the boxes that have
/// lists are the ones layout writes to, a quarter of a document's elements.
#[derive(Clone, Debug, Default)]
pub(crate) struct LayoutChildLists {
    layout: RefCell<Option<ThinVec<NodeId>>>,
    paint: RefCell<Option<ThinVec<NodeId>>>,
}

impl LayoutChildLists {
    pub(crate) const fn new() -> Self {
        Self {
            layout: RefCell::new(None),
            paint: RefCell::new(None),
        }
    }
}

/// Parents tracked at once by `child_index`; a tree walk keeps one slot per open ancestor.
const CHILD_INDEX_HINTS: usize = 64;

thread_local! {
    /// Last child index found under each parent slot. Storing it in `Node` would grow every node.
    static CHILD_INDEX_HINT: [Cell<usize>; CHILD_INDEX_HINTS] =
        const { [const { Cell::new(0) }; CHILD_INDEX_HINTS] };
}

pub struct Node {
    // The actual tree we belong to. This is unsafe!!
    tree: *mut crate::NodeTree,

    /// Our parent's slot index and how to find our layout parent (see `LAYOUT_KIND_MASK`); read
    /// through `parent_id` and `layout_parent_id`.
    links: Cell<u32>,
    /// Our children's slot indexes, a lone child inline; read through `children`.
    children: super::child_list::ChildList,

    /// Sparse shadow, out-of-flow, and paint bookkeeping.
    rare: OnceCell<Box<NodeRare>>,

    /// Flags, packed above our own slot index (the version comes from the tree, see `id`).
    pub flags: NodeFlags,

    /// Node type (Element, TextNode, etc) specific data.
    ///
    /// For element nodes this holds the [`ElementData`], which stores most of
    /// the per-node style/layout state. For the document node it holds the
    /// [`DocumentData`]. Access the moved fields through the forwarding methods
    /// on [`Node`] (e.g. [`Node::style`], [`Node::final_layout`]).
    pub data: NodeData,
}

unsafe impl Send for Node {}
unsafe impl Sync for Node {}

/// Generates forwarding accessors for fields that live on both [`ElementData`]
/// (element / anonymous block nodes) and [`DocumentData`] (the document node).
macro_rules! universal_accessors {
    ($($(#[$meta:meta])* $field:ident / $field_mut:ident : $ty:ty),* $(,)?) => {
        impl Node {
            $(
                $(#[$meta])*
                #[inline]
                pub fn $field(&self) -> &$ty {
                    match &self.data {
                        NodeData::Element(data) | NodeData::AnonymousBlock(data) => &data.$field,
                        NodeData::Document(data) => &data.$field,
                        _ => panic!(concat!("`", stringify!($field), "` is not available on this node kind")),
                    }
                }

                $(#[$meta])*
                #[inline]
                pub fn $field_mut(&mut self) -> &mut $ty {
                    match &mut self.data {
                        NodeData::Element(data) | NodeData::AnonymousBlock(data) => &mut data.$field,
                        NodeData::Document(data) => &mut data.$field,
                        _ => panic!(concat!("`", stringify!($field), "` is not available on this node kind")),
                    }
                }
            )*
        }
    };
}

universal_accessors! {
    // The document node is styled/snapshotted like an element, so it also
    // carries these (and `snapshot_handled`, a bit of `NodeStateFlags`):
    element_state / element_state_mut: ElementStateBits,
}

impl Node {
    /// The display this box was last constructed with. Panics on non-box node kinds.
    #[inline]
    pub fn display_constructed_as(&self) -> StyloDisplay {
        match &self.data {
            NodeData::Element(data) | NodeData::AnonymousBlock(data) => {
                data.display_constructed_as()
            }
            NodeData::Document(data) => data.display_constructed_as(),
            _ => panic!("`display_constructed_as` is not available on this node kind"),
        }
    }

    #[inline]
    pub fn set_display_constructed_as(&mut self, display: StyloDisplay) {
        match &mut self.data {
            NodeData::Element(data) | NodeData::AnonymousBlock(data) => {
                data.set_display_constructed_as(display)
            }
            NodeData::Document(data) => data.set_display_constructed_as(display),
            _ => panic!("`display_constructed_as` is not available on this node kind"),
        }
    }

    /// `apply_selector_flags` deposits `for_parent()` flags on the parent node, and the parent
    /// of the root <html> element is the document -- so the document holds selector flags too.
    #[inline]
    pub fn selector_flags(&self) -> SelectorFlagsRef<'_> {
        match &self.data {
            NodeData::Element(data) | NodeData::AnonymousBlock(data) => data.selector_flags(),
            NodeData::Document(data) => data.selector_flags(),
            _ => panic!("`selector_flags` is not available on this node kind"),
        }
    }
}

impl Node {
    /// The layout data slot of a box node; `None` for kinds that never lay out.
    fn layout_data_cell(&self) -> Option<&OnceCell<PoolBox<super::LayoutData>>> {
        match &self.data {
            NodeData::Element(data) | NodeData::AnonymousBlock(data) => Some(&data.layout_data),
            NodeData::Document(data) => Some(&data.layout_data),
            _ => None,
        }
    }

    fn child_lists(&self) -> Option<&LayoutChildLists> {
        Some(&self.layout_data_cell()?.get()?.child_lists)
    }

    /// The child lists to store `children` in: allocating the layout data for a list, but not
    /// to clear one.
    fn child_lists_for(&self, children: &Option<ThinVec<NodeId>>) -> Option<&LayoutChildLists> {
        let Some(cell) = self.layout_data_cell() else {
            assert!(children.is_none(), "only box nodes have derived children");
            return None;
        };
        match children {
            Some(_) => Some(&cell.get_or_init(PoolBox::default).child_lists),
            None => Some(&cell.get()?.child_lists),
        }
    }

    /// Derived box children do not exist on text, comment or shadow-root nodes.
    pub fn layout_children(&self) -> Option<Ref<'_, ThinVec<NodeId>>> {
        Ref::filter_map(self.child_lists()?.layout.borrow(), |children| children.as_ref()).ok()
    }

    pub fn paint_children(&self) -> Option<Ref<'_, ThinVec<NodeId>>> {
        Ref::filter_map(self.child_lists()?.paint.borrow(), |children| children.as_ref()).ok()
    }

    pub fn take_layout_children(&self) -> Option<ThinVec<NodeId>> {
        self.child_lists()?.layout.borrow_mut().take()
    }

    pub fn take_paint_children(&self) -> Option<ThinVec<NodeId>> {
        self.child_lists()?.paint.borrow_mut().take()
    }

    pub fn set_layout_children(&self, children: Option<ThinVec<NodeId>>) {
        if let Some(lists) = self.child_lists_for(&children) {
            *lists.layout.borrow_mut() = children;
        }
    }

    pub fn set_paint_children(&self, children: Option<ThinVec<NodeId>>) {
        if let Some(lists) = self.child_lists_for(&children) {
            *lists.paint.borrow_mut() = children;
        }
    }

    fn rare(&self) -> &NodeRare {
        self.rare.get_or_init(|| Box::new(NodeRare::default()))
    }

    fn rare_mut(&mut self) -> &mut NodeRare {
        self.rare.get_or_init(|| Box::new(NodeRare::default()));
        self.rare.get_mut().expect("rare fields are initialized above")
    }

    pub fn assigned_slot(&self) -> Option<NodeId> {
        self.rare
            .get()
            .and_then(|rare| rare.assigned_slot.get().get())
    }

    pub fn set_assigned_slot(&self, id: Option<NodeId>) {
        if id.is_some() || self.rare.get().is_some() {
            self.rare().assigned_slot.set(id.into());
        }
    }

    pub fn assigned_nodes(&self) -> Option<&ThinVec<NodeId>> {
        self.rare
            .get()
            .and_then(|rare| rare.assigned_nodes.as_ref())
    }

    pub fn take_assigned_nodes(&mut self) -> Option<ThinVec<NodeId>> {
        self.rare
            .get_mut()
            .and_then(|rare| rare.assigned_nodes.take())
    }

    pub fn set_assigned_nodes(&mut self, nodes: Option<ThinVec<NodeId>>) {
        if nodes.is_some() || self.rare.get().is_some() {
            self.rare_mut().assigned_nodes = nodes;
        }
    }

    pub fn assigned_nodes_mut(&mut self) -> &mut ThinVec<NodeId> {
        self.rare_mut().assigned_nodes.get_or_insert_with(ThinVec::new)
    }

    /// Anonymous block boxes this node owns (see `NodeRare::anonymous_blocks`).
    pub fn anonymous_blocks(&self) -> &[NodeId] {
        self.rare.get().map_or(&[], |rare| &rare.anonymous_blocks)
    }

    pub fn take_anonymous_blocks(&mut self) -> ThinVec<NodeId> {
        self.rare
            .get_mut()
            .map_or_else(ThinVec::new, |rare| std::mem::take(&mut rare.anonymous_blocks))
    }

    pub fn set_anonymous_blocks(&mut self, blocks: ThinVec<NodeId>) {
        if !blocks.is_empty() || self.rare.get().is_some() {
            self.rare_mut().anonymous_blocks = blocks;
        }
    }

    pub fn paint_parent_override(&self) -> Option<NodeId> {
        self.rare
            .get()
            .and_then(|rare| rare.paint_parent_override.get().get())
    }

    pub fn set_paint_parent_override(&self, id: Option<NodeId>) {
        if id.is_some() || self.rare.get().is_some() {
            self.rare().paint_parent_override.set(id.into());
        }
    }

    pub fn rehomed_paint_children(&self) -> Option<Ref<'_, ThinVec<NodeId>>> {
        self.rare
            .get()
            .map(|rare| rare.rehomed_paint_children.borrow())
    }

    pub fn rehomed_paint_children_mut(&self) -> RefMut<'_, ThinVec<NodeId>> {
        self.rare().rehomed_paint_children.borrow_mut()
    }

    pub fn clear_rehomed_paint_children(&self) {
        if let Some(rare) = self.rare.get() {
            rare.rehomed_paint_children.borrow_mut().clear();
        }
    }

    pub fn hoisted_children(&self) -> Option<Ref<'_, ThinVec<NodeId>>> {
        self.rare.get().map(|rare| rare.hoisted_children.borrow())
    }

    pub fn hoisted_children_mut(&self) -> RefMut<'_, ThinVec<NodeId>> {
        self.rare().hoisted_children.borrow_mut()
    }

    pub fn clear_hoisted_children(&self) {
        if let Some(rare) = self.rare.get() {
            rare.hoisted_children.borrow_mut().clear();
        }
    }

    pub fn take_hoisted_children(&self) -> ThinVec<NodeId> {
        self.rare.get().map_or_else(ThinVec::new, |rare| {
            std::mem::take(&mut *rare.hoisted_children.borrow_mut())
        })
    }

    pub fn set_hoisted_children(&self, children: ThinVec<NodeId>) {
        if !children.is_empty() || self.rare.get().is_some() {
            *self.rare().hoisted_children.borrow_mut() = children;
        }
    }

    pub fn oof_containing_block(&self) -> Option<NodeId> {
        self.rare
            .get()
            .and_then(|rare| rare.oof_containing_block.get())
    }

    pub fn set_oof_containing_block(&self, id: Option<NodeId>) {
        if id.is_some() || self.rare.get().is_some() {
            self.rare().oof_containing_block.set(id);
        }
    }

    pub fn placed_floats(&self) -> bool {
        self.rare.get().is_some_and(|rare| rare.placed_floats.get())
    }

    pub fn set_placed_floats(&self, value: bool) {
        if value || self.rare.get().is_some() {
            self.rare().placed_floats.set(value);
        }
    }

    pub fn rehomed_reach(&self) -> Option<(f32, f32, f32, f32)> {
        self.rare
            .get()
            .and_then(|rare| rare.rehomed_reach.borrow().as_deref().copied())
    }

    pub fn set_rehomed_reach(&self, rect: Option<(f32, f32, f32, f32)>) {
        if rect.is_some() || self.rare.get().is_some() {
            let mut stored = self.rare().rehomed_reach.borrow_mut();
            if let Some(rect) = rect {
                **stored.get_or_insert_with(|| Box::new(rect)) = rect;
            } else {
                *stored = None;
            }
        }
    }

    pub fn take_stacking_context(&self) -> Option<Box<StackingContext>> {
        self.rare
            .get()
            .and_then(|rare| rare.stacking_context.borrow_mut().take())
    }

    pub fn set_stacking_context(&self, value: Option<Box<StackingContext>>) {
        if value.is_some() || self.rare.get().is_some() {
            *self.rare().stacking_context.borrow_mut() = value;
        }
    }

    pub fn stacking_context(&self) -> Option<Ref<'_, StackingContext>> {
        let rare = self.rare.get()?;
        Ref::filter_map(rare.stacking_context.borrow(), |value| value.as_deref()).ok()
    }

    pub fn take_sc_contribution_cache(&self) -> ThinVec<HoistedPaintChild> {
        self.rare.get().map_or_else(ThinVec::new, |rare| {
            std::mem::take(&mut *rare.sc_contribution_cache.borrow_mut())
        })
    }

    pub fn set_sc_contribution_cache(&self, entries: ThinVec<HoistedPaintChild>) {
        if !entries.is_empty() || self.rare.get().is_some() {
            *self.rare().sc_contribution_cache.borrow_mut() = entries;
        }
    }

    pub fn sc_contribution_cache(&self) -> Option<Ref<'_, ThinVec<HoistedPaintChild>>> {
        self.rare
            .get()
            .map(|rare| rare.sc_contribution_cache.borrow())
    }

    pub fn clear_sc_contribution_cache(&self) {
        if let Some(rare) = self.rare.get() {
            rare.sc_contribution_cache.borrow_mut().clear();
        }
    }

    pub fn transform(&self) -> Option<Affine> {
        match &self.data {
            NodeData::Element(data) | NodeData::AnonymousBlock(data) => data.transform(),
            NodeData::Document(data) => data.transform.as_deref().copied(),
            _ => panic!("`transform` is not available on this node kind"),
        }
    }

    /// This node's layout output state, or a shared default if layout has
    /// never written to this node.
    ///
    /// Panics for node kinds which do not participate in layout (text and
    /// comment nodes).
    #[inline]
    pub fn layout_data(&self) -> &LayoutData {
        match &self.data {
            NodeData::Element(data) | NodeData::AnonymousBlock(data) => data.layout_data(),
            NodeData::Document(data) => data.layout_data(),
            _ => panic!("`layout_data` is not available on this node kind"),
        }
    }

    /// Mutable access to this node's layout output state, allocating it if it
    /// does not yet exist.
    ///
    /// Panics for node kinds which do not participate in layout (text and
    /// comment nodes).
    #[inline]
    pub fn layout_data_mut(&mut self) -> &mut LayoutData {
        match &mut self.data {
            NodeData::Element(data) | NodeData::AnonymousBlock(data) => data.layout_data_mut(),
            NodeData::Document(data) => data.layout_data_mut(),
            _ => panic!("`layout_data` is not available on this node kind"),
        }
    }

    /// Mutable access to this node's layout output state, if it has been
    /// allocated. Never allocates. Returns `None` for node kinds which do not
    /// participate in layout.
    #[inline]
    pub fn try_layout_data_mut(&mut self) -> Option<&mut LayoutData> {
        match &mut self.data {
            NodeData::Element(data) | NodeData::AnonymousBlock(data) => {
                data.layout_data.get_mut().map(|data| &mut **data)
            }
            NodeData::Document(data) => data.layout_data.get_mut().map(|data| &mut **data),
            _ => None,
        }
    }

    /// Clear this node's taffy layout cache without allocating `LayoutData`
    /// for nodes that have never been laid out.
    #[inline]
    pub fn clear_layout_cache(&mut self) {
        if let Some(layout_data) = self.try_layout_data_mut() {
            layout_data.cache.clear();
        }
    }

    /// Clear this node's taffy layout cache and any cached inline
    /// `content_widths`, forcing a full relayout of it on the next pass.
    pub fn invalidate_layout_cache(&mut self) {
        self.clear_layout_cache();
        if let Some(inline_layout) = self
            .data
            .downcast_element_mut()
            .and_then(|el| el.inline_layout_data_mut())
        {
            inline_layout.content_widths = None;
        }
    }

    #[inline]
    pub fn cache(&self) -> &Cache {
        &self.layout_data().cache
    }

    #[inline]
    pub fn cache_mut(&mut self) -> &mut Cache {
        &mut self.layout_data_mut().cache
    }

    #[inline]
    pub fn unrounded_layout(&self) -> Layout {
        self.layout_data().unrounded_layout()
    }

    pub fn set_unrounded_layout(&mut self, layout: &Layout) {
        self.layout_data_mut().set_unrounded_layout(layout);
    }

    #[inline]
    pub fn final_layout(&self) -> Layout {
        self.layout_data().final_layout()
    }

    pub fn set_final_layout(&mut self, layout: &Layout) {
        self.layout_data_mut().set_final_layout(layout);
    }

    /// The post-layout shift baked into `final_layout().location`
    /// (see `set_pin_offset`).
    #[inline]
    pub fn pin_offset(&self) -> &taffy::Point<f32> {
        self.layout_data().pin_offset()
    }

    /// Reconcile the baked post-layout shift to `(x, y)`, moving the location
    /// by the delta. Idempotent: re-running a pass is a no-op.
    ///
    /// Callers (sticky positioning) state the shift they want in absolute
    /// terms and never track what is already applied: `pin_offset` is the
    /// single record, zeroed whenever layout rewrites the location wholesale.
    /// Returns whether the box moved.
    pub fn set_pin_offset(&mut self, x: f32, y: f32) -> bool {
        let layout_data = self.layout_data_mut();
        let dx = x - layout_data.pin_offset().x;
        let dy = y - layout_data.pin_offset().y;
        if dx == 0.0 && dy == 0.0 {
            return false;
        }
        layout_data.shift_final_location(dx, dy);
        layout_data.set_pin_offset(taffy::Point { x, y });
        true
    }

    #[inline]
    pub fn scroll_offset(&self) -> &crate::Point<f64> {
        self.layout_data().scroll_offset()
    }

    #[inline]
    pub fn scroll_offset_mut(&mut self) -> &mut crate::Point<f64> {
        self.layout_data_mut().scroll_offset_mut()
    }

    pub fn set_scroll_offset(&mut self, offset: crate::Point<f64>) {
        let previous = self.scroll_offset();
        if previous.x.to_bits() != offset.x.to_bits()
            || previous.y.to_bits() != offset.y.to_bits()
        {
            *self.scroll_offset_mut() = offset;
        }
    }

    #[inline]
    pub fn scrollable_overflow(&self) -> &KurboRect {
        &self.layout_data().scrollable_overflow
    }

    #[inline]
    pub fn scrollable_overflow_mut(&mut self) -> &mut KurboRect {
        &mut self.layout_data_mut().scrollable_overflow
    }

    /// Style data from stylo, if this node kind carries it (element or document
    /// nodes). Returns `None` for text/comment nodes.
    #[inline]
    pub fn try_stylo_element_data(&self) -> Option<StyloData<'_>> {
        match &self.data {
            NodeData::Element(data) | NodeData::AnonymousBlock(data) => Some(data.stylo_data()),
            NodeData::Document(data) => Some(data.stylo_data()),
            _ => None,
        }
    }

    #[inline]
    pub fn try_stylo_element_data_mut(&mut self) -> Option<StyloDataMut<'_>> {
        match &mut self.data {
            NodeData::Element(data) | NodeData::AnonymousBlock(data) => {
                Some(data.stylo_data_mut())
            }
            NodeData::Document(data) => Some(data.stylo_data_mut()),
            _ => None,
        }
    }

    /// Style data from stylo. Panics on node kinds without it (see
    /// [`Self::try_stylo_element_data`]).
    #[inline]
    pub fn stylo_element_data(&self) -> StyloData<'_> {
        self.try_stylo_element_data()
            .expect("`stylo_element_data` is not available on this node kind")
    }

    #[inline]
    pub fn stylo_element_data_mut(&mut self) -> StyloDataMut<'_> {
        self.try_stylo_element_data_mut()
            .expect("`stylo_element_data` is not available on this node kind")
    }

    /// The state flags, if this node kind carries them (element or document
    /// nodes). Returns `None` for text/comment nodes.
    #[inline]
    fn state_flags(&self) -> Option<&NodeStateFlags> {
        match &self.data {
            NodeData::Element(data) | NodeData::AnonymousBlock(data) => Some(&data.state_flags),
            NodeData::Document(data) => Some(&data.state_flags),
            _ => None,
        }
    }

    /// Whether Stylo has processed this node's snapshot. Panics on node kinds
    /// that cannot be snapshotted.
    #[inline]
    pub fn snapshot_handled(&self) -> bool {
        self.state_flags()
            .expect("`snapshot_handled` is not available on this node kind")
            .snapshot_handled()
    }

    #[inline]
    pub fn set_snapshot_handled(&self, value: bool) {
        self.state_flags()
            .expect("`snapshot_handled` is not available on this node kind")
            .set_snapshot_handled(value);
    }

    /// The document's shared style lock. Only available on element and
    /// document nodes.
    #[inline]
    pub fn guard(&self) -> &SharedRwLock {
        match &self.data {
            NodeData::Element(_) | NodeData::AnonymousBlock(_) | NodeData::Document(_) => {
                &self.tree().guard
            }
            _ => panic!("`guard` is not available on this node kind"),
        }
    }

    #[inline]
    pub fn has_snapshot(&self) -> bool {
        match &self.data {
            NodeData::Element(data) | NodeData::AnonymousBlock(data) => data.has_snapshot(),
            NodeData::Document(data) => data.has_snapshot,
            _ => false,
        }
    }

    #[inline]
    pub fn set_has_snapshot(&mut self, value: bool) {
        match &mut self.data {
            NodeData::Element(data) | NodeData::AnonymousBlock(data) => {
                data.set_has_snapshot(value)
            }
            NodeData::Document(data) => data.has_snapshot = value,
            _ => {}
        }
    }

    #[inline]
    pub fn before(&self) -> Option<NodeId> {
        self.element_data().and_then(|data| data.before())
    }

    #[inline]
    pub fn after(&self) -> Option<NodeId> {
        self.element_data().and_then(|data| data.after())
    }
}

impl Node {
    pub(crate) fn new(tree: *mut crate::NodeTree, id: NodeId, data: NodeData) -> Self {
        Self {
            tree,

            links: Cell::new(LAYOUT_NONE),
            children: super::child_list::ChildList::new(),
            rare: OnceCell::new(),

            flags: {
                let index = slot_index(id);
                assert!(index <= SLOT_INDEX_MASK, "node slot index {index} out of range");
                NodeFlags::from_bits_retain(index)
            },
            data,
        }
    }

    /// Our id. Only meaningful while the node is live in its tree: a node taken out by a
    /// removal reads its freed slot's version as 0, so keep the id it was removed by instead.
    #[inline]
    pub fn id(&self) -> NodeId {
        // SAFETY: `tree` is the node's own tree pointer (see `version_at`).
        let version = unsafe { crate::tree::version_at(self.tree, self.flags.slot_index()) };
        crate::tree::id_from_parts(self.flags.slot_index(), version)
    }

    /// The slot index to store for a link to `id`, 0 for none.
    #[inline]
    fn link(&self, id: Option<NodeId>) -> u32 {
        id.map_or(0, |id| {
            let index = link_index(self.tree, id);
            assert!(index != 0 && index <= SLOT_INDEX_MASK, "invalid node link slot {index}");
            index
        })
    }

    #[inline]
    fn resolve(&self, index: u32) -> Option<NodeId> {
        if index == 0 {
            return None;
        }
        // A link names a slot, not a generation. When the node it named has since been freed the
        // link resolves to nothing: callers already read a missing parent as "no parent", and
        // resolving it would name whatever occupies the slot next.
        // SAFETY: `tree` is the node's own tree pointer (see `version_at`).
        let version = unsafe { crate::tree::version_at(self.tree, index) };
        (version != 0).then(|| crate::tree::id_from_parts(index, version))
    }

    #[inline(always)]
    fn parent_index(&self) -> u32 {
        self.links.get() & SLOT_INDEX_MASK
    }

    fn layout_parent_index(&self) -> u32 {
        let links = self.links.get();
        match links & LAYOUT_KIND_MASK {
            LAYOUT_NONE => 0,
            LAYOUT_SAME => links & SLOT_INDEX_MASK,
            // SAFETY: `tree` is the node's own tree pointer (see `version_at`).
            _ => unsafe { crate::tree::layout_parents(self.tree) }
                .borrow()
                .get(&self.flags.slot_index())
                .copied()
                .unwrap_or(0),
        }
    }

    /// Stores both links, keeping the side table entry exactly while the layout parent is
    /// another node than the parent.
    fn set_links(&self, parent: u32, layout_parent: u32) {
        let kind = match layout_parent {
            0 => LAYOUT_NONE,
            _ if layout_parent == parent => LAYOUT_SAME,
            _ => LAYOUT_OTHER,
        };
        let was_other = self.links.get() & LAYOUT_KIND_MASK == LAYOUT_OTHER;
        if kind == LAYOUT_OTHER || was_other {
            // SAFETY: `tree` is the node's own tree pointer (see `version_at`).
            let mut table = unsafe { crate::tree::layout_parents(self.tree) }.borrow_mut();
            let own = self.flags.slot_index();
            if kind == LAYOUT_OTHER {
                table.insert(own, layout_parent);
            } else {
                table.remove(&own);
            }
        }
        self.links.set(parent | kind);
    }

    /// Whether this node's layout parent is in the tree's side table; if so, the node now reads
    /// none, and the caller removes the entry. For a node leaving its tree.
    pub(crate) fn forget_side_table_layout_parent(&mut self) -> bool {
        let links = self.links.get_mut();
        let was_other = *links & LAYOUT_KIND_MASK == LAYOUT_OTHER;
        if was_other {
            *links &= !LAYOUT_KIND_MASK;
        }
        was_other
    }

    /// Our parent's ID.
    #[inline]
    pub fn parent_id(&self) -> Option<NodeId> {
        self.resolve(self.parent_index())
    }

    pub fn set_parent_id(&mut self, id: Option<NodeId>) {
        let parent = self.link(id);
        if parent != self.parent_index() {
            self.set_links(parent, self.layout_parent_index());
        }
    }

    pub fn take_parent_id(&mut self) -> Option<NodeId> {
        let parent = self.parent_id();
        self.set_parent_id(None);
        parent
    }

    /// Our parent in the layout hierarchy, which includes anonymous blocks.
    #[inline]
    pub fn layout_parent_id(&self) -> Option<NodeId> {
        self.resolve(self.layout_parent_index())
    }

    pub fn set_layout_parent_id(&self, id: Option<NodeId>) {
        self.set_links(self.parent_index(), self.link(id));
    }

    pub fn replace_layout_parent_id(&self, id: Option<NodeId>) -> Option<NodeId> {
        let previous = self.layout_parent_id();
        self.set_layout_parent_id(id);
        previous
    }

    pub fn take_layout_parent_id(&self) -> Option<NodeId> {
        self.replace_layout_parent_id(None)
    }

    /// Our DOM children.
    #[inline]
    pub fn children(&self) -> ChildIds<'_> {
        ChildIds::of(self.tree, &self.children)
    }

    pub fn push_child(&mut self, id: NodeId) {
        let index = link_index(self.tree, id);
        self.children.push(index);
    }

    pub fn insert_child(&mut self, i: usize, id: NodeId) {
        let index = link_index(self.tree, id);
        self.children.insert(i, index);
    }

    /// Inserts `ids` as one run starting at child index `i`.
    pub fn insert_children(&mut self, i: usize, ids: impl IntoIterator<Item = NodeId>) {
        let tree = self.tree;
        self.children.edit(|children| {
            children.splice(i..i, ids.into_iter().map(|id| link_index(tree, id)));
        });
    }

    pub fn remove_child_at(&mut self, i: usize) -> NodeId {
        link_id(self.tree, self.children.remove(i))
    }

    /// Removes `id` from the child list. Matches by slot index alone, so it also
    /// unlinks a child that was already dropped (its slot is then free).
    pub fn remove_child(&mut self, id: NodeId) {
        let index = slot_index(id);
        self.children.retain(|&child| child != index);
    }

    /// Keeps the children for which `keep` holds; every child must still be live.
    pub fn retain_children(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        let tree = self.tree;
        self.children.retain(|&index| keep(link_id(tree, index)));
    }

    pub fn clear_children(&mut self) {
        self.children.clear();
    }

    pub fn extend_children(&mut self, ids: impl IntoIterator<Item = NodeId>) {
        let tree = self.tree;
        let mut ids = ids.into_iter().peekable();
        if self.children.len() == 0 {
            // A lone new child goes inline without a list allocated and freed for it.
            if let Some(first) = ids.next() {
                self.children.push(link_index(tree, first));
            }
            if ids.peek().is_none() {
                return;
            }
        }
        self.children.edit(|children| {
            children.extend(ids.map(|id| link_index(tree, id)));
        });
    }

    pub fn set_children(&mut self, ids: impl IntoIterator<Item = NodeId>) {
        self.children.clear();
        self.extend_children(ids);
    }

    pub fn take_children(&mut self) -> Vec<NodeId> {
        let ids = self.children().to_vec();
        self.children.clear();
        ids
    }

    pub fn shrink_children_to_fit(&mut self) {
        self.children.shrink_to_fit();
    }

    /// Moves the child list out, without copying, for a walk that mutates the document.
    pub(crate) fn take_detached_children(&mut self) -> DetachedChildren {
        DetachedChildren {
            tree: self.tree,
            slots: std::mem::take(&mut self.children),
        }
    }

    /// Puts back a list from `take_detached_children`, replacing the current one.
    pub(crate) fn restore_detached_children(&mut self, children: DetachedChildren) {
        debug_assert!(std::ptr::eq(children.tree, self.tree));
        self.children = children.slots;
    }

    pub fn set_transform(&mut self, scale: f32) -> Option<Affine> {
        let transform = self.primary_styles().and_then(|s| {
            let size = self.final_layout().size;
            let reference_box = Rect::new(
                Point2D::new(CSSPixelLength::new(0.0), CSSPixelLength::new(0.0)),
                Size2D::new(
                    CSSPixelLength::new(size.width),
                    CSSPixelLength::new(size.height),
                ),
            );
            // Resolve the transform in CSS pixels, then convert it to device-pixel space
            // (S * T * S^-1): translation components are scaled, linear components are not.
            crate::resolve_2d_transform(s.get_box(), reference_box).map(|t| {
                let scale = scale as f64;
                let [m11, m12, m21, m22, m41, m42] = t.as_coeffs();
                Affine::new([m11, m12, m21, m22, m41 * scale, m42 * scale])
            })
        });

        match &mut self.data {
            NodeData::Element(data) | NodeData::AnonymousBlock(data) => data.set_transform(transform),
            NodeData::Document(data) => super::element::store_transform(&mut data.transform, transform),
            _ => panic!("`transform` is not available on this node kind"),
        }
        transform
    }

    pub fn pe_by_index(&self, index: usize) -> Option<NodeId> {
        match index {
            0 => self.after(),
            1 => self.before(),
            _ => panic!("Invalid pseudo element index"),
        }
    }

    pub fn set_pe_by_index(&mut self, index: usize, value: Option<NodeId>) {
        let Some(data) = self.element_data_mut() else {
            return;
        };
        match index {
            0 => data.set_after(value),
            1 => data.set_before(value),
            _ => panic!("Invalid pseudo element index"),
        }
    }

    pub(crate) fn display_style(&self) -> Option<StyloDisplay> {
        let display = self.primary_styles().as_ref()?.clone_display();
        Some(
            if self.element_data().is_some_and(|el| el.is_button_input()) {
                stylo_taffy::convert::button_input_display(display)
            } else {
                display
            },
        )
    }

    pub fn is_or_contains_block(&self) -> bool {
        let style = self.primary_styles();
        let style = style.as_ref();

        // Ignore out-of-flow items
        let position = style
            .map(|s| s.clone_position())
            .unwrap_or(Position::Relative);
        let is_in_flow = matches!(
            position,
            Position::Static | Position::Relative | Position::Sticky
        );
        if !is_in_flow {
            return false;
        }
        // Floated boxes do not break up the inline flow: they participate in the
        // inline formatting context as out-of-flow inline boxes
        let is_floating = style
            .map(|s| s.clone_float().is_floating())
            .unwrap_or(false);
        if is_floating {
            return false;
        }
        let display = self.display_style().unwrap_or(StyloDisplay::inline());
        match display.outside() {
            DisplayOutside::None if display.inside() != DisplayInside::Contents => false,
            DisplayOutside::Block => true,
            _ => {
                if matches!(display.inside(), DisplayInside::Flow | DisplayInside::Contents) {
                    self.flat_children()
                        .iter()
                        .any(|child_id| self.tree()[child_id].is_or_contains_block())
                } else {
                    false
                }
            }
        }
    }

    pub fn is_whitespace_node(&self) -> bool {
        match &self.data {
            NodeData::Text(data) => data.content.chars().all(|c| c.is_ascii_whitespace()),
            _ => false,
        }
    }

    /// Whether this is an empty or whitespace-only text node whose whitespace may be collapsed
    /// away under the `white-space-collapse` it inherits from its parent.
    pub fn is_collapsible_whitespace_node(&self) -> bool {
        let NodeData::Text(data) = &self.data else {
            return false;
        };
        if data.content.is_empty() {
            return true;
        }
        if !data.content.chars().all(|c| c.is_ascii_whitespace()) {
            return false;
        }
        let white_space_collapse = self
            .parent_id()
            .and_then(|parent_id| self.with(parent_id).primary_styles())
            .map(|style| style.clone_white_space_collapse());
        match white_space_collapse {
            Some(WhiteSpaceCollapse::Preserve | WhiteSpaceCollapse::BreakSpaces) => false,
            Some(WhiteSpaceCollapse::PreserveBreaks) => !data.content.contains('\n'),
            Some(WhiteSpaceCollapse::Collapse) | None => true,
        }
    }

    pub fn is_focussable(&self) -> bool {
        self.data
            .downcast_element()
            .map(|el| el.is_focussable())
            .unwrap_or(false)
    }

    pub fn set_restyle_hint(&mut self, hint: RestyleHint) {
        if let Some(stylo_element_data) = self.try_stylo_element_data_mut() {
            if let Some(mut element_data) = stylo_element_data.get_mut() {
                element_data.hint.insert(hint);
            }
        }
        // Mark all ancestors as having dirty descendants so the style traversal
        // will visit this node's subtree
        self.mark_ancestors_dirty();
    }

    /// Returns whether this node has any descendants that need restyling.
    pub fn has_dirty_descendants(&self) -> bool {
        self.state_flags()
            .is_some_and(|flags| flags.dirty_descendants())
    }

    /// Sets the dirty_descendants flag on this node.
    pub fn set_dirty_descendants(&self) {
        if let Some(flags) = self.state_flags() {
            flags.set_dirty_descendants(true);
        }
    }

    /// Clears the dirty_descendants flag on this node.
    pub fn unset_dirty_descendants(&self) {
        if let Some(flags) = self.state_flags() {
            flags.set_dirty_descendants(false);
        }
    }

    /// Marks all ancestors of this node as having dirty descendants.
    /// This propagates the dirty flag up the tree so that the style traversal
    /// knows to visit the subtree containing this node.
    pub fn mark_ancestors_dirty(&self) {
        let mut current_id = self.flat_tree_parent();
        while let Some(parent_id) = current_id {
            let parent = &self.tree()[parent_id];
            // If this ancestor already has dirty_descendants set, we can stop
            // because all further ancestors must also have it set
            if let Some(flags) = parent.state_flags() {
                if flags.swap_set_dirty_descendants() {
                    break;
                }
            }
            current_id = parent.flat_tree_parent();
        }
    }

    /// Style, damage and construct walks all follow the flat tree: a slotted
    /// node sits under its slot, and a shadow root's content under the host.
    pub(crate) fn flat_tree_parent(&self) -> Option<NodeId> {
        if let Some(slot_id) = self.assigned_slot() {
            return Some(slot_id);
        }
        if let Some(host) = self.shadow_root_data().map(|data| data.host) {
            return Some(host);
        }
        self.parent_id()
    }

    /// Returns whether this node or any of its descendants may carry damage.
    pub fn has_damaged_descendants(&self) -> bool {
        self.state_flags()
            .is_some_and(|flags| flags.damaged_descendants())
    }

    /// Clears the damaged_descendants flag on this node.
    pub fn unset_damaged_descendants(&self) {
        if let Some(flags) = self.state_flags() {
            flags.set_damaged_descendants(false);
        }
    }

    /// Marks this node and all of its ancestors as (potentially) carrying
    /// damage, so that the damage propagation pass visits this node's subtree.
    ///
    /// The invariant is: if a node carries damage (or needs damage-phase
    /// processing such as pseudo-element style syncing), then it and all of
    /// its ancestors have `damaged_descendants` set.
    pub fn mark_damaged(&self) {
        if let Some(flags) = self.state_flags() {
            if flags.swap_set_damaged_descendants() {
                return;
            }
        }
        let mut current_id = self.flat_tree_parent();
        while let Some(parent_id) = current_id {
            let parent = &self.tree()[parent_id];
            // If this ancestor already has damaged_descendants set, we can stop
            // because all further ancestors must also have it set
            if let Some(flags) = parent.state_flags() {
                if flags.swap_set_damaged_descendants() {
                    break;
                }
            }
            current_id = parent.flat_tree_parent();
        }
    }

    // pub fn damage_mut(&mut self) -> Option<&mut RestyleDamage> {
    //     self.stylo_element_data
    //         .get_mut()
    //         .map(|mut data: ElementDataMut<'a>| &'a mut data.damage)
    // }

    pub fn damage(&self) -> Option<RestyleDamage> {
        self.try_stylo_element_data()
            .and_then(|stylo| stylo.get().map(|data| data.damage))
    }

    pub fn set_damage(&mut self, damage: RestyleDamage) {
        if let Some(stylo) = self.try_stylo_element_data_mut() {
            if let Some(mut data) = stylo.get_mut() {
                data.damage = damage;
            }
        }
    }

    pub fn insert_damage(&mut self, damage: RestyleDamage) {
        if let Some(stylo) = self.try_stylo_element_data_mut() {
            if let Some(mut data) = stylo.get_mut() {
                data.damage |= damage;
            }
        }
        if !damage.is_empty() {
            self.mark_damaged();
        }
    }

    pub fn remove_damage(&mut self, damage: RestyleDamage) {
        if let Some(stylo) = self.try_stylo_element_data_mut() {
            if let Some(mut data) = stylo.get_mut() {
                data.damage.remove(damage);
            }
        }
    }

    pub fn clear_damage_mut(&mut self) {
        if let Some(stylo) = self.try_stylo_element_data_mut() {
            if let Some(mut data) = stylo.get_mut() {
                data.damage = RestyleDamage::empty();
            }
        }
    }

    // State changes (hover/focus/active/disabled) do not set a restyle hint.
    // Invalidation is driven by element snapshots: the style traversal diffs the
    // snapshotted (pre-change) state against the current state and invalidates
    // only the elements matched by selectors that depend on the changed state
    // bits. Ancestors are marked dirty so the traversal reaches this node.
    pub fn hover(&mut self) {
        if let Some(data) = self.element_data_mut() {
            data.element_state.insert(ElementState::HOVER);
        }
        self.mark_ancestors_dirty();
    }

    pub fn unhover(&mut self) {
        if let Some(data) = self.element_data_mut() {
            data.element_state.remove(ElementState::HOVER);
        }
        self.mark_ancestors_dirty();
    }

    pub fn is_hovered(&self) -> bool {
        self.element_data()
            .is_some_and(|data| data.element_state.contains(ElementState::HOVER))
    }

    pub fn focus(&mut self, shell_provider: Arc<dyn ShellProvider>) {
        if let Some(data) = self.element_data_mut() {
            data.element_state
                .insert(ElementState::FOCUS | ElementState::FOCUSRING);
        }
        self.mark_ancestors_dirty();

        // If focussing a text input, enable IME and set IME area
        if self
            .element_data()
            .and_then(|elem| elem.text_input_data())
            .is_some()
        {
            shell_provider.set_ime_enabled(true);
            let mut pos = self.absolute_position(0.0, 0.0);
            pos.x += self.final_layout().content_box_x();
            pos.y += self.final_layout().content_box_y();
            let width = self.final_layout().content_box_width();
            let height = self.final_layout().content_box_height();
            shell_provider.set_ime_cursor_area(pos.x, pos.y, width, height);
        }
    }

    pub fn blur(&mut self, shell_provider: Arc<dyn ShellProvider>) {
        if let Some(data) = self.element_data_mut() {
            data.element_state
                .remove(ElementState::FOCUS | ElementState::FOCUSRING);
        }
        self.mark_ancestors_dirty();

        // If blurring a text input, disable IME
        if self
            .element_data()
            .and_then(|elem| elem.text_input_data())
            .is_some()
        {
            shell_provider.set_ime_enabled(false);
        }
    }

    pub fn is_focussed(&self) -> bool {
        self.element_data()
            .is_some_and(|data| data.element_state.contains(ElementState::FOCUS))
    }

    pub fn active(&mut self) {
        if let Some(data) = self.element_data_mut() {
            data.element_state.insert(ElementState::ACTIVE);
        }
        self.mark_ancestors_dirty();
    }

    pub fn unactive(&mut self) {
        if let Some(data) = self.element_data_mut() {
            data.element_state.remove(ElementState::ACTIVE);
        }
        self.mark_ancestors_dirty();
    }

    pub fn is_active(&self) -> bool {
        self.element_data()
            .is_some_and(|data| data.element_state.contains(ElementState::ACTIVE))
    }

    // Marks the node as disabled if it can be.
    // It does not disable any children which should be disabled as well (relevant for the `select` element).
    pub fn disable(&mut self) {
        if let Some(data) = self.element_data_mut() {
            if data.can_be_disabled() {
                data.element_state.insert(ElementState::DISABLED);
                data.element_state.remove(ElementState::ENABLED);
            }
        }
        self.mark_ancestors_dirty();
    }

    // Marks the node as enabled if it can be.
    // It does not enable any children which should be enabled as well (relevant for the `select` element).
    pub fn enable(&mut self) {
        if let Some(data) = self.element_data_mut() {
            if data.can_be_disabled() {
                data.element_state.insert(ElementState::ENABLED);
                data.element_state.remove(ElementState::DISABLED);
            }
        }
        self.mark_ancestors_dirty();
    }

    pub fn subdoc(&self) -> Option<&dyn Document> {
        self.element_data().and_then(|el| el.sub_doc_data())
    }

    pub fn subdoc_mut(&mut self) -> Option<&mut dyn Document> {
        self.element_data_mut().and_then(|el| el.sub_doc_data_mut())
    }

    pub fn text_input_v_centering_offset(&self, scale: f64) -> f64 {
        // For single-line inputs, add an offset to vertically center the text input layout
        // within the content box of it's node.
        if let Some(input_data) = self
            .data
            .downcast_element()
            .and_then(|el| el.text_input_data())
        {
            if !input_data.is_multiline {
                let content_box_height = self.final_layout().content_box_height();
                let input_height = input_data.editor.try_layout().unwrap().height() / scale as f32;
                let y_offset = ((content_box_height - input_height) / 2.0).max(0.0);

                return y_offset as f64;
            }
        }

        0.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NodeKind {
    Document,
    Element,
    AnonymousBlock,
    Text,
    Comment,
    ShadowRoot,
}

/// Encapsulation mode of a shadow root, mirroring the
/// DOM spec's ShadowRootMode (open/closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowRootMode {
    Open,
    Closed,
}

// Shadow roots live in the slab OUT of the host's child list (the
// ElementData::template_contents pattern), so light walks skip them.

/// Root of a shadow tree; the host element links here via
/// ElementData::shadow_root and `host` points back at it.
pub struct ShadowRootData {
    pub host: NodeId,
    pub mode: ShadowRootMode,
    /// Scoped author stylesheets: rules from this shadow tree's own
    /// <style> elements, consulted via TShadowRoot::style_data.
    pub author_styles: AuthorStyles<DocumentStyleSheet>,
}

impl ShadowRootData {
    pub fn new(host: NodeId, mode: ShadowRootMode) -> Self {
        Self {
            host,
            mode,
            author_styles: AuthorStyles::new(),
        }
    }
}

// Manual impls: AuthorStyles is neither Clone nor Debug. A cloned
// shadow root starts with empty styles (its <style> nodes re-register).
impl Clone for ShadowRootData {
    fn clone(&self) -> Self {
        Self::new(self.host, self.mode)
    }
}

impl std::fmt::Debug for ShadowRootData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShadowRootData")
            .field("host", &self.host)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

/// The different kinds of nodes in the DOM.
#[derive(Debug, Clone)]
pub enum NodeData {
    /// The `Document` itself - the root node of a HTML document.
    Document(Box<DocumentData>),

    /// An element with attributes.
    Element(PoolBox<ElementData>),

    /// An anonymous block box
    AnonymousBlock(PoolBox<ElementData>),

    /// A text node.
    Text(TextNodeData),

    /// A comment.
    Comment {
        /// The textual content of the comment. Boxed so `NodeData` fits in
        /// the text variant's 12 bytes on wasm32, which shrinks every node.
        contents: Box<String>,
    },

    /// The root of a shadow tree attached to a host element.
    ShadowRoot(Box<ShadowRootData>),
    // /// A `DOCTYPE` with name, public id, and system id. See
    // /// [document type declaration on wikipedia][https://en.wikipedia.org/wiki/Document_type_declaration]
    // Doctype { name: String, public_id: String, system_id: String },

    // /// A Processing instruction.
    // ProcessingInstruction { target: String, contents: String },
}

// One node slot per DOM node: keep the boxed comment variant from widening it.
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<NodeData>() == 12);
// The node's id lives in the tree's slot index and version table, not in each node.
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<Node>() <= 32);

impl NodeData {
    pub fn downcast_element(&self) -> Option<&ElementData> {
        match self {
            Self::Element(data) => Some(data),
            Self::AnonymousBlock(data) => Some(data),
            _ => None,
        }
    }

    pub fn downcast_element_mut(&mut self) -> Option<&mut ElementData> {
        match self {
            Self::Element(data) => Some(data),
            Self::AnonymousBlock(data) => Some(data),
            _ => None,
        }
    }

    pub fn is_element_with_tag_name(&self, name: &impl PartialEq<LocalName>) -> bool {
        let Some(elem) = self.downcast_element() else {
            return false;
        };
        *name == elem.name.local
    }

    pub fn attrs(&self) -> Option<&[Attribute]> {
        Some(&self.downcast_element()?.attrs)
    }

    pub fn attr(&self, name: impl PartialEq<LocalName>) -> Option<&str> {
        self.downcast_element()?.attr(name)
    }

    pub fn has_attr(&self, name: impl PartialEq<LocalName>) -> bool {
        self.downcast_element()
            .is_some_and(|elem| elem.has_attr(name))
    }

    pub fn kind(&self) -> NodeKind {
        match self {
            NodeData::Document(_) => NodeKind::Document,
            NodeData::Element(_) => NodeKind::Element,
            NodeData::AnonymousBlock(_) => NodeKind::AnonymousBlock,
            NodeData::Text(_) => NodeKind::Text,
            NodeData::Comment { .. } => NodeKind::Comment,
            NodeData::ShadowRoot(_) => NodeKind::ShadowRoot,
        }
    }

    pub fn shadow_root_data(&self) -> Option<&ShadowRootData> {
        match self {
            Self::ShadowRoot(data) => Some(data),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TextNodeData {
    /// The textual content of the text node. Inline up to 12 bytes on wasm32, which holds
    /// three quarters of a typical document's text nodes without an allocation.
    pub content: compact_str::CompactString,
}

impl TextNodeData {
    pub fn new(content: impl Into<compact_str::CompactString>) -> Self {
        Self {
            content: content.into(),
        }
    }
}

/*
-> Computed styles
-> Layout
-----> Needs to happen only when styles are computed
*/

// type DomRefCell<T> = RefCell<T>;

// pub struct DomData {
//     // ... we can probs just get away with using the html5ever types directly. basically just using the servo dom, but without the bindings
//     local_name: html5ever::LocalName,
//     tag_name: html5ever::QualName,
//     namespace: html5ever::Namespace,
//     prefix: DomRefCell<Option<html5ever::Prefix>>,
//     attrs: DomRefCell<Vec<Attr>>,
//     // attrs: DomRefCell<Vec<Dom<Attr>>>,
//     id_attribute: DomRefCell<Option<Atom>>,
//     is: DomRefCell<Option<LocalName>>,
//     // style_attribute: DomRefCell<Option<Arc<Locked<PropertyDeclarationBlock>>>>,
//     // attr_list: MutNullableDom<NamedNodeMap>,
//     // class_list: MutNullableDom<DOMTokenList>,
//     state: Cell<ElementState>,
// }

impl Node {
    pub fn tree(&self) -> &crate::NodeTree {
        unsafe { &*self.tree }
    }

    #[track_caller]
    pub fn with(&self, id: NodeId) -> &Node {
        self.tree().get(id).unwrap()
    }

    pub fn print_tree(&self, level: usize) {
        println!(
            "{} {} {:?} {} {:?}",
            "  ".repeat(level),
            self.id(),
            self.parent_id(),
            self.node_debug_str().replace('\n', ""),
            self.children()
        );
        for child_id in self.children().iter() {
            let child = self.with(child_id);
            child.print_tree(level + 1)
        }
    }

    // Get the index of the current node in the parents child list
    pub fn index_of_child(&self, child_id: NodeId) -> Option<usize> {
        self.children().position(child_id)
    }

    // Get the index of the current node in the parents child list
    pub fn child_index(&self) -> Option<usize> {
        let parent_id = self.parent_id()?;
        // Siblings share our tree, so comparing slot indexes compares ids.
        let own_slot = self.flags.slot_index();
        let siblings = match self.tree()[parent_id].children.slots() {
            super::child_list::ChildSlots::One(only) => return (only == own_slot).then_some(0),
            super::child_list::ChildSlots::Many(siblings) => siblings,
        };
        let slot = parent_id.as_u64() as usize % CHILD_INDEX_HINTS;
        let hint = CHILD_INDEX_HINT.with(|hints| hints[slot].get());
        // Walks visit siblings in order, so the last index seen under this parent (or its
        // neighbour) is usually ours. Any hint is checked against the list before use.
        let found = [hint.wrapping_add(1), hint, hint.wrapping_sub(1)]
            .into_iter()
            .find(|&index| siblings.get(index) == Some(&own_slot))
            .or_else(|| siblings.iter().position(|&index| index == own_slot))?;
        CHILD_INDEX_HINT.with(|hints| hints[slot].set(found));
        Some(found)
    }

    // Get the nth node in the parents child list
    pub fn forward(&self, n: usize) -> Option<&Node> {
        let child_idx = self.child_index().unwrap_or(0);
        self.tree()[self.parent_id()?]
            .children()
            .get(child_idx + n)
            .map(|id| self.with(id))
    }

    pub fn backward(&self, n: usize) -> Option<&Node> {
        let child_idx = self.child_index().unwrap_or(0);
        if child_idx < n {
            return None;
        }

        self.tree()[self.parent_id()?]
            .children()
            .get(child_idx - n)
            .map(|id| self.with(id))
    }

    pub fn is_element(&self) -> bool {
        matches!(self.data, NodeData::Element { .. })
    }

    pub fn is_anonymous(&self) -> bool {
        matches!(self.data, NodeData::AnonymousBlock { .. })
    }

    pub fn is_shadow_root(&self) -> bool {
        matches!(self.data, NodeData::ShadowRoot(_))
    }

    pub fn shadow_root_data(&self) -> Option<&ShadowRootData> {
        self.data.shadow_root_data()
    }

    /// Shadow-root node id when this node is a shadow host.
    /// None for non-elements and hosts without an attached root.
    pub fn attached_shadow_root_id(&self) -> Option<NodeId> {
        self.element_data().and_then(|el| el.shadow_root())
    }

    /// Nearest enclosing shadow root's node id, walking light parents.
    /// None for nodes in the document tree proper.
    pub fn containing_shadow_root_id(&self) -> Option<NodeId> {
        let mut current = self.parent_id();
        while let Some(id) = current {
            let node = self.with(id);
            if node.is_shadow_root() {
                return Some(id);
            }
            current = node.parent_id();
        }
        None
    }

    /// Flat-tree children for layout/paint: hosts render their shadow
    /// root's children; slots their assigned nodes (children=fallback).
    pub fn flat_children(&self) -> ChildIds<'_> {
        if let Some(shadow_root_id) = self.attached_shadow_root_id() {
            return self.with(shadow_root_id).children();
        }
        if let Some(assigned) = self.assigned_nodes() {
            if !assigned.is_empty() {
                return ChildIds(ChildRepr::Ids(assigned));
            }
        }
        self.children()
    }

    /// True for HTML <slot> elements (flat-tree projection points).
    pub fn is_slot_element(&self) -> bool {
        self.data.is_element_with_tag_name(&local_name!("slot"))
    }

    pub fn is_text_node(&self) -> bool {
        matches!(self.data, NodeData::Text { .. })
    }

    pub fn element_data(&self) -> Option<&ElementData> {
        match self.data {
            NodeData::Element(ref data) => Some(data),
            NodeData::AnonymousBlock(ref data) => Some(data),
            _ => None,
        }
    }

    pub fn element_data_mut(&mut self) -> Option<&mut ElementData> {
        match self.data {
            NodeData::Element(ref mut data) => Some(data),
            NodeData::AnonymousBlock(ref mut data) => Some(data),
            _ => None,
        }
    }

    pub fn text_data(&self) -> Option<&TextNodeData> {
        match self.data {
            NodeData::Text(ref data) => Some(data),
            _ => None,
        }
    }

    pub fn text_data_mut(&mut self) -> Option<&mut TextNodeData> {
        match self.data {
            NodeData::Text(ref mut data) => Some(data),
            _ => None,
        }
    }

    pub fn node_debug_str(&self) -> String {
        let mut s = String::new();

        match &self.data {
            NodeData::Document(_) => write!(s, "DOCUMENT"),
            // NodeData::Doctype { name, .. } => write!(s, "DOCTYPE {name}"),
            NodeData::Text(data) => {
                let bytes = data.content.as_bytes();
                write!(
                    s,
                    "TEXT {}",
                    std::str::from_utf8(bytes.split_at(10.min(bytes.len())).0)
                        .unwrap_or("INVALID UTF8")
                )
            }
            NodeData::Comment { .. } => write!(s, "COMMENT"),
            NodeData::AnonymousBlock(_) => write!(s, "AnonymousBlock"),
            NodeData::ShadowRoot(data) => write!(s, "#shadow-root ({:?})", data.mode),
            NodeData::Element(data) => {
                let name = &data.name;
                let class = self.attr(local_name!("class")).unwrap_or("");
                let id = self.attr(local_name!("id")).unwrap_or("");
                let display = self.display_constructed_as().to_css_string();
                write!(s, "<{}", name.local).unwrap();
                if !id.is_empty() {
                    write!(s, " #{id}").unwrap();
                }
                if !class.is_empty() {
                    if class.contains(' ') {
                        write!(s, " class=\"{class}\"").unwrap()
                    } else {
                        write!(s, " .{class}").unwrap()
                    }
                }
                write!(s, "> ({display})")
            } // NodeData::ProcessingInstruction { .. } => write!(s, "ProcessingInstruction"),
        }
        .unwrap();
        s
    }

    pub fn attrs(&self) -> Option<&[Attribute]> {
        Some(&self.element_data()?.attrs)
    }

    pub fn attr(&self, name: LocalName) -> Option<&str> {
        let attr = self.attrs()?.iter().find(|id| id.name.local == name)?;
        Some(&attr.value)
    }

    pub fn primary_styles(&self) -> Option<impl Deref<Target = ServoArc<ComputedValues>>> {
        self.try_stylo_element_data()
            .and_then(|stylo| stylo.primary_styles())
    }

    /// A lazy Taffy style backed by this node's primary stylo style.
    ///
    /// Panics if the node has no computed styles: layout always runs after
    /// styling, and the only unstyled nodes (text, comments, descendants of
    /// `display: none`) are never queried by Taffy.
    pub fn layout_style(&self) -> stylo_taffy::TaffyStyloStyle<ComputedStyleRef<'_>> {
        let styles = self
            .try_stylo_element_data()
            .and_then(|stylo| stylo.computed_styles())
            .expect("layout_style() called on a node without computed styles");

        let mut flags = stylo_taffy::StyleFlags::empty();
        if let Some(el) = self.data.downcast_element() {
            if crate::layout::replaced::is_replaced_element(&el.name.local) {
                flags |= stylo_taffy::StyleFlags::IS_REPLACED;
            }
            if el.name.local == local_name!("button") {
                flags |= stylo_taffy::StyleFlags::IS_BUTTON;
            }
            if el.is_button_input() {
                flags |= stylo_taffy::StyleFlags::IS_BUTTON_INPUT;
            }
            if let Some(inline_layout) = el.inline_layout_data() {
                flags |= inline_layout.span_cb_flags;
            }
        }

        #[cfg(feature = "writing-mode")]
        {
            // Own-layout_wm by default: the node's own algorithm reads its style in its own axes
            let layout_wm = styles.writing_mode;
            stylo_taffy::TaffyStyloStyle::new_in(styles, flags, layout_wm)
        }
        #[cfg(not(feature = "writing-mode"))]
        {
            stylo_taffy::TaffyStyloStyle::new(styles, flags)
        }
    }

    /// The node's style for Taffy, expressed in the axes of the writing mode `layout_wm` (the writing
    /// mode of the box laying this node out). Used for the child-style getters.
    #[cfg(feature = "writing-mode")]
    pub fn layout_style_in(
        &self,
        layout_wm: stylo_taffy::WritingMode,
    ) -> stylo_taffy::TaffyStyloStyle<ComputedStyleRef<'_>> {
        let mut style = self.layout_style();
        style.layout_wm = layout_wm;
        style
    }

    /// Whether the node has any `contain` value, which blocks `writing-mode` propagation from
    /// `<body>` to the root (css-writing-modes-3 §8).
    #[cfg(feature = "writing-mode")]
    pub(crate) fn has_containment(&self) -> bool {
        self.primary_styles()
            .is_some_and(|style| !style.clone_contain().is_empty())
    }

    /// Whether the node is a flex container with a column `flex-direction`, i.e. aligns its
    /// children's `align-self` in its inline axis.
    #[cfg(feature = "writing-mode")]
    pub(crate) fn is_column_flex_container(&self) -> bool {
        self.primary_styles().is_some_and(|s| {
            s.clone_display().inside() == DisplayInside::Flex
                && !self.element_data().is_some_and(|el| el.is_button_input())
                && matches!(
                    s.get_position().flex_direction,
                    FlexDirection::Column | FlexDirection::ColumnReverse
                )
        })
    }

    /// The node's computed `writing-mode` (horizontal-tb for nodes without styles)
    #[cfg(feature = "writing-mode")]
    pub fn writing_mode(&self) -> stylo_taffy::WritingMode {
        self.primary_styles()
            .map(|s| s.writing_mode)
            .unwrap_or(stylo_taffy::WritingMode::empty())
    }

    /// The node's `display` as a [`taffy::Display`]. Returns [`taffy::Display::Block`]
    /// for nodes without computed styles (e.g. text nodes).
    pub fn taffy_display(&self) -> taffy::Display {
        self.primary_styles()
            .map(|style| {
                let display = style.clone_display();
                if self.element_data().is_some_and(|el| el.is_button_input())
                    && !matches!(
                        display.inside(),
                        DisplayInside::None | DisplayInside::Contents
                    )
                {
                    taffy::Display::FlowRoot
                } else if display.inside() == DisplayInside::WebkitBox {
                    // Stylo_taffy's fallback maps -webkit-box to Block;
                    // emulate the legacy box as a flex container (the main axis
                    // comes from -webkit-box-orient, see FlexContainerStyle).
                    taffy::Display::Flex
                } else {
                    stylo_taffy::convert::display(display)
                }
            })
            .unwrap_or(taffy::Display::Block)
    }

    /// The node's `position` as a [`taffy::Position`]. Returns [`taffy::Position::Static`]
    /// for nodes without computed styles (e.g. text nodes).
    pub fn taffy_position(&self) -> taffy::Position {
        self.primary_styles()
            .map(|s| stylo_taffy::convert::position(s.get_box().position))
            .unwrap_or(taffy::Position::Static)
    }

    /// Whether the node is an out-of-flow box that Taffy positions from its containing
    /// block's hoisted child list rather than from its parent (`display: none` boxes
    /// generate no box and are never hoisted).
    pub fn is_out_of_flow(&self) -> bool {
        self.primary_styles().is_some_and(|s| {
            let box_style = s.get_box();
            stylo_taffy::convert::position(box_style.position).is_out_of_flow()
                && stylo_taffy::convert::display(box_style.display) != taffy::Display::None
        })
    }

    pub fn text_content(&self) -> String {
        let mut out = String::new();
        self.write_text_content(&mut out);
        out
    }

    // HTML "the directionality" with whatwg/html#9796's
    // shadow rules; drives :dir() matching and the dir=auto hint.

    // The upward chain is the light tree plus the shadow-root→host
    // crossing. Slotted nodes do not take directionality from their
    // assigned slot (dir-shadow-03/04/31: dir=rtl on the slot gives
    // ltr :dir but rtl computed; CSS inherits flat, HTML doesn't).
    pub(crate) fn html_directionality_is_rtl(&self) -> bool {
        let mut cur = Some(self.id());
        while let Some(id) = cur {
            let node = self.with(id);
            if let Some(el) = node.data.downcast_element() {
                let dir = el.attr(local_name!("dir"));
                if let Some(d) = dir {
                    if d.eq_ignore_ascii_case("rtl") {
                        return true;
                    }
                    if d.eq_ignore_ascii_case("ltr") {
                        return false;
                    }
                }
                // bdi defaults to auto; an invalid dir value on it
                // is also auto per HTML's "undefined" attr state.
                let auto = dir.is_some_and(|d| d.eq_ignore_ascii_case("auto"))
                    || el.name.local == local_name!("bdi");
                if auto {
                    return node.dir_auto_scan_is_rtl().unwrap_or(false);
                }
            }
            cur = node.directionality_flat_parent();
        }
        false
    }

    fn directionality_flat_parent(&self) -> Option<NodeId> {
        let parent_id = self.parent_id()?;
        if let NodeData::ShadowRoot(ref data) = self.with(parent_id).data {
            return Some(data.host);
        }
        Some(parent_id)
    }

    // HTML auto-directionality: tree-order first-strong scan. Slots
    // scan their assigned nodes (fallback content when unassigned);
    // other elements scan light children: a host's shadow tree is
    // not its descendant. Excluded subtrees match the spec list.
    pub(crate) fn dir_auto_scan_is_rtl(&self) -> Option<bool> {
        // HTML: a text control's auto-directionality comes from its
        // value, not its children. The dirty IDL value lives in the
        // editor's value_override (set_text_input_value restyles the
        // control on every write, so the cascade hint stays fresh);
        // pre-dirty falls back to the value attribute for inputs.
        // Textareas fall through to child text = the default value.
        if let Some(el) = self.data.downcast_element() {
            if let Some(data) = el.text_input_data() {
                if let Some(v) = &data.value_override {
                    return match parley::first_strong_direction(v) {
                        Some(parley::BaseDirection::Rtl) => Some(true),
                        Some(_) => Some(false),
                        None => None,
                    };
                }
            }
            if el.name.local == local_name!("input") {
                let v = el.attr(local_name!("value")).unwrap_or("");
                return match parley::first_strong_direction(v) {
                    Some(parley::BaseDirection::Rtl) => Some(true),
                    Some(_) => Some(false),
                    None => None,
                };
            }
        }
        fn scan(node: &Node, is_scan_root: bool) -> Option<bool> {
            match &node.data {
                NodeData::Text(t) => match parley::first_strong_direction(&t.content) {
                    Some(parley::BaseDirection::Rtl) => Some(true),
                    Some(_) => Some(false),
                    None => None,
                },
                NodeData::Element(el) | NodeData::AnonymousBlock(el) => {
                    if !is_scan_root {
                        let tag = &el.name.local;
                        if *tag == local_name!("bdi")
                            || *tag == local_name!("script")
                            || *tag == local_name!("style")
                            || *tag == local_name!("textarea")
                        {
                            return None;
                        }
                        // A slot met as a descendant contributes
                        // nothing: its assigned nodes belong to the
                        // light tree and must not leak into shadow-
                        // side scans (WPT dir-shadow-30/34 pin this).
                        if node.is_slot_element() {
                            return None;
                        }
                        if el.attr(local_name!("dir")).is_some_and(|d| {
                            d.eq_ignore_ascii_case("auto")
                                || d.eq_ignore_ascii_case("ltr")
                                || d.eq_ignore_ascii_case("rtl")
                        }) {
                            return None;
                        }
                    }
                    let kids = if node.is_slot_element() {
                        node.flat_children()
                    } else {
                        node.children()
                    };
                    kids.iter()
                        .find_map(|child| scan(node.with(child), false))
                }
                _ => None,
            }
        }
        scan(self, true)
    }

    fn write_text_content(&self, out: &mut String) {
        match &self.data {
            NodeData::Text(data) => {
                out.push_str(&data.content);
            }
            NodeData::Element(..) | NodeData::AnonymousBlock(..) => {
                for child_id in self.children().iter() {
                    self.with(child_id).write_text_content(out);
                }
            }
            _ => {}
        }
    }

    pub fn flush_style_attribute(&mut self, url_extra_data: &UrlExtraData) {
        let guard = self.tree().guard.clone();
        if let NodeData::Element(ref mut elem_data) = self.data {
            elem_data.flush_style_attribute(&guard, url_extra_data);
        }
    }

    /// The `order` sort key of this node within a flex/grid container's
    /// `layout_children`. The list is stable-sorted by this key when the
    /// container's children are constructed (`collect_layout_children`) and
    /// re-collected when a child's `order` changes (`REORDER_CHILDREN` damage).
    ///
    /// ::before/::after pseudos are flex/grid items and honor `order`. They
    /// sit first/last in the constructed list, and the sort is stable, so
    /// ties keep ::before first and ::after last. Out-of-flow children are
    /// not flex/grid items, so they key as 0 and keep source order.
    pub fn order(&self) -> i32 {
        self.primary_styles()
            .filter(|s| !matches!(s.get_box().position, Position::Absolute | Position::Fixed))
            .map(|s| s.clone_order())
            .unwrap_or(0)
    }

    pub fn z_index(&self) -> i32 {
        self.primary_styles()
            .map(|s| s.clone_z_index().integer_or(0))
            .unwrap_or(0)
    }

    // https://developer.mozilla.org/en-US/docs/Web/CSS/CSS_positioned_layout/Stacking_context#features_creating_stacking_contexts
    /// Origin correction for a re-homed abspos painted by its flow
    /// parent. `final_layout.location` is relative to the box's
    /// out-of-flow containing block, so shift by (containing block -
    /// flow parent) to land it in the flow parent's coordinate space.
    ///
    /// The flow parent is a descendant of the containing block, so the
    /// delta is minus the offsets accumulated walking up from the flow
    /// parent to the containing block (same `location - scroll_offset`
    /// convention as the hoisted-child positions).
    pub fn rehomed_paint_translate(&self, paint_parent: NodeId) -> taffy::Point<f32> {
        let mut offset = taffy::Point { x: 0.0, y: 0.0 };
        let Some(containing_block) = self.oof_containing_block() else {
            return offset;
        };
        let tree = self.tree();
        let mut cur = paint_parent;
        // Bounded: a malformed layout-parent chain must not spin.
        for _ in 0..64 {
            if cur == containing_block {
                // A viewport-fixed box does not scroll with the
                // page, so cancel the viewport scroll its containing block
                // (the root element) applies, as paint and hit-testing do for
                // the root's own fixed children.
                if self.is_viewport_fixed() {
                    let scroll = *tree[containing_block].scroll_offset();
                    let anchor = tree.fixed_anchor_scroll();
                    offset.x -= (scroll.x + anchor.x) as f32;
                    offset.y -= (scroll.y + anchor.y) as f32;
                }
                return taffy::Point { x: -offset.x, y: -offset.y };
            }
            let Some(node) = tree.get(cur) else { break };
            offset.x += node.final_layout().location.x;
            offset.y += node.final_layout().location.y;
            // The paint parent draws its re-homed children unscrolled, so
            // only the scroll of the boxes above it moves them.
            if cur != paint_parent {
                offset.x -= node.scroll_offset().x as f32;
                offset.y -= node.scroll_offset().y as f32;
            }
            // An out-of-flow box is placed against its containing block,
            // not its flow parent, so continue the walk from there.
            let next = match node.taffy_position() {
                taffy::Position::Absolute | taffy::Position::Fixed => node
                    .oof_containing_block()
                    .or(node.layout_parent_id()),
                _ => node.layout_parent_id(),
            };
            match next {
                Some(next) if next != cur => cur = next,
                _ => break,
            }
        }
        // Chain didn't reach the containing block: leave the box where
        // layout put it rather than shifting it somewhere arbitrary.
        taffy::Point { x: 0.0, y: 0.0 }
    }

    /// Whether this is a `position: fixed` box positioned against
    /// the viewport: its containing block is the root element, which is a
    /// layout child of the Document node. A fixed box contained by a
    /// transformed ancestor scrolls with that ancestor instead.
    pub fn is_viewport_fixed(&self) -> bool {
        if self.taffy_position() != taffy::Position::Fixed {
            return false;
        }
        let tree = self.tree();
        self.oof_containing_block()
            .and_then(|cb| tree.get(cb))
            .is_some_and(|cb| {
                cb.containing_block()
                    .and_then(|id| tree.get(id))
                    .is_none_or(|parent| matches!(parent.data, NodeData::Document(_)))
            })
    }

    /// This re-homed paint child's painted extent in `host`'s CSS-px space:
    /// its layout box and overflow, mapped through its own transform.
    /// Floating UI parks every popup at `left:0;top:0` and moves it with
    /// `transform: translate(...)`, so a reach built from layout alone sat
    /// at the origin and the hit descent pruned the popup at its paint
    /// parent. `scrollable_overflow` is device px and already unions the
    /// transformed boxes below this one, so read it once transforms resolve.
    pub fn rehomed_paint_rect(&self, host: NodeId, scale: f64) -> (f32, f32, f32, f32) {
        let t = self.rehomed_paint_translate(host);
        let layout = self.final_layout();
        let w = layout.size.width.max(layout.scrollable_overflow_rect.right) as f64 * scale;
        let h = layout.size.height.max(layout.scrollable_overflow_rect.bottom) as f64 * scale;
        let local = KurboRect::new(0.0, 0.0, w, h).union(*self.scrollable_overflow());
        let mut full = Affine::translate((
            (t.x + layout.location.x) as f64 * scale,
            (t.y + layout.location.y) as f64 * scale,
        ));
        if let Some(transform) = self.transform() {
            full *= transform;
        }
        let r = full.transform_rect_bbox(local);
        (
            (r.x0 / scale) as f32,
            (r.y0 / scale) as f32,
            (r.x1 / scale) as f32,
            (r.y1 / scale) as f32,
        )
    }

    // Live union of re-homed paint-child boxes in this node's own
    // coordinate space. rehomed_paint_translate reads scroll offsets
    // at call time, unlike the resolve-time rehomed_reach cache, so
    // this stays correct on pure-scroll frames that skip resolve.
    pub fn live_rehomed_reach(&self, scale: f64) -> Option<(f32, f32, f32, f32)> {
        let children = self.rehomed_paint_children()?;
        if children.is_empty() {
            return None;
        }
        let tree = self.tree();
        let mut rect: Option<(f32, f32, f32, f32)> = None;
        for &child_id in children.iter() {
            let Some(child) = tree.get(child_id) else { continue };
            let r = child.rehomed_paint_rect(self.id(), scale);
            rect = Some(match rect {
                None => r,
                Some(a) => (a.0.min(r.0), a.1.min(r.1), a.2.max(r.2), a.3.max(r.3)),
            });
        }
        rect
    }

    pub fn is_stacking_context_root(&self, is_flex_or_grid_item: bool) -> bool {
        let Some(style) = self.primary_styles() else {
            return false;
        };

        let position = style.clone_position();
        let has_z_index = !style.clone_z_index().is_auto();

        if style.clone_opacity() != 1.0 {
            return true;
        }

        let position_based = match position {
            Position::Fixed | Position::Sticky => true,
            Position::Relative | Position::Absolute => has_z_index,
            Position::Static => has_z_index && is_flex_or_grid_item,
        };
        if position_based {
            return true;
        }

        let box_style = style.get_box();
        if !box_style.transform.0.is_empty()
            || !matches!(box_style.rotate, Rotate::None)
            || !matches!(box_style.scale, Scale::None)
            || !matches!(box_style.translate, Translate::None)
        {
            return true;
        }

        if self.applies_atomic_paint_effect() {
            return true;
        }

        if style.get_box().isolation == Isolation::Isolate {
            return true;
        }

        // TODO: mix-blend-mode
        // TODO: contain

        false
    }

    /// Whether this node's styles apply an atomic paint effect (opacity, filter,
    /// clip-path, mask) to its subtree.
    pub(crate) fn applies_atomic_paint_effect(&self) -> bool {
        use style::values::computed::basic_shape::ClipPath;
        use style::values::generics::image::GenericImage;

        let Some(style) = self.primary_styles() else {
            return false;
        };

        if style.clone_opacity() != 1.0 {
            return true;
        }

        let effects = style.get_effects();
        if !effects.filter.0.is_empty() {
            return true;
        }

        if !matches!(style.clone_clip_path(), ClipPath::None) {
            return true;
        }

        style
            .get_svg()
            .mask_image
            .0
            .iter()
            .any(|image| !matches!(image, GenericImage::None))
    }

    /// Takes an (x, y) position (relative to the *parent's* top-left corner) and returns:
    ///    - None if the position is outside of this node's bounds
    ///    - Some(HitResult) if the position is within the node but doesn't match any children
    ///    - The result of recursively calling child.hit() on the the child element that is
    ///      positioned at that position if there is one.
    ///
    /// TODO: z-index
    /// (If multiple children are positioned at the position then a random one will be recursed into)
    pub fn hit(&self, x: f32, y: f32, scale: f64) -> Option<HitResult> {
        self.hit_inner(x, y, scale, &mut None)
    }

    /// [`hit`](Self::hit), also resolving the innermost overlay scrollbar
    /// thumb under the point into `scrollbar` during the same descent (so
    /// thumb hit-testing shares the exact coordinate handling — transforms
    /// included — of every other hit test).
    pub(crate) fn hit_inner(
        &self,
        x: f32,
        y: f32,
        scale: f64,
        scrollbar: &mut Option<crate::node::ScrollbarRef>,
    ) -> Option<HitResult> {
        use style::computed_values::pointer_events::T as PointerEvents;
        use style::computed_values::visibility::T as Visibility;

        // Don't hit on visbility:hidden elements
        if let Some(style) = self.primary_styles() {
            if matches!(
                style.clone_visibility(),
                Visibility::Hidden | Visibility::Collapse
            ) {
                return None;
            }
        }

        // pointer-events:none makes this element transparent to hits, but its
        // descendants are still tested (one may restore pointer-events:auto).
        let pointer_events_none = self
            .primary_styles()
            .is_some_and(|style| style.clone_pointer_events() == PointerEvents::None);

        let mut x = x - self.final_layout().location.x + self.scroll_offset().x as f32;
        let mut y = y - self.final_layout().location.y + self.scroll_offset().y as f32;

        if let Some(t) = self.transform() {
            // Only a singular matrix has no inverse; small nonzero scales still map points.
            if t.determinant() == 0.0 {
                return None;
            }
            let p = t.inverse() * kurbo::Point::new(x as f64 * scale, y as f64 * scale);
            x = (p.x / scale) as f32;
            y = (p.y / scale) as f32;
        }

        let size = self.final_layout().size;
        let matches_self = !(x < 0.0
            || x > size.width + self.scroll_offset().x as f32
            || y < 0.0
            || y > size.height + self.scroll_offset().y as f32);

        let overflow_rect = self.final_layout().scrollable_overflow_rect;
        let matches_content = !(x < 0.0
            || x > overflow_rect.right + self.scroll_offset().x as f32
            || y < 0.0
            || y > overflow_rect.bottom + self.scroll_offset().y as f32);

        let matches_hoisted_content = match self.stacking_context() {
            Some(sc) => {
                // Both the point (after the scroll adjustment above) and the
                // bbox are in this node's unscrolled content coordinates.
                let bbox = sc.hoisted_content_bbox(self.tree(), self.id(), scale);
                x >= bbox.left && x <= bbox.right && y >= bbox.top && y <= bbox.bottom
            }
            None => false,
        };

        // `scrollable_overflow` is stored in device (scaled) pixels, whereas the
        // coordinates here are in CSS pixels, so unscale it before comparing.
        let overflow = *self.scrollable_overflow();

        let matches_overflow = x >= (overflow.x0 / scale) as f32
            && x <= (overflow.x1 / scale) as f32
            && y >= (overflow.y0 / scale) as f32
            && y <= (overflow.y1 / scale) as f32;

        // Taffy's bounds cannot cover re-homed paint children (they are
        // not layout children of their flow parent); widen the prune by
        // the post-layout union of any re-homed boxes reachable below.
        // This node's own re-homed boxes paint unscrolled, while reach
        // propagated up from descendants is in its scrolled content space,
        // so try the point in both: a false match only costs a descent.
        let matches_rehomed = match self.rehomed_reach() {
            Some((x0, y0, x1, y1)) => {
                let inside = |px: f32, py: f32| px >= x0 && px <= x1 && py >= y0 && py <= y1;
                inside(x, y)
                    || inside(
                        x - self.scroll_offset().x as f32,
                        y - self.scroll_offset().y as f32,
                    )
            }
            None => false,
        };

        if !matches_self
            && !matches_content
            && !matches_hoisted_content
            && !matches_overflow
            && !matches_rehomed
        {
            return None;
        }

        // Descendants overwrite, so the innermost scroll container's thumb
        // wins. Thumb coords are border-box relative (unscrolled).
        if matches_self
            && let Some(sb) = self.scrollbar_at_local(
                (x - self.scroll_offset().x as f32) as f64,
                (y - self.scroll_offset().y as f32) as f64,
            )
        {
            *scrollbar = Some(sb);
        }

        // Positive z_index hoisted children
        if matches_hoisted_content {
            if let Some(hoisted) = self.stacking_context() {
                for hoisted_child in hoisted.pos_z_hoisted_children().rev() {
                    let pos = hoisted_child.position(self.tree(), self.id());
                    let x = x - pos.x;
                    let y = y - pos.y;
                    if let Some(hit) = self
                        .with(hoisted_child.node_id)
                        .hit_inner(x, y, scale, scrollbar)
                    {
                        return Some(hit);
                    }
                }
            }
        }

        // Call `.hit()` on each child in turn. If any return `Some` then return that value. Else return `Some(self.id).
        let paint_children = self.paint_children();
        for child_id in paint_children.iter().flat_map(|children| children.iter()).rev() {
            let child = self.with(*child_id);
            let child_position = child.taffy_position();
            let mut child_x = x;
            let mut child_y = y;
            if child_position.is_out_of_flow() {
                // Fixed-position children of the root element are positioned
                // against the viewport and do not scroll with it. (A fixed box
                // whose containing block is a transformed ancestor scrolls with
                // that ancestor like any other out-of-flow box.)
                // The root element is a layout child of the Document
                // node, so recognise it by that, not by a `None` containing block.
                if child_position == taffy::Position::Fixed
                    && self
                        .containing_block()
                        .map(|id| self.with(id))
                        .is_none_or(|parent| matches!(parent.data, NodeData::Document(_)))
                {
                    child_x -= self.scroll_offset().x as f32;
                    child_y -= self.scroll_offset().y as f32;
                    let viewport_scroll = self.tree().viewport_scroll();
                    child_x -= viewport_scroll.x as f32;
                    child_y -= viewport_scroll.y as f32;
                }
            }
            // A re-homed abspos keeps its containing block's layout
            // origin, so undo the difference before descending. It paints
            // unscrolled, so leave this node's scrolled content space too.
            if child.paint_parent_override() == Some(self.id()) {
                let t = child.rehomed_paint_translate(self.id());
                child_x -= t.x + self.scroll_offset().x as f32;
                child_y -= t.y + self.scroll_offset().y as f32;
            }
            if let Some(hit) = child.hit_inner(child_x, child_y, scale, scrollbar) {
                return Some(hit);
            }
        }
        drop(paint_children);

        // Negative z_index hoisted children
        if matches_hoisted_content {
            if let Some(hoisted) = self.stacking_context() {
                for hoisted_child in hoisted.neg_z_hoisted_children().rev() {
                    let pos = hoisted_child.position(self.tree(), self.id());
                    let x = x - pos.x;
                    let y = y - pos.y;
                    if let Some(hit) = self
                        .with(hoisted_child.node_id)
                        .hit_inner(x, y, scale, scrollbar)
                    {
                        return Some(hit);
                    }
                }
            }
        }

        // Inline children
        if self.flags.is_inline_root() {
            let element_data = &self.element_data().unwrap();
            if let Some(ild) = element_data.inline_layout_data() {
                let layout = ild.layout(self.tree());
                let scale = layout.scale();
                let content_box_offset = taffy::Point {
                    x: self.final_layout().padding.left + self.final_layout().border.left,
                    y: self.final_layout().padding.top + self.final_layout().border.top,
                };
                let text_x = x - content_box_offset.x;
                let text_y = y - content_box_offset.y - ild.block_offset;

                // Parley's `from_point_exact` is exact in the inline axis
                // only: `line_for_offset` clamps a block offset above the
                // first line or below the last one onto that line. The
                // descent above reaches nodes whose box does not contain
                // the point (matches_content / matches_overflow /
                // matches_rehomed exist for that), so without a block-axis
                // check a pointer 17,000px above a long reference list
                // hits the reference text whose x-range it shares, and
                // every drag in the lead paragraph selects the article.
                if let Some((cluster, _side)) =
                    Cluster::from_point_exact(layout, text_x * scale, text_y * scale)
                    && {
                        let line = cluster.line();
                        let m = line.metrics();
                        let by = text_y * scale;
                        by >= m.block_min_coord && by < m.block_max_coord
                    }
                {
                    let node_id = cluster.style().brush.id;
                    let text_pointer_events_none = self
                        .with(node_id)
                        .primary_styles()
                        .is_some_and(|style| style.clone_pointer_events() == PointerEvents::None);
                    if !text_pointer_events_none {
                        return Some(HitResult {
                            node_id,
                            x: text_x,
                            y: text_y,
                            is_text: true,
                        });
                    }
                }
            }
        }

        // Self (this node)
        if matches_self && !pointer_events_none {
            let (hit_x, hit_y) = if self.flags.is_inline_root() {
                (
                    x - self.final_layout().padding.left - self.final_layout().border.left,
                    y - self.final_layout().padding.top - self.final_layout().border.top,
                )
            } else {
                (x, y)
            };
            return Some(HitResult {
                node_id: self.id(),
                x: hit_x,
                y: hit_y,
                is_text: false,
            });
        }

        None
    }

    /// Find the inline root ancestor of this node (or self if this is an inline root).
    /// Returns None if no inline root ancestor exists.
    pub fn inline_root_ancestor(&self) -> Option<&Node> {
        let mut node = self;
        loop {
            if node.flags.is_inline_root() {
                return Some(node);
            }
            let id = node.layout_parent_id()?;
            node = self.with(id);
        }
    }

    /// Get the text byte offset at a given point, using coordinates already transformed
    /// to be relative to this inline root's content box.
    /// Returns Some(byte_offset) if the point hits text, None otherwise.
    pub fn text_offset_at_point(&self, x: f32, y: f32) -> Option<usize> {
        if !self.flags.is_inline_root() {
            return None;
        }

        let element_data = self.element_data()?;
        let inline_layout = element_data.inline_layout_data()?;
        let layout = inline_layout.layout(self.tree());
        let scale = layout.scale();
        let y = y - inline_layout.block_offset;

        // Use Parley's cluster hit testing (from_point is more forgiving than from_point_exact)
        let (cluster, side) = Cluster::from_point(layout, x * scale, y * scale)?;

        // Determine byte offset based on which side of the cluster was clicked
        // For LTR text: left side = start of cluster, right side = end of cluster
        // For RTL text: left side = end of cluster, right side = start of cluster
        // Also, explicit line breaks should always use start to avoid cursor appearing on next line
        let is_leading = side == ClusterSide::Left;
        let offset = if cluster.is_rtl() {
            if is_leading {
                cluster.text_range().end
            } else {
                cluster.text_range().start
            }
        } else {
            // LTR text
            if is_leading || cluster.is_line_break() == Some(BreakReason::Explicit) {
                cluster.text_range().start
            } else {
                cluster.text_range().end
            }
        };

        Some(offset)
    }

    /// Whether this node is a non-atomic inline element: one that has no layout box of its
    /// own because it is flattened into the containing inline root's text layout as a style
    /// span.
    pub fn is_non_atomic_inline(&self) -> bool {
        let Some(element) = self.element_data() else {
            return false;
        };
        if self.flags.is_inline_root()
            || crate::layout::replaced::is_inline_box_element(&element.name.local)
        {
            return false;
        }
        self.primary_styles().is_some_and(|styles| {
            let display = styles.clone_display();
            display.outside() == DisplayOutside::Inline && display.inside() == DisplayInside::Flow
        })
    }

    /// The node whose box this node's `Layout.location` is relative to: the
    /// `oof_containing_block` for a hoisted out-of-flow box, otherwise the
    /// `layout_parent`.
    pub fn containing_block(&self) -> Option<NodeId> {
        self.oof_containing_block()
            .or_else(|| self.layout_parent_id())
    }

    /// Computes the Document-relative coordinates of the `Node`
    pub fn absolute_position(&self, x: f32, y: f32) -> crate::util::Point<f32> {
        let x = x + self.final_layout().location.x - self.scroll_offset().x as f32;
        let y = y + self.final_layout().location.y - self.scroll_offset().y as f32;

        // Recurse up the positioning hierarchy
        self.containing_block()
            .map(|i| self.with(i).absolute_position(x, y))
            .unwrap_or(crate::util::Point { x, y })
    }

    /// Whether this node can act as an [`offset_parent`](Self::offset_parent): a positioned
    /// element, or one of the elements that always qualify (`body`, `td`, `th`).
    pub(crate) fn is_offset_parent(&self) -> bool {
        let Some(styles) = self.primary_styles() else {
            return false;
        };
        if styles.get_box().position != Position::Static {
            return true;
        }
        self.data.is_element_with_tag_name(&local_name!("body"))
            || self.data.is_element_with_tag_name(&local_name!("td"))
            || self.data.is_element_with_tag_name(&local_name!("th"))
    }

    /// Whether this node is a non-positioned `body` element. When such an element is the
    /// `offsetParent`, `offsetLeft`/`offsetTop` are measured from the initial containing
    /// block origin rather than from the `body`'s padding edge.
    pub(crate) fn is_static_body(&self) -> bool {
        self.data.is_element_with_tag_name(&local_name!("body"))
            && self
                .primary_styles()
                .is_some_and(|styles| styles.get_box().position == Position::Static)
    }

    /// The nearest layout ancestor that [is an offset parent](Self::is_offset_parent), as in
    /// CSSOM View's `offsetParent`.
    pub fn offset_parent(&self) -> Option<&Node> {
        let mut node = self;
        loop {
            node = self.with(node.containing_block()?);
            if node.is_offset_parent() {
                return Some(node);
            }
        }
    }

    /// The per-line-box fragment boxes of a non-atomic inline element, in CSS pixels relative
    /// to the border box of its inline root.
    ///
    /// Returns `None` for nodes that have their own layout box.
    pub fn inline_fragment_boxes(&self) -> Option<impl Iterator<Item = taffy::Rect<f32>> + '_> {
        use parley::PositionedLayoutItem;

        if !self.is_non_atomic_inline() {
            return None;
        }

        let inline_root = self.inline_root_ancestor()?;
        let inline_layout = inline_root.element_data()?.inline_layout_data()?;
        let layout = inline_layout.layout(self.tree());
        let scale = layout.scale();

        // Walk up the DOM parent chain from `id` to check whether it is (or is
        // inside) the target node, stopping at the inline root.
        let inline_root_id = inline_root.id();
        let is_in_target = move |mut id: NodeId| -> bool {
            loop {
                if id == self.id() {
                    return true;
                }
                if id == inline_root_id {
                    return false;
                }
                match self.with(id).parent_id() {
                    Some(parent) => id = parent,
                    None => return false,
                }
            }
        };

        let root_layout = inline_root.unrounded_layout();
        let content_box_inset = root_layout.padding + root_layout.border;
        let origin_x = content_box_inset.left;
        let origin_y = content_box_inset.top + inline_layout.block_offset;

        fn union(acc: &mut Option<taffy::Rect<f32>>, left: f32, top: f32, right: f32, bottom: f32) {
            *acc = Some(match *acc {
                Some(rect) => taffy::Rect {
                    left: rect.left.min(left),
                    top: rect.top.min(top),
                    right: rect.right.max(right),
                    bottom: rect.bottom.max(bottom),
                },
                None => taffy::Rect {
                    left,
                    top,
                    right,
                    bottom,
                },
            });
        }

        // One rect per line box: the union of all of the target's fragments on that line
        Some(layout.lines().filter_map(move |line| {
            let line_metrics = line.metrics();
            let mut line_rect: Option<taffy::Rect<f32>> = None;

            for item in line.items() {
                match item {
                    PositionedLayoutItem::GlyphRun(glyph_run) => {
                        if !is_in_target(glyph_run.style().brush.id) {
                            continue;
                        }
                        let x0 = glyph_run.offset();
                        let x1 = x0 + glyph_run.advance();
                        // Use the line box's block extent rather than the
                        // run's font ascent/descent: fonts with small
                        // typographic metrics would otherwise produce rects
                        // that clip the rendered glyphs. This matches the
                        // geometry used for text selection highlights.
                        let y0 = line_metrics.block_min_coord;
                        let y1 = line_metrics.block_max_coord;
                        union(&mut line_rect, x0, y0, x1, y1);
                    }
                    PositionedLayoutItem::InlineBox(inline_box) => {
                        if !is_in_target(NodeId::from_u64(inline_box.id)) {
                            continue;
                        }
                        let x0 = inline_box.x;
                        let y0 = inline_box.y;
                        union(
                            &mut line_rect,
                            x0,
                            y0,
                            x0 + inline_box.width,
                            y0 + inline_box.height,
                        );
                    }
                }
            }

            line_rect.map(|rect| taffy::Rect {
                left: origin_x + rect.left / scale,
                top: origin_y + rect.top / scale,
                right: origin_x + rect.right / scale,
                bottom: origin_y + rect.bottom / scale,
            })
        }))
    }

    /// CSSOM View's `offsetLeft`/`offsetTop`: the offset of this node's border box from the
    /// padding edge of its [`offset_parent`](Self::offset_parent).
    pub fn offset_top_left(&self) -> crate::util::Point<f32> {
        let mut x = 0.0;
        let mut y = 0.0;
        let mut current = self;

        // Non-atomic inlines have no layout box of their own: start from the first
        // fragment box (relative to the inline root's border box) and continue the
        // walk from the inline root.
        if let Some(mut fragments) = self.inline_fragment_boxes() {
            if let Some(first) = fragments.next() {
                x += first.left;
                y += first.top;
            }
            let Some(inline_root) = self.inline_root_ancestor() else {
                return crate::util::Point { x, y };
            };
            if inline_root.is_offset_parent() && !inline_root.is_static_body() {
                let border = inline_root.final_layout().border;
                return crate::util::Point {
                    x: x - border.left,
                    y: y - border.top,
                };
            }
            current = inline_root;
        }

        loop {
            let layout = current.final_layout();
            x += layout.location.x;
            y += layout.location.y;

            let Some(parent_id) = current.containing_block() else {
                break;
            };
            let parent = self.with(parent_id);
            if parent.is_offset_parent() && !parent.is_static_body() {
                let border = parent.final_layout().border;
                x -= border.left;
                y -= border.top;
                break;
            }
            current = parent;
        }
        crate::util::Point { x, y }
    }

    /// CSSOM View's `clientWidth`: the width of the padding box (border box minus
    /// borders and scrollbar)
    pub fn client_width(&self) -> f32 {
        let layout = self.final_layout();
        layout.size.width - layout.border.left - layout.border.right - layout.scrollbar_size.width
    }

    /// CSSOM View's `clientHeight`: the height of the padding box (border box minus
    /// borders and scrollbar)
    pub fn client_height(&self) -> f32 {
        let layout = self.final_layout();
        layout.size.height - layout.border.top - layout.border.bottom - layout.scrollbar_size.height
    }

    /// CSSOM View's `scrollWidth`: the width of the node's content, including
    /// content not visible due to overflow
    pub fn scroll_width(&self) -> f32 {
        self.client_width()
            .max(self.final_layout().scrollable_overflow_rect.right)
    }

    /// CSSOM View's `scrollHeight`: the height of the node's content, including
    /// content not visible due to overflow
    pub fn scroll_height(&self) -> f32 {
        self.client_height()
            .max(self.final_layout().scrollable_overflow_rect.bottom)
    }

    /// Does the node generate any boxes? (e.g. `getClientRects()` returns an empty
    /// list for boxless nodes, such as `display: none`, `display: contents`, or
    /// detached elements)
    pub fn has_boxes(&self) -> bool {
        self.flags.is_in_document()
            && !self.display_style().is_some_and(|display| {
                matches!(
                    display.inside(),
                    style::values::specified::box_::DisplayInside::None
                        | style::values::specified::box_::DisplayInside::Contents
                )
            })
    }

    /// Creates a synthetic click event
    pub fn synthetic_click_event(&self, mods: Modifiers) -> DomEventData {
        DomEventData::Click(self.synthetic_click_event_data(mods))
    }

    pub fn synthetic_click_event_data(&self, mods: Modifiers) -> BlitzPointerEvent {
        let absolute_position = self.absolute_position(0.0, 0.0);
        let x = absolute_position.x + (self.final_layout().size.width / 2.0);
        let y = absolute_position.y + (self.final_layout().size.height / 2.0);

        BlitzPointerEvent {
            id: BlitzPointerId::Mouse,
            is_primary: true,
            coords: PointerCoords {
                page_x: x,
                page_y: y,

                // TODO: should these be different?
                screen_x: x,
                screen_y: y,
                client_x: x,
                client_y: y,
            },
            mods,
            button: Default::default(),
            buttons: Default::default(),
            details: Default::default(),
            element: Default::default(),
            active_pointers: Default::default(),
        }
    }
}

/// It might be wrong to expose this since what does *equality* mean outside the dom?
impl PartialEq for Node {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}

impl Eq for Node {}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // FIXME: update to reflect changes to fields
        f.debug_struct("NodeData")
            .field("parent", &self.parent_id())
            .field("id", &self.id())
            .field("is_inline_root", &self.flags.is_inline_root())
            .field("children", &self.children())
            .field("layout_children", &self.layout_children())
            // .field("style", &self.style)
            .field("node", &self.data)
            .field("stylo_element_data", &self.try_stylo_element_data())
            // .field("unrounded_layout", &self.unrounded_layout)
            // .field("final_layout", &self.final_layout)
            .finish()
    }
}

#[cfg(test)]
mod test {
    use style_dom::ElementState;

    use crate::{Attribute, BaseDocument, DocumentConfig, ElementData, NodeData, qual_name};

    #[test]
    fn create_node_with_disabled_attr() {
        let mut document = BaseDocument::new(DocumentConfig::default());
        let node = document.create_node(NodeData::Element(crate::pool::PoolBox::new(ElementData::new(
            qual_name!("button"),
            vec![Attribute::new(qual_name!("disabled"), "")],
        ))));
        let node = document.get_node(node).unwrap();

        assert!(
            node.element_state().contains(ElementState::DISABLED),
            "form node is disabled"
        );
        assert!(
            !node.element_state().contains(ElementState::ENABLED),
            "form node is not enabled"
        );
    }

    #[test]
    fn ignore_disabled_attr_content() {
        let mut document = BaseDocument::new(DocumentConfig::default());
        let node = document.create_node(NodeData::Element(crate::pool::PoolBox::new(ElementData::new(
            qual_name!("button"),
            vec![Attribute::new(qual_name!("disabled"), "false")],
        ))));
        let node = document.get_node(node).unwrap();

        assert!(
            node.element_state().contains(ElementState::DISABLED),
            "form node is disabled"
        );
        assert!(
            !node.element_state().contains(ElementState::ENABLED),
            "form node is not enabled"
        );
    }

    #[test]
    fn create_node_with_ignored_disable() {
        let mut document = BaseDocument::new(DocumentConfig::default());
        let node = document.create_node(NodeData::Element(crate::pool::PoolBox::new(ElementData::new(
            qual_name!("a"),
            vec![Attribute::new(qual_name!("disabled"), "")],
        ))));
        let node = document.get_node(node).unwrap();

        assert!(
            !node.element_state().contains(ElementState::DISABLED),
            "Non form node cannot be disabled"
        );
        assert!(
            !node.element_state().contains(ElementState::ENABLED),
            "Non form node cannot be enabled"
        );
    }

    #[test]
    fn create_empty_enabled_node() {
        let mut document = BaseDocument::new(DocumentConfig::default());
        let node = document.create_node(NodeData::Element(crate::pool::PoolBox::new(ElementData::new(
            qual_name!("button"),
            vec![],
        ))));
        let node = document.get_node(node).unwrap();

        assert!(
            node.element_state().contains(ElementState::ENABLED),
            "Button should be enabled by default"
        );
    }
}

#[cfg(test)]
mod size_tests {
    // Every node, text included, carries a NodeData inline in the slab, so a rare large
    // variant must stay boxed or it widens every slot.
    #[test]
    fn node_data_stays_four_words() {
        let words = std::mem::size_of::<super::NodeData>() / std::mem::size_of::<usize>();
        assert!(words <= 4, "NodeData grew to {words} words");
    }
}
