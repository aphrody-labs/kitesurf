//! Enable the dom to participate in styling by servo
//!

use blitz_traits::node_id::NodeId;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use crate::StyleThreading;
use crate::layout::damage::{CONSTRUCT_BOX, compute_layout_damage};
use crate::node::Node;
use crate::node::NodeData;
use crate::stylo_to_parley::has_inline_background;
use markup5ever::{LocalName, LocalNameStaticSet, Namespace, NamespaceStaticSet, local_name, ns};
use selectors::bloom::BLOOM_HASH_MASK;
use selectors::{
    Element, OpaqueElement,
    attr::{AttrSelectorOperation, NamespaceConstraint},
    matching::{ElementSelectorFlags, MatchingContext, VisitedHandlingMode},
    sink::Push,
};
use style::CaseSensitivityExt;
use style::animation::AnimationSetKey;
use style::animation::AnimationState;
use style::applicable_declarations::ApplicableDeclarationBlock;
use style::bloom::each_relevant_element_hash;
use style::color::AbsoluteColor;
use style::data::{ElementDataFlags, ElementDataMut, ElementDataRef};
use style::global_style_data::STYLE_THREAD_POOL;
use style::invalidation::element::element_wrapper::ElementSnapshot;
use style::invalidation::element::invalidator::SiblingTraversalMap;
use style::invalidation::element::relative_selector::{
    DomMutationOperation, RelativeSelectorInvalidator,
};
use style::invalidation::element::restyle_hints::RestyleHint;
use style::properties::ComputedValues;
use style::properties::{Importance, PropertyDeclaration};
use style::rule_tree::CascadeLevel;
use style::rule_tree::CascadeOrigin;
use style::selector_parser::PseudoElement;
use style::selector_parser::RestyleDamage;
use style::stylesheets::layer_rule::LayerOrder;
use style::stylesheets::scope_rule::ImplicitScopeRoot;
use style::values::AtomString;
use style::values::specified::{NoCalcPercentage, XLang};
use style::{
    Atom,
    context::{
        QuirksMode, RegisteredSpeculativePainter, RegisteredSpeculativePainters,
        SharedStyleContext, StyleContext, ThreadLocalStyleContext,
    },
    dom::{LayoutIterator, NodeInfo, OpaqueNode, TDocument, TElement, TNode, TShadowRoot},
    global_style_data::GLOBAL_STYLE_DATA,
    properties::PropertyDeclarationBlock,
    selector_parser::{NonTSPseudoClass, SelectorImpl},
    servo_arc::{Arc, ArcBorrow},
    shared_lock::{Locked, SharedRwLock, StylesheetGuards},
    stylist::RuleInclusion,
    thread_state::ThreadState,
    traversal::{DomTraversal, resolve_style},
    traversal_flags::TraversalFlags,
    values::{AtomIdent, GenericAtomIdent},
};
use style_dom::ElementState;

use style::values::computed::text::TextAlign as StyloTextAlign;

impl crate::document::BaseDocument {
    pub fn resolve_stylist(&mut self, now: f64) {
        style::thread_state::enter(ThreadState::LAYOUT);

        // Rebuild scoped CascadeData for shadow roots whose <style>
        // set changed; invalidation was done wholesale at mutation.
        let dirty_roots = std::mem::take(&mut self.dirty_shadow_style_roots);
        for shadow_root_id in dirty_roots {
            let Some(node) = self.nodes.get_mut(shadow_root_id) else {
                continue;
            };
            let NodeData::ShadowRoot(ref mut data) = node.data else {
                continue;
            };
            // Steal the styles so stylist + styles can borrow disjointly.
            let mut styles = std::mem::replace(
                &mut data.author_styles,
                style::author_styles::AuthorStyles::new(),
            );
            let _ = styles.flush(&mut self.stylist, &self.guard.read());
            if let NodeData::ShadowRoot(ref mut data) = self.nodes[shadow_root_id].data {
                data.author_styles = styles;
            }
        }

        let guard = &self.guard;
        let guards = StylesheetGuards {
            author: &guard.read(),
            ua_or_user: &guard.read(),
        };

        let root = TDocument::as_node(&&self.nodes[self.root_node_id])
            .first_element_child()
            .unwrap()
            .as_element()
            .unwrap();

        self.stylist
            .flush(&guards)
            .process_style(root, Some(&self.snapshots));

        // Mark actively animating nodes as dirty
        let mut sets = self.animations.sets.write();
        for (key, set) in sets.iter_mut() {
            let node_id = self.nodes.id_from_opaque(key.node.id());

            // Drop animations belonging to nodes that are no longer in the
            // document. A removed element is never restyled, so it would never
            // get a chance to cancel its own animations; an infinite animation
            // would then keep `has_active_animations` set forever and force a
            // redraw every frame. Emptying the set here lets the `retain` below
            // discard it so the flag can clear on this same pass.
            let in_document = node_id
                .and_then(|id| self.nodes.get(id))
                .is_some_and(|node| node.flags.is_in_document());
            let Some(node_id) = node_id.filter(|_| in_document) else {
                set.animations.clear();
                set.transitions.clear();
                continue;
            };

            // Only a set with something left to tick can change what it
            // contributes; a finished fill or a paused animation cannot.

            // The hint also walks the ancestor chain, so an idle set costs a
            // recascade of itself plus that walk, on every single resolve.
            if set.needs_animation_ticks() {
                self.nodes[node_id].set_restyle_hint(RestyleHint::RESTYLE_SELF);
            }

            for animation in set.animations.iter_mut() {
                if animation.state == AnimationState::Pending && animation.started_at <= now {
                    animation.state = AnimationState::Running;
                }
                animation.iterate_if_necessary(now);

                if animation.state == AnimationState::Running && animation.has_ended(now) {
                    animation.state = AnimationState::Finished;
                }
            }

            for transition in set.transitions.iter_mut() {
                if transition.state == AnimationState::Pending && transition.start_time <= now {
                    transition.state = AnimationState::Running;
                }
                if transition.state == AnimationState::Running && transition.has_ended(now) {
                    transition.state = AnimationState::Finished;
                }
            }
        }
        drop(sets);

        self.invalidate_relative_selectors();

        // Build the style context used by the style traversal
        let context = SharedStyleContext {
            traversal_flags: TraversalFlags::empty(),
            stylist: &self.stylist,
            options: GLOBAL_STYLE_DATA.options.clone(),
            guards,
            visited_styles_enabled: false,
            animations: self.animations.clone(),
            current_time_for_animations: now,
            snapshot_map: &self.snapshots,
            registered_speculative_painters: &RegisteredPaintersImpl,
        };

        // components/layout_2020/lib.rs:983
        let root = self.root_element();
        // dbg!(root);
        let token = RecalcStyle::pre_traverse(root, &context);

        let mut nodes_needing_style_image_flush = Vec::new();
        if token.should_traverse() {
            // Style the elements, resolving their data
            let mut traverser = RecalcStyle::new(context);
            // `Sequential` bypasses Stylo's global pool. See `StyleThreading`.
            let pool_guard = matches!(self.style_threading, StyleThreading::Parallel)
                .then(|| STYLE_THREAD_POOL.pool());
            let rayon_pool = pool_guard.as_ref().and_then(|g| g.as_ref());
            style::driver::traverse_dom(&traverser, token, rayon_pool);
            nodes_needing_style_image_flush =
                std::mem::take(traverser.nodes_needing_style_image_flush.get_mut().unwrap());
        }
        self.pending_style_image_nodes
            .extend(nodes_needing_style_image_flush);

        for opaque in self.snapshots.keys() {
            if let Some(id) = self.nodes.id_from_opaque(opaque.id()) {
                self.nodes[id].set_has_snapshot(false);
            }
        }
        self.snapshots.clear();

        let mut sets = self.animations.sets.write();
        for set in sets.values_mut() {
            set.clear_canceled_animations();
            for animation in set.animations.iter_mut() {
                animation.is_new = false;
            }
            for transition in set.transitions.iter_mut() {
                transition.is_new = false;
            }
        }
        sets.retain(|_, state| !state.is_empty());
        self.has_active_animations = sets.values().any(|state| state.needs_animation_ticks());

        // Maybe run garbage collection. Stylo has internal to determine whether to run or not.
        self.stylist.rule_tree().maybe_gc();

        style::thread_state::exit(ThreadState::LAYOUT);
    }

    /// Compute the style of an element which the regular style traversal
    /// skipped because it lives inside a `display: none` subtree.
    ///
    /// The style traversal culls `display: none` subtrees, so such elements
    /// never have styles stored on them, but `getComputedStyle()` must still
    /// return their computed values. This resolves the style on demand
    /// (styling any unstyled ancestors along the way) without storing it on the
    /// node. Returns `None` for non-elements and elements outside the document.
    pub fn resolve_undisplayed_style(&self, node_id: NodeId) -> Option<Arc<ComputedValues>> {
        let node = self.nodes.get(node_id)?;
        if !node.is_element() || !node.flags.is_in_document() {
            return None;
        }

        style::thread_state::enter(ThreadState::LAYOUT);

        let guard = &self.guard;
        let guards = StylesheetGuards {
            author: &guard.read(),
            ua_or_user: &guard.read(),
        };
        let shared = SharedStyleContext {
            traversal_flags: TraversalFlags::empty(),
            stylist: &self.stylist,
            options: GLOBAL_STYLE_DATA.options.clone(),
            guards,
            visited_styles_enabled: false,
            animations: self.animations.clone(),
            current_time_for_animations: 0.0,
            snapshot_map: &self.snapshots,
            registered_speculative_painters: &RegisteredPaintersImpl,
        };
        let mut thread_local = ThreadLocalStyleContext::new();
        let mut context = StyleContext {
            shared: &shared,
            thread_local: &mut thread_local,
        };
        let styles = resolve_style(&mut context, node, RuleInclusion::All, None, None);

        style::thread_state::exit(ThreadState::LAYOUT);

        Some(styles.primary().clone())
    }

    /// Run stylo's relative-selector invalidator over this pass's snapshots so
    /// a `:has()` anchor is recascaded when its subject changes. Gecko drives
    /// this from C++ `RestyleManager`; blitz never did, so the anchor was only
    /// recascaded when it happened to fall inside the blunt `restyle_subtree()`
    /// that `set_attribute` stamped on the mutated node's parent.
    ///
    /// Must run before `traverse_dom`: `invalidated_self` writes RESTYLE_SELF
    /// into the anchor's `ElementData`, and the mutation already marked the
    /// root-to-node spine `dirty_descendants`, so the traversal reaches every
    /// ancestor and every sibling of an ancestor with no extra marking.
    fn invalidate_relative_selectors(&self) {
        if !self.has_relative_selectors() {
            return;
        }
        let quirks_mode = self.stylist.quirks_mode();

        for (opaque, snapshot) in self.snapshots.iter() {
            let Some(node) = self.nodes.id_from_opaque(opaque.id()).map(|id| &self.nodes[id]) else {
                continue;
            };
            let Some(element) = TNode::as_element(&node) else {
                continue;
            };

            // The snapshot holds the pre-mutation state, so the XOR is exactly
            // the bits a `:has(:hover)`-shaped dependency can have flipped.
            let state_changes = snapshot
                .state
                .map_or_else(ElementState::empty, |state| state ^ element.state());
            let has_attrs = snapshot.has_attrs();
            let snapshot_id = if has_attrs { snapshot.id_attr() } else { None };

            RelativeSelectorInvalidator {
                element,
                quirks_mode,
                snapshot_table: Some(&self.snapshots),
                invalidated: |_, _| {},
                sibling_traversal_map: SiblingTraversalMap::default(),
                _marker: PhantomData,
            }
            .invalidate_relative_selectors_for_this(
                &self.stylist,
                |element, scope, data, quirks_mode, collector| {
                    let map = data.relative_selector_invalidation_map();

                    // A key the mutation removed is already gone from the
                    // element but still in the snapshot, and vice versa.

                    // Both sides have to be looked up, or the anchor latches on
                    // and never invalidates back when the subject goes away.
                    for id in element.id().into_iter().chain(snapshot_id) {
                        for dep in map.id_to_selector.get(id, quirks_mode).into_iter().flatten() {
                            collector.add_dependency(dep, *element, scope);
                        }
                    }

                    let mut each_class = |class: &AtomIdent| {
                        for dep in map
                            .class_to_selector
                            .get(class, quirks_mode)
                            .into_iter()
                            .flatten()
                        {
                            collector.add_dependency(dep, *element, scope);
                        }
                    };
                    element.each_class(&mut each_class);
                    if has_attrs {
                        snapshot.each_class(&mut each_class);
                    }

                    let mut each_attr = |name: &GenericAtomIdent<LocalNameStaticSet>| {
                        for dep in map
                            .other_attribute_affecting_selectors
                            .get(name)
                            .into_iter()
                            .flatten()
                        {
                            collector.add_dependency(dep, *element, scope);
                        }
                    };
                    element.each_attr_name(&mut each_attr);

                    // Unguarded on purpose, unlike id_attr/each_class above:
                    // those are trait methods whose `get_attr` unwraps
                    // `attrs` and would panic on a state-only snapshot, while
                    // this one just walks the `changed_attrs` vec.
                    snapshot.each_attr_changed(&mut each_attr);

                    if state_changes.is_empty() {
                        return;
                    }
                    map.state_affecting_selectors.lookup_with_additional(
                        *element,
                        quirks_mode,
                        None,
                        &[],
                        state_changes,
                        |dep| {
                            if dep.state.intersects(state_changes) {
                                collector.add_dependency(&dep.dep, *element, scope);
                            }
                            true
                        },
                    );
                },
            );
        }
    }

    /// Cheap gate for both relative-selector passes: a document with no
    /// `:has()` anywhere should not pay for the invalidator at all.
    ///
    /// `iter_origins` only sees document-level origins, so any shadow root
    /// forces the slow path rather than risk missing a shadow `:has()`.
    fn has_relative_selectors(&self) -> bool {
        !self.shadow_roots.is_empty()
            || self
                .stylist
                .iter_origins()
                .any(|(data, _)| data.relative_invalidation_map_attributes().used)
    }

    /// Structural sibling of the pass above, for tree mutations rather than
    /// attribute and state ones. Snapshots only exist for the latter, so an
    /// insert or remove has to invalidate at mutation time.
    ///
    /// Ordering is forced by the collector reading the node's live parent and
    /// siblings: insertions invalidate after the node is linked, removals
    /// before it is unlinked.
    fn invalidate_relative_selectors_for_dom_mutation(
        &self,
        node_id: NodeId,
        operation: DomMutationOperation,
    ) {
        if !self.has_relative_selectors() {
            return;
        }
        let Some(node) = self.nodes.get(node_id) else {
            return;
        };
        let Some(element) = TNode::as_element(&node) else {
            return;
        };

        // A node that never took part in selector matching carries no search
        // direction, and `DomMutationOperation::accept` rejects those outright.

        // Seeding it from the parent is what makes a freshly created element
        // invalidate at all, and mirrors what matching would have stamped.
        let inherited_search_path = selectors::Element::parent_element(&element)
            .map_or_else(ElementSelectorFlags::empty, |parent| {
                parent.relative_selector_search_direction()
            });

        RelativeSelectorInvalidator {
            element,
            quirks_mode: self.stylist.quirks_mode(),
            snapshot_table: Some(&self.snapshots),
            // The invalidator propagates `dirty_descendants` from what it
            // restyles up to the anchor only. `set_dirty_descendants` does not
            // mark ancestors, so the anchor's own ancestors stay clean
            // unless this marks them. A removal runs this before unlinking, and
            // its own `mark_ancestors_dirty` from the parent then stops at the
            // already-set anchor, so the traversal never reached the subject.
            invalidated: |anchor, result| {
                if result.has_invalidated_self()
                    || result.has_invalidated_descendants()
                    || result.has_invalidated_siblings()
                {
                    anchor.mark_ancestors_dirty();
                }
            },
            sibling_traversal_map: SiblingTraversalMap::default(),
            _marker: PhantomData,
        }
        // `subtree`: the node may carry a whole subtree containing the
        // subject, and upstream still gates the walk on the map's own flag.
        .invalidate_relative_selectors_for_dom_mutation(
            true,
            &self.stylist,
            inherited_search_path,
            operation,
        );
    }

    pub(crate) fn invalidate_relative_selectors_for_removal(&self, node_id: NodeId) {
        self.invalidate_relative_selectors_for_dom_mutation(node_id, DomMutationOperation::Remove);
    }

    pub(crate) fn invalidate_relative_selectors_for_insertion(&self, node_id: NodeId) {
        // `Append` only unlocks an optimization that assumes a later sibling
        // can take over the match, so `Insert` is the conservative choice.
        self.invalidate_relative_selectors_for_dom_mutation(node_id, DomMutationOperation::Insert);
    }
}

/// A handle to a node that Servo's style traits are implemented against
///
/// Since BlitzNodes are not persistent (IE we don't keep the pointers around between frames), we choose to just implement
/// the tree structure in the nodes themselves, and temporarily give out pointers during the layout phase.
type BlitzNode<'a> = &'a Node;

impl<'a> TDocument for BlitzNode<'a> {
    type ConcreteNode = BlitzNode<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        self
    }

    fn is_html_document(&self) -> bool {
        true
    }

    fn quirks_mode(&self) -> QuirksMode {
        QuirksMode::NoQuirks
    }

    fn shared_lock(&self) -> &SharedRwLock {
        self.guard()
    }
}

impl NodeInfo for BlitzNode<'_> {
    fn is_element(&self) -> bool {
        Node::is_element(self)
    }

    fn is_text_node(&self) -> bool {
        Node::is_text_node(self)
    }
}

impl<'a> TShadowRoot for BlitzNode<'a> {
    type ConcreteNode = BlitzNode<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        self
    }

    fn host(&self) -> <Self::ConcreteNode as TNode>::ConcreteElement {
        let host_id = self
            .shadow_root_data()
            .expect("host() called on a non-shadow-root node")
            .host;
        self.with(host_id)
    }

    fn style_data<'b>(&self) -> Option<&'b style::stylist::CascadeData>
    where
        Self: 'b,
    {
        // Self: 'b bounds 'a: 'b, so handing out the node's cascade
        // data at 'b is sound (it lives in the slab entry itself).
        let node: &'b Node = self;
        node.shadow_root_data().map(|d| &*d.author_styles.data)
    }
}

// components/styleaapper.rs:
impl<'a> TNode for BlitzNode<'a> {
    type ConcreteElement = BlitzNode<'a>;
    type ConcreteDocument = BlitzNode<'a>;
    type ConcreteShadowRoot = BlitzNode<'a>;

    fn parent_node(&self) -> Option<Self> {
        self.parent.map(|id| self.with(id))
    }

    fn first_child(&self) -> Option<Self> {
        self.children.first().map(|id| self.with(*id))
    }

    fn last_child(&self) -> Option<Self> {
        self.children.last().map(|id| self.with(*id))
    }

    fn prev_sibling(&self) -> Option<Self> {
        self.backward(1)
    }

    fn next_sibling(&self) -> Option<Self> {
        self.forward(1)
    }

    fn owner_doc(&self) -> Self::ConcreteDocument {
        // Walk up the (layout-)parent chain to the root Document node.
        let mut node = *self;
        while let Some(parent_id) = node.parent {
            node = node.with(parent_id);
        }
        node
    }

    fn is_in_document(&self) -> bool {
        true
    }

    // Flat-tree parent: assigned nodes hang off their slot, shadow
    // top-levels off the host (the shadow-root node is transparent).
    fn traversal_parent(&self) -> Option<Self::ConcreteElement> {
        if let Some(slot_id) = self.assigned_slot {
            return Some(self.with(slot_id));
        }
        let parent = self.parent_node()?;
        match &parent.data {
            NodeData::ShadowRoot(data) => Some(parent.with(data.host)),
            _ => parent.as_element(),
        }
    }

    fn opaque(&self) -> OpaqueNode {
        OpaqueNode(self.id.as_u64() as usize)
    }

    fn debug_id(self) -> usize {
        self.id.as_u64() as usize
    }

    fn as_element(&self) -> Option<Self::ConcreteElement> {
        match self.data {
            NodeData::Element { .. } => Some(self),
            _ => None,
        }
    }

    fn as_document(&self) -> Option<Self::ConcreteDocument> {
        match self.data {
            NodeData::Document(_) => Some(self),
            _ => None,
        }
    }

    fn as_shadow_root(&self) -> Option<Self::ConcreteShadowRoot> {
        match self.data {
            NodeData::ShadowRoot { .. } => Some(self),
            _ => None,
        }
    }
}

impl selectors::Element for BlitzNode<'_> {
    type Impl = SelectorImpl;

    fn opaque(&self) -> selectors::OpaqueElement {
        // This correctly uses a unique id for the OpaqueElement (unlike using a pointer to the "slot")
        // However, it makes it impossible for us to "rehydrate" the OpaqueElement back into an actual Element
        // which is required to implement the `implicit_scope_for_sheet_in_shadow_root` method below
        //
        // We should see if selectors will accept a PR that allows us to use 128bits for the OpaqueElement. Or
        // find some other solution that will enable "rehydration". This is required to enable and use the
        // Shadow DOM functionality in Stylo.
        let non_null =
            NonNull::new((self.id.as_u64() as usize).wrapping_add(1) as *mut ()).unwrap();
        OpaqueElement::from_non_null_ptr(non_null)
    }

    // Selector matching stays inside the current tree: shadow-tree
    // top-levels have a null parentElement (traversal_parent crosses).
    fn parent_element(&self) -> Option<Self> {
        self.parent_node().and_then(|node| node.as_element())
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        self.parent
            .is_some_and(|id| self.with(id).is_shadow_root())
    }

    // ::slotted() matching walks this edge (selectors crate default
    // is None, which disables slotted rules entirely).
    fn assigned_slot(&self) -> Option<Self> {
        // Field access (no parens) reads Node's stored assignment,
        // dodging the method-name collision with this override.
        self.assigned_slot.map(|id| self.with(id))
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        TElement::containing_shadow(self).map(|root| TShadowRoot::host(&root))
    }

    fn is_pseudo_element(&self) -> bool {
        matches!(self.data, NodeData::AnonymousBlock(_))
    }

    // These methods are implemented naively since we only threaded real nodes and not fake nodes
    // we should try and use `find` instead of this foward/backward stuff since its ugly and slow
    fn prev_sibling_element(&self) -> Option<Self> {
        let mut n = 1;
        while let Some(node) = self.backward(n) {
            if node.is_element() {
                return Some(node);
            }
            n += 1;
        }

        None
    }

    fn next_sibling_element(&self) -> Option<Self> {
        let mut n = 1;
        while let Some(node) = self.forward(n) {
            if node.is_element() {
                return Some(node);
            }
            n += 1;
        }

        None
    }

    fn first_element_child(&self) -> Option<Self> {
        let mut children = self.dom_children();
        children.find(|child| child.is_element())
    }

    fn is_html_element_in_html_document(&self) -> bool {
        true // self.has_namespace(ns!(html))
    }

    fn has_local_name(&self, local_name: &LocalName) -> bool {
        self.data.is_element_with_tag_name(local_name)
    }

    fn has_namespace(&self, ns: &Namespace) -> bool {
        self.element_data().expect("Not an element").name.ns == *ns
    }

    fn is_same_type(&self, other: &Self) -> bool {
        self.local_name() == other.local_name() && self.namespace() == other.namespace()
    }

    fn attr_matches(
        &self,
        ns: &NamespaceConstraint<&GenericAtomIdent<NamespaceStaticSet>>,
        local_name: &GenericAtomIdent<LocalNameStaticSet>,
        operation: &AttrSelectorOperation<&AtomString>,
    ) -> bool {
        let Some(attrs) = self.data.attrs() else {
            return false;
        };
        attrs.iter().any(|attr| {
            attr.name.local == local_name.0
                && match ns {
                    NamespaceConstraint::Any => true,
                    NamespaceConstraint::Specific(ns) => attr.name.ns == ns.0,
                }
                && operation.eval_str(&attr.value)
        })
    }

    fn match_non_ts_pseudo_class(
        &self,
        pseudo_class: &<Self::Impl as selectors::SelectorImpl>::NonTSPseudoClass,
        _context: &mut MatchingContext<Self::Impl>,
    ) -> bool {
        match *pseudo_class {
            NonTSPseudoClass::Active => self.element_state().contains(ElementState::ACTIVE),
            // :dir() matches HTML directionality (not the
            // CSS direction property): unknown idents never match.
            NonTSPseudoClass::Dir(ref dir) => {
                let rtl = self.html_directionality_is_rtl();
                match &**dir {
                    "rtl" => rtl,
                    "ltr" => !rtl,
                    _ => false,
                }
            }
            NonTSPseudoClass::AnyLink => self
                .element_state()
                .intersects(ElementState::VISITED_OR_UNVISITED),
            NonTSPseudoClass::Checked => {
                // Two-tier: layout-tracked state first (covers the
                // .checked IDL property mutation path that real
                // pages use), fall back to the content attribute
                // (covers detached/Fragment elements where layout
                // never ran: the WPT
                // ParentNode-querySelector-All "matching checked
                // radio buttons and checkboxes" pattern).
                //
                // Per HTML §dom-checked-pseudo:
                //   - input[type=checkbox|radio] is :checked when
                //     the *checkedness* is true. Checkedness starts
                //     from the `checked` content attribute and is
                //     then mutated by .checked = ... / user input.
                //   - option is :checked when the `selected` IDL
                //     state is true.
                //
                // SpecialElementData::CheckboxInput tracks live
                // checkedness once the element enters layout, so
                // it's authoritative for rendered inputs. Detached
                // / Fragment-rooted elements never get that data
                // populated, so we fall back to the content attr.
                let elem = self.data.downcast_element();
                if let Some(elem) = elem {
                    if elem.name.local == local_name!("input") {
                        let type_attr = elem
                            .attr(local_name!("type"))
                            .unwrap_or("text");
                        let is_checkable = type_attr.eq_ignore_ascii_case("checkbox")
                            || type_attr.eq_ignore_ascii_case("radio");
                        if !is_checkable {
                            false
                        } else if let Some(state) = elem.checkbox_input_checked() {
                            // Live layout-tracked state. Real pages.
                            state
                        } else {
                            // Detached / unrendered. WPT pattern.
                            elem.attr(local_name!("checked")).is_some()
                        }
                    } else if elem.name.local == local_name!("option") {
                        elem.attr(local_name!("selected")).is_some()
                    } else {
                        false
                    }
                } else {
                    false
                }
            },
            NonTSPseudoClass::Valid => false,
            NonTSPseudoClass::Invalid => false,
            NonTSPseudoClass::Defined => false,
            NonTSPseudoClass::Disabled => self.element_state().contains(ElementState::DISABLED),
            NonTSPseudoClass::Enabled => self.element_state().contains(ElementState::ENABLED),
            NonTSPseudoClass::Focus => self.element_state().contains(ElementState::FOCUS),
            NonTSPseudoClass::FocusWithin => false,
            NonTSPseudoClass::FocusVisible => false,
            NonTSPseudoClass::Fullscreen => false,
            NonTSPseudoClass::Hover => self.element_state().contains(ElementState::HOVER),
            NonTSPseudoClass::Indeterminate => false,
            NonTSPseudoClass::Lang(ref lang_arg) => {
                // Walk self + ancestors looking for the nearest `lang`
                // attribute. BCP-47 prefix-match (case-insensitive).
                let arg = lang_arg.as_ref();
                if arg.is_empty() {
                    false
                } else {
                    let mut cur = Some(*self);
                    let effective_lang = loop {
                        let Some(node) = cur else { break None };
                        if let Some(value) = node.data.attr(local_name!("lang")) {
                            break Some(value);
                        }
                        cur = node.parent_node();
                    };
                    match effective_lang {
                        None => false,
                        Some(lang) => {
                            let lang_b = lang.as_bytes();
                            let arg_b = arg.as_bytes();
                            lang_b.eq_ignore_ascii_case(arg_b)
                                || (lang_b.len() > arg_b.len()
                                    && lang_b[arg_b.len()] == b'-'
                                    && lang_b[..arg_b.len()].eq_ignore_ascii_case(arg_b))
                        }
                    }
                }
            },
            NonTSPseudoClass::CustomState(_) => false,
            NonTSPseudoClass::Link => self.element_state().contains(ElementState::UNVISITED),
            NonTSPseudoClass::PlaceholderShown => false,
            NonTSPseudoClass::ReadWrite => false,
            NonTSPseudoClass::ReadOnly => false,
            NonTSPseudoClass::ServoNonZeroBorder => false,
            NonTSPseudoClass::Target => false,
            NonTSPseudoClass::Visited => false,
            NonTSPseudoClass::Autofill => false,
            NonTSPseudoClass::Default => false,

            NonTSPseudoClass::InRange => false,
            NonTSPseudoClass::Modal => false,
            NonTSPseudoClass::Open => false,
            NonTSPseudoClass::Optional => false,
            NonTSPseudoClass::OutOfRange => false,
            NonTSPseudoClass::PopoverOpen => false,
            NonTSPseudoClass::Required => false,
            NonTSPseudoClass::UserInvalid => false,
            NonTSPseudoClass::UserValid => false,
            NonTSPseudoClass::MozMeterOptimum => false,
            NonTSPseudoClass::MozMeterSubOptimum => false,
            NonTSPseudoClass::MozMeterSubSubOptimum => false,
        }
    }

    fn match_pseudo_element(
        &self,
        pe: &PseudoElement,
        _context: &mut MatchingContext<Self::Impl>,
    ) -> bool {
        let pseudo = match self.try_stylo_element_data().and_then(|s| s.get()) {
            Some(el) => el
                .styles
                .get_primary()
                .and_then(|s| s.pseudo())
                .or(match &self.data {
                    NodeData::AnonymousBlock(_) => Some(PseudoElement::ServoAnonymousBox),
                    _ => None,
                }),
            None => None,
        };

        pseudo.is_some_and(|psuedo| psuedo == *pe)
    }

    fn apply_selector_flags(&self, flags: ElementSelectorFlags) {
        // Handle flags that apply to the element.
        let self_flags = flags.for_self();
        if !self_flags.is_empty() {
            self.selector_flags()
                .set(self.selector_flags().get() | self_flags);
        }

        // Handle flags that apply to the parent.
        let parent_flags = flags.for_parent();
        if !parent_flags.is_empty() {
            // Shadow roots hold no selector flags, and nothing here reads parent
            // flags: mutations under a root restyle through the host's subtree hint.
            if let Some(parent) = self.parent_node().filter(|p| !p.is_shadow_root()) {
                parent
                    .selector_flags()
                    .set(parent.selector_flags().get() | parent_flags);
            }
        }
    }

    fn is_link(&self) -> bool {
        self.data.is_element_with_tag_name(&local_name!("a"))
    }

    // The selectors crate asserts assigned_slot() targets satisfy
    // this; ::slotted() matching debug-panics without it.
    fn is_html_slot_element(&self) -> bool {
        self.is_slot_element()
    }

    fn has_id(
        &self,
        id: &<Self::Impl as selectors::SelectorImpl>::Identifier,
        case_sensitivity: selectors::attr::CaseSensitivity,
    ) -> bool {
        self.element_data()
            .and_then(|data| data.id.as_ref())
            .map(|id_attr| case_sensitivity.eq_atom(id_attr, id))
            .unwrap_or(false)
    }

    fn has_class(
        &self,
        search_name: &<Self::Impl as selectors::SelectorImpl>::Identifier,
        case_sensitivity: selectors::attr::CaseSensitivity,
    ) -> bool {
        // Read the pre-interned list, never re-split the attribute: this is
        // hit once per class selector per candidate element during matching.
        self.element_data().is_some_and(|data| {
            data.classes
                .iter()
                .any(|class| case_sensitivity.eq_atom(class, search_name))
        })
    }

    fn imported_part(
        &self,
        _name: &<Self::Impl as selectors::SelectorImpl>::Identifier,
    ) -> Option<<Self::Impl as selectors::SelectorImpl>::Identifier> {
        None
    }

    fn is_part(&self, _name: &<Self::Impl as selectors::SelectorImpl>::Identifier) -> bool {
        false
    }

    fn is_empty(&self) -> bool {
        // Per CSS Selectors Level 4 + WPT (matching Chrome/Firefox):
        // comments do not count as content for `:empty`.
        self.dom_children()
            .all(|c| matches!(c.data, NodeData::Comment { .. }))
    }

    fn is_root(&self) -> bool {
        // Per CSS Selectors Level 4 `:root` matches the root element;
        // the element whose parent is a Document node. Detached
        // elements and children of detached subtrees must not match.
        match self.parent_node() {
            Some(parent) => matches!(parent.data, NodeData::Document(_)),
            None => false,
        }
    }

    fn has_custom_state(
        &self,
        _name: &<Self::Impl as selectors::SelectorImpl>::Identifier,
    ) -> bool {
        false
    }

    fn add_element_unique_hashes(&self, filter: &mut selectors::bloom::BloomFilter) -> bool {
        each_relevant_element_hash(*self, |hash| filter.insert_hash(hash & BLOOM_HASH_MASK));
        true
    }
}

impl<'a> TElement for BlitzNode<'a> {
    type ConcreteNode = BlitzNode<'a>;

    type TraversalChildrenIterator = Traverser<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        self
    }

    fn implicit_scope_for_sheet_in_shadow_root(
        _opaque_host: OpaqueElement,
        _sheet_index: usize,
    ) -> Option<ImplicitScopeRoot> {
        // Unimplementable while OpaqueElement is a bare NodeId (no slab
        // to rehydrate through): see the comment on `opaque()` above.

        // None = no implicit @scope root; only misdirects @scope rules
        // inside shadow sheets, vastly better than a todo!() panic.
        None
    }

    // Flat tree: the Traverser reads flat_children(), so hosts yield
    // shadow children, slots their assigned nodes, others DOM children.
    fn traversal_children(&self) -> style::dom::LayoutIterator<Self::TraversalChildrenIterator> {
        LayoutIterator(Traverser {
            // dom: self.tree(),
            parent: self,
            child_index: 0,
        })
    }

    fn is_html_element(&self) -> bool {
        self.is_element()
    }

    // not implemented.....
    fn is_mathml_element(&self) -> bool {
        false
    }

    // need to check the namespace
    fn is_svg_element(&self) -> bool {
        false
    }

    // Style inheritance follows the flat tree (parent_element stops
    // at the shadow root by design; inheritance must cross to host).
    fn inheritance_parent(&self) -> Option<Self> {
        TElement::traversal_parent(self)
    }

    fn style_attribute(&self) -> Option<ArcBorrow<'_, Locked<PropertyDeclarationBlock>>> {
        self.element_data()
            .expect("Not an element")
            .style_attribute
            .as_ref()
            .map(|f| f.borrow_arc())
    }

    fn state(&self) -> ElementState {
        *self.element_state()
    }

    fn has_part_attr(&self) -> bool {
        false
    }

    fn exports_any_part(&self) -> bool {
        false
    }

    fn id(&self) -> Option<&style::Atom> {
        self.element_data().and_then(|data| data.id.as_ref())
    }

    fn each_class<F>(&self, mut callback: F)
    where
        F: FnMut(&style::values::AtomIdent),
    {
        // Read the pre-interned list rather than re-splitting: rule
        // bucketing, the style bloom and revalidation each land here.
        if let Some(data) = self.element_data() {
            for class in &data.classes {
                callback(AtomIdent::cast(class));
            }
        }
    }

    fn each_attr_name<F>(&self, mut callback: F)
    where
        F: FnMut(&style::LocalName),
    {
        if let Some(attrs) = self.data.attrs() {
            for attr in attrs.iter() {
                callback(&GenericAtomIdent(attr.name.local.clone()));
            }
        }
    }

    fn has_dirty_descendants(&self) -> bool {
        Node::has_dirty_descendants(self)
    }

    fn has_snapshot(&self) -> bool {
        Node::has_snapshot(self)
    }

    fn handled_snapshot(&self) -> bool {
        self.snapshot_handled().load(Ordering::SeqCst)
    }

    unsafe fn set_handled_snapshot(&self) {
        self.snapshot_handled().store(true, Ordering::SeqCst);
    }

    // Stylo calls this on elements it is already traversing (or has already
    // reached via invalidation), so the ancestor chain must not be re-marked:
    // ancestors visited earlier in the preorder traversal have had their bits
    // cleared, and re-flagging them leaves stale `dirty_descendants` bits that
    // break the early-out invariant of `Node::mark_ancestors_dirty` (a set bit
    // implies all ancestors are set).
    unsafe fn set_dirty_descendants(&self) {
        Node::set_dirty_descendants(self);
    }

    unsafe fn unset_dirty_descendants(&self) {
        Node::unset_dirty_descendants(self);
    }

    fn store_children_to_process(&self, _n: isize) {
        unimplemented!()
    }

    fn did_process_child(&self) -> isize {
        unimplemented!()
    }

    unsafe fn ensure_data(&self) -> ElementDataMut<'_> {
        // SAFETY: stylo traversal has exclusive access to nodes
        unsafe { self.stylo_element_data().ensure_init() }
    }

    unsafe fn clear_data(&self) {
        // SAFETY: stylo traversal has exclusive access to nodes
        unsafe { self.stylo_element_data().clear() }
    }

    fn has_data(&self) -> bool {
        self.try_stylo_element_data().is_some_and(|s| s.has_data())
    }

    fn borrow_data(&self) -> Option<ElementDataRef<'_>> {
        self.try_stylo_element_data().and_then(|s| s.get())
    }

    fn mutate_data(&self) -> Option<ElementDataMut<'_>> {
        unsafe { self.stylo_element_data().unsafe_stylo_only_mut() }
    }

    fn skip_item_display_fixup(&self) -> bool {
        false
    }

    fn may_have_animations(&self) -> bool {
        true
    }

    fn has_animations(&self, context: &SharedStyleContext) -> bool {
        self.has_css_animations(context, None) || self.has_css_transitions(context, None)
    }

    fn has_css_animations(
        &self,
        context: &SharedStyleContext,
        pseudo_element: Option<PseudoElement>,
    ) -> bool {
        let key = AnimationSetKey::new(TNode::opaque(&TElement::as_node(self)), pseudo_element);
        context.animations.has_active_animations(&key)
    }

    fn has_css_transitions(
        &self,
        context: &SharedStyleContext,
        pseudo_element: Option<PseudoElement>,
    ) -> bool {
        let key = AnimationSetKey::new(TNode::opaque(&TElement::as_node(self)), pseudo_element);
        context.animations.has_active_transitions(&key)
    }

    fn animation_rule(
        &self,
        context: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        let opaque = TNode::opaque(&TElement::as_node(self));
        context.animations.get_animation_declarations(
            &AnimationSetKey::new_for_non_pseudo(opaque),
            context.current_time_for_animations,
            self.guard(),
        )
    }

    fn transition_rule(
        &self,
        context: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        let opaque = TNode::opaque(&TElement::as_node(self));
        context.animations.get_transition_declarations(
            &AnimationSetKey::new_for_non_pseudo(opaque),
            context.current_time_for_animations,
            self.guard(),
        )
    }

    fn shadow_root(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        self.attached_shadow_root_id().map(|id| self.with(id))
    }

    // Walking Node::parent terminates: a shadow subtree's chain hits
    // its shadow-root node before ever reaching the host's ancestors.
    fn containing_shadow(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        let mut current = self.parent_node();
        while let Some(node) = current {
            if node.is_shadow_root() {
                return Some(node);
            }
            current = node.parent_node();
        }
        None
    }

    fn get_attr(&self, attr: &style::LocalName, ns: &style::Namespace) -> Option<String> {
        // TODO: case-insensitive matching for HTML-ns attrs
        self.attrs()?
            .iter()
            .find(|a| a.name.local == attr.0 && a.name.ns == ns.0)
            .map(|a| a.value.clone())
    }

    fn lang_attr(&self) -> Option<style::selector_parser::AttrValue> {
        None
    }

    fn match_element_lang(
        &self,
        _override_lang: Option<Option<style::selector_parser::AttrValue>>,
        _value: &style::selector_parser::Lang,
    ) -> bool {
        false
    }

    fn is_html_document_body_element(&self) -> bool {
        // Check node is a <body> element
        let is_body_element = self.data.is_element_with_tag_name(&local_name!("body"));

        // If it isn't then return early
        if !is_body_element {
            return false;
        }

        // If it is then check if it is a child of the root (<html>) element
        let root_node = TNode::owner_doc(self);
        let root_element = TDocument::as_node(&root_node)
            .first_element_child()
            .unwrap();
        root_element.children.contains(&self.id)
    }

    fn synthesize_presentational_hints_for_legacy_attributes<V>(
        &self,
        _visited_handling: VisitedHandlingMode,
        hints: &mut V,
    ) where
        V: Push<style::applicable_declarations::ApplicableDeclarationBlock>,
    {
        let Some(elem) = self.data.downcast_element() else {
            return;
        };

        let tag = &elem.name.local;

        let mut push_style = |decl: PropertyDeclaration| {
            hints.push(ApplicableDeclarationBlock::from_declarations(
                Arc::new(
                    self.guard()
                        .wrap(PropertyDeclarationBlock::with_one(decl, Importance::Normal)),
                ),
                CascadeLevel::new(CascadeOrigin::PresHints),
                LayerOrder::root(),
            ));
        };

        // Dir=auto can't ride a stylesheet (its scan skips
        // subtrees no selector can express): resolve it here so
        // getComputedStyle().direction and flat-tree inheritance see
        // the result. ltr/rtl attrs stay on the UA [dir] rules.
        {
            use style::properties::longhands::direction::SpecifiedValue as Direction;
            let dir_attr = elem.attr(local_name!("dir"));
            let has_fixed = dir_attr.is_some_and(|d| {
                d.eq_ignore_ascii_case("ltr") || d.eq_ignore_ascii_case("rtl")
            });
            let dir_auto = dir_attr.is_some_and(|d| d.eq_ignore_ascii_case("auto"))
                || (*tag == local_name!("bdi") && !has_fixed);
            if dir_auto {
                let rtl = self.dir_auto_scan_is_rtl().unwrap_or(false);
                push_style(PropertyDeclaration::Direction(if rtl {
                    Direction::Rtl
                } else {
                    Direction::Ltr
                }));
            }
        }

        fn parse_color_attr(value: &str) -> Option<(u8, u8, u8, f32)> {
            if !value.starts_with('#') {
                return None;
            }

            let value = &value[1..];
            if value.len() == 3 {
                let r = u8::from_str_radix(&value[0..1], 16).ok()?;
                let g = u8::from_str_radix(&value[1..2], 16).ok()?;
                let b = u8::from_str_radix(&value[2..3], 16).ok()?;
                return Some((r, g, b, 1.0));
            }

            if value.len() == 6 {
                let r = u8::from_str_radix(&value[0..2], 16).ok()?;
                let g = u8::from_str_radix(&value[2..4], 16).ok()?;
                let b = u8::from_str_radix(&value[4..6], 16).ok()?;
                return Some((r, g, b, 1.0));
            }

            None
        }

        /// The HTML "rules for parsing dimension values" -- Stylo's
        /// implementation of them -- packaged as a specified
        /// `<length-percentage>`. `ignoring_zero` selects the spec's separate
        /// "maps to the dimension property (ignoring zero)" mapping, where a
        /// zero value is dropped rather than honoured.
        ///
        /// https://html.spec.whatwg.org/multipage/common-microsyntaxes.html#rules-for-parsing-dimension-values
        fn parse_size_attr(
            value: &str,
            ignoring_zero: bool,
        ) -> Option<style::values::specified::LengthPercentage> {
            use style::servo::attr::{
                LengthOrPercentageOrAuto, parse_length, parse_nonzero_length,
            };
            use style::values::specified::{LengthPercentage, NoCalcLength};
            let parsed = if ignoring_zero {
                parse_nonzero_length(value)
            } else {
                parse_length(value)
            };
            match parsed {
                LengthOrPercentageOrAuto::Length(length) => Some(LengthPercentage::Length(
                    NoCalcLength::from_px(length.to_f32_px()),
                )),
                LengthOrPercentageOrAuto::Percentage(fraction) => Some(
                    LengthPercentage::Percentage(NoCalcPercentage::new(fraction)),
                ),
                LengthOrPercentageOrAuto::Auto => None,
            }
        }

        /// Parse the value of an SVG `width`/`height` presentation attribute.
        /// Unlike the legacy HTML dimension attributes, these accept any CSS
        /// <length-percentage> (e.g. `1em`), and a unitless number means user
        /// units, which map to CSS px.
        fn parse_svg_size_attr(value: &str) -> Option<style::values::specified::LengthPercentage> {
            use style::values::specified::{LengthPercentage, NoCalcLength};
            use style_traits::ParsingMode;

            let value = value.trim();
            if let Some(number) = value.strip_suffix('%') {
                let val: f32 = number.trim().parse().ok()?;
                return (val >= 0.0)
                    .then(|| LengthPercentage::Percentage(NoCalcPercentage::new(val / 100.0)));
            }

            // Split into number and unit: the unit is the trailing run of
            // ASCII alphabetic characters (this never eats into a scientific
            // exponent such as `1e3`, which ends in a digit).
            let number_len = value
                .trim_end_matches(|c: char| c.is_ascii_alphabetic())
                .len();
            let (number, unit) = value.split_at(number_len);
            let val: f32 = number.trim().parse().ok().filter(|v| *v >= 0.0)?;
            let length = if unit.is_empty() {
                NoCalcLength::from_px(val)
            } else {
                NoCalcLength::parse_dimension_with_flags(ParsingMode::DEFAULT, false, val, unit)
                    .ok()?
            };
            Some(LengthPercentage::Length(length))
        }

        // `<input type=image>` is the only input type that is replaced
        // content, and it is the only one that takes the embedded-content
        // presentational attributes. The type attribute is matched ASCII
        // case-insensitively, as attribute keywords always are.
        let is_image_input = *tag == local_name!("input")
            && elem.attrs().iter().any(|attr| {
                attr.name.local == local_name!("type") && attr.value.eq_ignore_ascii_case("image")
            });

        // https://html.spec.whatwg.org/multipage/#the-lang-and-xml:lang-attributes
        let accepts_lang = elem.name.ns == ns!(html) || elem.name.ns == ns!(svg);
        let mut lang = None;

        for attr in elem.attrs() {
            let name = &attr.name.local;
            let value = attr.value.as_str();

            if *name == local_name!("lang") {
                if attr.name.ns == ns!(xml) {
                    lang = Some(value);
                } else if attr.name.ns == ns!() && accepts_lang {
                    lang = lang.or(Some(value));
                }
            }

            if *name == local_name!("align") {
                use style::values::specified::TextAlign;
                let keyword = match value {
                    "left" => Some(StyloTextAlign::MozLeft),
                    "right" => Some(StyloTextAlign::MozRight),
                    "center" => Some(StyloTextAlign::MozCenter),
                    _ => None,
                };

                if let Some(keyword) = keyword {
                    push_style(PropertyDeclaration::TextAlign(TextAlign::Keyword(keyword)));
                }
            }

            // The width/height attributes on these elements map to the
            // corresponding dimension properties (percentages allowed):
            // https://html.spec.whatwg.org/multipage/rendering.html#dimRendering
            // https://html.spec.whatwg.org/multipage/rendering.html#attributes-for-embedded-content-and-images
            let is_width = *name == local_name!("width");
            let is_height = *name == local_name!("height");
            // The elements whose width/height attributes map to the dimension
            // properties. `input` only joins them as type=image, which is the
            // one replaced input type.
            let is_embedded = *tag == local_name!("iframe")
                || *tag == local_name!("embed")
                || *tag == local_name!("video")
                || *tag == local_name!("object")
                || *tag == local_name!("img")
                || *tag == local_name!("marquee")
                || is_image_input;
            let maps_to_dimension = if is_width {
                is_embedded
                    || *tag == local_name!("table")
                    || *tag == local_name!("col")
                    || *tag == local_name!("colgroup")
                    || *tag == local_name!("tr")
                    || *tag == local_name!("td")
                    || *tag == local_name!("th")
                    || *tag == local_name!("hr")
            } else if is_height {
                is_embedded
                    || *tag == local_name!("table")
                    || *tag == local_name!("thead")
                    || *tag == local_name!("tbody")
                    || *tag == local_name!("tfoot")
                    || *tag == local_name!("tr")
                    || *tag == local_name!("td")
                    || *tag == local_name!("th")
            } else {
                false
            };
            if maps_to_dimension {
                // Three of these use the "(ignoring zero)" variant of the
                // mapping, where a zero is dropped instead of honoured:
                // `table width`, and `td`/`th` in both axes.
                let is_cell = *tag == local_name!("td") || *tag == local_name!("th");
                let ignoring_zero = is_cell || (is_width && *tag == local_name!("table"));
                if let Some(size) = parse_size_attr(value, ignoring_zero) {
                    use style::values::generics::{NonNegative, length::Size};
                    let size = Size::LengthPercentage(NonNegative(size));
                    push_style(if is_width {
                        PropertyDeclaration::Width(size)
                    } else {
                        PropertyDeclaration::Height(size)
                    });
                }
            }

            // hspace/vspace map to the horizontal and vertical margins as
            // dimension properties. This is a *smaller* set than the one that
            // takes width/height: `iframe` and `video` take width and height
            // but not hspace/vspace, and html/rendering/unmapped-attributes
            // checks exactly that -- browsers have got it wrong before.
            let takes_spacing = *tag == local_name!("embed")
                || *tag == local_name!("img")
                || *tag == local_name!("object")
                || *tag == local_name!("marquee")
                || is_image_input;
            if takes_spacing {
                let is_hspace = *name == local_name!("hspace");
                let is_vspace = *name == local_name!("vspace");
                if is_hspace || is_vspace {
                    if let Some(size) = parse_size_attr(value, false) {
                        use style::values::generics::length::GenericMargin;
                        let margin = GenericMargin::LengthPercentage(size);
                        if is_hspace {
                            push_style(PropertyDeclaration::MarginLeft(margin.clone()));
                            push_style(PropertyDeclaration::MarginRight(margin));
                        } else {
                            push_style(PropertyDeclaration::MarginTop(margin.clone()));
                            push_style(PropertyDeclaration::MarginBottom(margin));
                        }
                    }
                }
            }

            // https://svgwg.org/svg2-draft/geometry.html#Sizing
            // The `width` and `height` attributes on an `<svg>` element are
            // presentation attributes that map to the CSS `width`/`height`
            // properties, so e.g. `width="1em"` must resolve against the
            // element's font-size like any other CSS length.
            if *tag == local_name!("svg")
                && (*name == local_name!("width") || *name == local_name!("height"))
            {
                if let Some(size) = parse_svg_size_attr(value) {
                    use style::values::generics::{NonNegative, length::Size};
                    let size = Size::LengthPercentage(NonNegative(size));
                    push_style(if *name == local_name!("width") {
                        PropertyDeclaration::Width(size)
                    } else {
                        PropertyDeclaration::Height(size)
                    });
                }
            }

            // The `border` attribute maps to the four border widths as a
            // pixel length, plus the four border styles as `solid` -- width
            // alone would compute back to zero against the default
            // border-style of `none`. It is only these three elements:
            // `embed`, `iframe`, `marquee` and non-image `input` all have a
            // `border` attribute that must stay unmapped.
            if *name == local_name!("border")
                && (*tag == local_name!("img") || *tag == local_name!("object") || is_image_input)
            {
                if let Ok(px) = style::servo::attr::parse_unsigned_integer(value.chars()) {
                    use style::values::specified::{BorderSideWidth, BorderStyle};
                    let width = BorderSideWidth::from_px(px as f32);
                    push_style(PropertyDeclaration::BorderTopWidth(width.clone()));
                    push_style(PropertyDeclaration::BorderRightWidth(width.clone()));
                    push_style(PropertyDeclaration::BorderBottomWidth(width.clone()));
                    push_style(PropertyDeclaration::BorderLeftWidth(width));
                    push_style(PropertyDeclaration::BorderTopStyle(BorderStyle::Solid));
                    push_style(PropertyDeclaration::BorderRightStyle(BorderStyle::Solid));
                    push_style(PropertyDeclaration::BorderBottomStyle(BorderStyle::Solid));
                    push_style(PropertyDeclaration::BorderLeftStyle(BorderStyle::Solid));
                }
            }

            // `body` carries four legacy margin attributes, as pixel lengths:
            // marginwidth and marginheight set both sides of an axis, and
            // leftmargin and topmargin set one side each.
            //
            // There is deliberately no `rightmargin` or `bottommargin`. They
            // look like the obvious counterparts to the two that exist, but
            // the spec does not define them and browsers ignore them in both
            // standards and quirks mode -- body-margin-3a/3b assert exactly
            // that.
            if *tag == local_name!("body") {
                // Matched as strings: these are not in the static atom set,
                // so `local_name!` will not compile for them.
                let sides: &[u8] = match &**name {
                    "marginwidth" => b"lr",
                    "marginheight" => b"tb",
                    "leftmargin" => b"l",
                    "topmargin" => b"t",
                    _ => b"",
                };
                if !sides.is_empty() {
                    if let Ok(px) = style::servo::attr::parse_unsigned_integer(value.chars()) {
                        use style::values::generics::length::GenericMargin;
                        use style::values::specified::{LengthPercentage, NoCalcLength};
                        let margin = GenericMargin::LengthPercentage(LengthPercentage::Length(
                            NoCalcLength::from_px(px as f32),
                        ));
                        for side in sides {
                            push_style(match side {
                                b'l' => PropertyDeclaration::MarginLeft(margin.clone()),
                                b'r' => PropertyDeclaration::MarginRight(margin.clone()),
                                b't' => PropertyDeclaration::MarginTop(margin.clone()),
                                b'b' => PropertyDeclaration::MarginBottom(margin.clone()),
                                _ => unreachable!("side table above only yields lrtb"),
                            });
                        }
                    }
                }
            }

            // SVG font-size presentation attribute (user units map to px).
            // Keeps attr-styled svg text correct now that the serializer
            // stamps computed font-size onto text elements: the attr feeds
            // the cascade at pres-hint level, so the computed value has it.
            if *name == local_name!("font-size") && *elem.name.ns == *"http://www.w3.org/2000/svg"
            {
                if let Some(size) = parse_svg_size_attr(value) {
                    use style::values::specified::FontSize;
                    push_style(PropertyDeclaration::FontSize(FontSize::Length(size)));
                }
            }

            if *name == local_name!("bgcolor") {
                use style::values::specified::Color;
                if let Some((r, g, b, a)) = parse_color_attr(value) {
                    push_style(PropertyDeclaration::BackgroundColor(
                        Color::from_absolute_color(AbsoluteColor::srgb_legacy(r, g, b, a)),
                    ));
                }
            }

            if *name == local_name!("hidden") {
                use style::values::specified::Display;
                push_style(PropertyDeclaration::Display(Display::None));
            }
        }

        if let Some(lang) = lang {
            push_style(PropertyDeclaration::XLang(XLang(Atom::from(lang))));
        }
    }

    fn local_name(&self) -> &LocalName {
        &self.element_data().expect("Not an element").name.local
    }

    fn namespace(&self) -> &Namespace {
        &self.element_data().expect("Not an element").name.ns
    }

    fn query_container_size(
        &self,
        _display: &style::values::specified::Display,
    ) -> euclid::default::Size2D<Option<app_units::Au>> {
        // Content-box size from the last layout pass (cq units, size
        // container queries). Blitz doesn't interleave style and
        // layout, so the first cascade sees a zeroed layout; callers
        // converge on the next restyle once the container is laid out.
        let l = self.final_layout();
        let w = l.size.width - l.padding.left - l.padding.right - l.border.left - l.border.right;
        let h = l.size.height - l.padding.top - l.padding.bottom - l.border.top - l.border.bottom;
        euclid::default::Size2D::new(
            Some(app_units::Au::from_f32_px(w.max(0.0))),
            Some(app_units::Au::from_f32_px(h.max(0.0))),
        )
    }

    // No CustomStateSet here, matching `has_custom_state` returning false.
    // The relative-selector collector calls this, so it cannot stay todo!().
    fn each_custom_state<F>(&self, _callback: F)
    where
        F: FnMut(&AtomIdent),
    {
    }

    fn has_selector_flags(&self, flags: ElementSelectorFlags) -> bool {
        self.selector_flags().get().contains(flags)
    }

    fn relative_selector_search_direction(&self) -> ElementSelectorFlags {
        let flags = self.selector_flags().get();
        if flags.contains(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING)
        {
            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING
        } else if flags.contains(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR)
        {
            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR
        } else if flags.contains(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING) {
            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING
        } else {
            ElementSelectorFlags::empty()
        }
    }

    fn compute_layout_damage(old: &ComputedValues, new: &ComputedValues) -> RestyleDamage {
        compute_layout_damage(old, new)
        // ALL_DAMAGE
    }

    // fn update_animations(
    //     &self,
    //     before_change_style: Option<Arc<ComputedValues>>,
    //     tasks: style::context::UpdateAnimationsTasks,
    // ) {
    //     todo!()
    // }

    // fn process_post_animation(&self, tasks: style::context::PostAnimationTasks) {
    //     todo!()
    // }

    // fn needs_transitions_update(
    //     &self,
    //     before_change_style: &ComputedValues,
    //     after_change_style: &ComputedValues,
    // ) -> bool {
    //     todo!()
    // }
}

pub struct Traverser<'a> {
    // dom: &'a Slab<Node>,
    parent: BlitzNode<'a>,
    child_index: usize,
}

impl<'a> Iterator for Traverser<'a> {
    type Item = BlitzNode<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let node_id = self.parent.flat_children().get(self.child_index)?;
        let node = self.parent.with(*node_id);

        self.child_index += 1;

        Some(node)
    }
}

impl std::hash::Hash for BlitzNode<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.id.as_u64())
    }
}

/// Handle custom painters like images for layouting
///
/// todo: actually implement this
pub struct RegisteredPaintersImpl;
impl RegisteredSpeculativePainters for RegisteredPaintersImpl {
    fn get(&self, _name: &Atom) -> Option<&dyn RegisteredSpeculativePainter> {
        None
    }
}

use style::traversal::recalc_style_at;

pub struct RecalcStyle<'a> {
    context: SharedStyleContext<'a>,
    /// Nodes whose `background-image`/`mask-image` layers need flushing to
    /// dedicated storage on the node (see `flush_image_layers_from_style`)
    /// because their style changed during this traversal.
    nodes_needing_style_image_flush: Mutex<Vec<NodeId>>,
}

impl<'a> RecalcStyle<'a> {
    pub fn new(context: SharedStyleContext<'a>) -> Self {
        RecalcStyle {
            context,
            nodes_needing_style_image_flush: Mutex::new(Vec::new()),
        }
    }
}

#[allow(unsafe_code)]
impl<'dom> DomTraversal<BlitzNode<'dom>> for RecalcStyle<'_> {
    fn process_preorder<F: FnMut(BlitzNode<'dom>)>(
        &self,
        context: &mut StyleContext<BlitzNode<'dom>>,
        node: BlitzNode<'dom>,
        note_child: F,
    ) {
        if let Some(el) = node.as_element() {
            // let mut data = el.mutate_data().unwrap();
            let mut data = unsafe { el.ensure_data() };
            let had_inline_background = data
                .styles
                .get_primary()
                .map(|style| has_inline_background(style));
            recalc_style_at(self, context, el, &mut data, note_child);

            // Parley captures background-fragment presence at box construction. Stylo's repaint
            // damage does not call compute_layout_damage, so handle transparency changes here.
            if let (Some(had_background), Some(style)) =
                (had_inline_background, data.styles.get_primary())
            {
                if had_background != has_inline_background(style) {
                    data.damage.insert(CONSTRUCT_BOX);
                }
            }

            // Box construction copies a shown placeholder's look onto its text input, and stylo's
            // damage covers only the control's own style.
            let styled = !data.flags.contains(ElementDataFlags::TRAVERSED_WITHOUT_STYLING);
            if styled && placeholder_look_is_stale(&self.context, el, &data) {
                data.damage.insert(CONSTRUCT_BOX);
            }

            sync_pseudo_element_styles(el, &data, &self.nodes_needing_style_image_flush);

            if !data.damage.is_empty() {
                // Mark the ancestor chain so that the damage propagation pass
                // visits this element.
                el.mark_damaged();

                if data
                    .styles
                    .get_primary()
                    .is_some_and(|style| needs_style_image_flush(el, style))
                {
                    self.nodes_needing_style_image_flush
                        .lock()
                        .unwrap()
                        .push(el.id);
                }
            }

            // Gets set later on
            el.unset_dirty_descendants();
        }
    }

    #[inline]
    fn needs_postorder_traversal() -> bool {
        false
    }

    fn process_postorder(
        &self,
        _style_context: &mut StyleContext<BlitzNode<'dom>>,
        _node: BlitzNode<'dom>,
    ) {
        panic!("this should never be called")
    }

    #[inline]
    fn shared_context(&self) -> &SharedStyleContext<'_> {
        &self.context
    }
}

/// Whether a text control showing its placeholder holds a `::placeholder`
/// colour or font style that its fresh style no longer gives: a class or an
/// attr() that only the pseudo reads changes nothing stylo diffs.
fn placeholder_look_is_stale(
    context: &SharedStyleContext,
    el: &Node,
    data: &style::data::ElementData,
) -> bool {
    use crate::layout::construct::{placeholder_look, placeholder_pseudo_style};
    let Some(input) = el.element_data().and_then(|e| e.text_input_data()) else {
        return false;
    };
    if !input.is_showing_placeholder {
        return false;
    }
    let Some(primary) = data.styles.get_primary() else {
        return false;
    };
    let fresh = placeholder_pseudo_style(context.stylist, &context.guards, el, primary)
        .map(|style| placeholder_look(&style));
    fresh != input.placeholder_color.zip(input.placeholder_font_style)
}

/// Flush updated pseudo-element (`::before`/`::after`) styles from the owning
/// element's freshly-computed stylo data to the pseudo-element's anonymous node.
///
/// Pseudo-element styles are normally flushed to the pseudo-element's node
/// during box construction (see `flush_pseudo_elements`), but in incremental
/// mode box construction only runs for nodes with construction damage.
/// Pseudo-element style changes which don't require reconstruction (e.g.
/// animations/transitions of repaint- or relayout-only properties) must still
/// be flushed to the pseudo-element's node - along with the damage they imply -
/// so that layout and paint see the new style.
///
/// This runs during the style traversal, immediately after the owning element
/// has been restyled, because that is where the old and new pseudo styles can
/// be diffed precisely (the pseudo node's stored primary style is the "old"
/// style; the owner's just-computed pseudo styles are the "new" ones).
///
/// SAFETY: the style traversal has exclusive access to the tree, and each
/// pseudo-element node is only ever accessed from its owning element's
/// `process_preorder` (pseudo nodes are not part of the DOM child list and are
/// not themselves visited by the traversal), so mutating the pseudo node's
/// stylo data here cannot race with any other traversal thread.
#[allow(unsafe_code)]
fn sync_pseudo_element_styles(
    el: &Node,
    data: &style::data::ElementData,
    nodes_needing_style_image_flush: &Mutex<Vec<NodeId>>,
) {
    let before_node_id = el.before();
    let after_node_id = el.after();
    if before_node_id.is_none() && after_node_id.is_none() {
        return;
    }

    // Note: yes these are kinda backwards (see `flush_pseudo_elements`)
    let pseudos = data.styles.pseudos.as_array();
    let before_style = pseudos[1].clone();
    let after_style = pseudos[0].clone();

    // Creation and removal of pseudo-elements is handled during box construction
    // (Stylo generates construction damage for those cases), so only the case
    // where the pseudo-element both was and remains present is handled here.
    for (pe_node_id, pe_style) in [(before_node_id, before_style), (after_node_id, after_style)] {
        let (Some(pe_node_id), Some(pe_style)) = (pe_node_id, pe_style) else {
            continue;
        };
        let pe_node = el.with(pe_node_id);
        let Some(stylo_data) = pe_node.try_stylo_element_data() else {
            continue;
        };
        let mut pe_data = match unsafe { stylo_data.unsafe_stylo_only_mut() } {
            Some(data) => data,
            None => continue,
        };
        let Some(old_style) = pe_data.styles.primary.clone() else {
            continue;
        };
        if std::ptr::eq(&*old_style, &*pe_style) {
            continue;
        }

        let mut diff = RestyleDamage::compute_style_difference::<&Node>(&old_style, &pe_style);
        if has_inline_background(&old_style) != has_inline_background(&pe_style) {
            diff.damage.insert(CONSTRUCT_BOX);
        }
        if !diff.damage.is_empty() {
            pe_data.damage.insert(diff.damage);
            pe_node.mark_damaged();

            if needs_style_image_flush(pe_node, &pe_style) {
                nodes_needing_style_image_flush
                    .lock()
                    .unwrap()
                    .push(pe_node_id);
            }
        }
        pe_data.styles.primary = Some(pe_style);
        pe_data.set_restyled();
    }
}

/// Whether a node's `background-image`/`mask-image` layers need flushing to
/// dedicated storage on the node: the URLs referenced by the new style differ
/// from the image data currently stored on the node.
fn needs_style_image_flush(node: &Node, style: &ComputedValues) -> bool {
    use crate::node::ImageResourceData;
    use style::url::ComputedUrl;
    use style::values::computed::image::Image;

    fn out_of_sync(style_images: &[Image], stored: &[Option<ImageResourceData>]) -> bool {
        if style_images.len() != stored.len() {
            // Differing lengths only matter if either side references an image
            // (a style list of `none` layers and empty storage are in sync).
            return style_images
                .iter()
                .any(|image| matches!(image, Image::Url(_)))
                || stored.iter().any(Option::is_some);
        }
        std::iter::zip(style_images, stored).any(|(style_image, stored)| {
            match (style_image, stored) {
                (Image::Url(ComputedUrl::Valid(url)), Some(data)) => **url != *data.url,
                (Image::Url(ComputedUrl::Valid(_)), None) => true,
                (_, Some(_)) => true,
                (_, None) => false,
            }
        })
    }

    node.data.downcast_element().is_some_and(|el| {
        out_of_sync(
            &style.get_background().background_image.0,
            &el.background_images,
        ) || out_of_sync(&style.get_svg().mask_image.0, &el.mask_images)
    })
}

#[test]
fn assert_size_of_equals() {
    // use std::mem;

    // fn assert_layout<E>() {
    //     assert_eq!(
    //         mem::size_of::<SharingCache<E>>(),
    //         mem::size_of::<TypelessSharingCache>()
    //     );
    //     assert_eq!(
    //         mem::align_of::<SharingCache<E>>(),
    //         mem::align_of::<TypelessSharingCache>()
    //     );
    // }

    // let size = mem::size_of::<StyleSharingCandidate<BlitzNode>>();
    // dbg!(size);
}

#[test]
fn parse_inline() {
    // let attrs = style::attr::AttrValue::from_serialized_tokenlist(
    //     r#"visibility: hidden; left: 1306.5px; top: 50px; display: none;"#.to_string(),
    // );

    // let val = CSSInlineStyleDeclaration();
}
