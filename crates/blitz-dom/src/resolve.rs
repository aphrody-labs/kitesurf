//! Resolve style and layout

use blitz_traits::node_id::NodeId;
use std::cell::RefCell;

use debug_timer::debug_timer;
use kurbo::{Affine, Rect};
use parley::LayoutContext;
use selectors::Element as _;
use style::dom::TDocument;

#[cfg(feature = "parallel-construct")]
use rayon::prelude::*;

// FIXME: static thread_local FontCtx isn't necessarily correct in multi-document context.
// Should use thread_local crate with ThreadLocal value store in the Document.
thread_local! {
    pub(crate) static LAYOUT_CTX: RefCell<Option<Box<LayoutContext<TextBrush>>>> = const { RefCell::new(None) };
}

use taffy::AvailableSpace;

use crate::{
    BaseDocument,
    layout::{
        LayoutPassState,
        construct::{
            ConstructionTask, ConstructionTaskData, ConstructionTaskResult,
            ConstructionTaskResultData, LayoutChildren, build_inline_layout_into,
            collect_layout_children,
        },
        damage::{ALL_DAMAGE, CONSTRUCT_BOX, CONSTRUCT_DESCENDENT, CONSTRUCT_FC},
    },
    node::{SpecialElementData, TextBrush},
};

fn snap_table_tracks(
    tracks: &mut taffy::DetailedGridTracksInfo<style::Atom>,
    raw_origin: f32,
    paint_origin: f32,
    step: f32,
) {
    if step <= 1.0 || tracks.positions.is_empty() {
        return;
    }

    let snap = |value: f32| (value / step).round() * step;
    for position in &mut tracks.positions {
        position.start = snap(raw_origin + position.start) - paint_origin;
        position.end = snap(raw_origin + position.end) - paint_origin;
    }
}

/// A query container, i.e. an element `@container` rules inside it
/// size themselves against. `container-type: normal` elements are not one.
fn is_size_container(node: &crate::node::Node) -> bool {
    node.primary_styles()
        .is_some_and(|styles| !styles.get_box().clone_container_type().is_normal())
}

/// Every query container's laid-out size, keyed by node id. Slab ids
/// ascend in iteration order, so this is sorted and the pass after layout can
/// binary-search it for the size a container was styled against.
fn container_sizes(doc: &BaseDocument) -> Vec<(NodeId, taffy::Size<f32>)> {
    doc.nodes
        .iter()
        .filter(|(_, node)| is_size_container(node))
        .map(|(id, node)| (id, node.final_layout().size))
        .collect()
}

/// How many style + layout passes one resolve spends settling
/// `@container` sizes. Size containment keeps a container's own contents from
/// feeding its size back, so only an ancestor's restyle can resize it and
/// nesting depth is the real bound: one level per pass. Eight is far past
/// anything we render, and it stops a sheet that oscillates through an axis no
/// query reads from wedging the resolve.
const CONTAINER_QUERY_SETTLE_PASSES: usize = 8;

impl BaseDocument {
    /// Restore construction damage after a panic strands an inline root
    /// without the layout data that `compute_inline_layout_inner` took.
    pub fn recover_from_layout_panic(&mut self) {
        for (_, node) in self.nodes.iter_mut() {
            let missing_inline_layout = node.flags.is_inline_root()
                && node
                    .data
                    .downcast_element()
                    .is_some_and(|element| element.inline_layout_data.is_none());
            if missing_inline_layout {
                node.insert_damage(ALL_DAMAGE);
            }
        }
    }

    /// Restyle the tree and then relayout it
    pub fn resolve(&mut self, current_time_for_animations: f64) {
        if TDocument::as_node(&self.root_node())
            .first_element_child()
            .is_none()
        {
            #[cfg(feature = "tracing")]
            tracing::warn!("No DOM - not resolving");
            return;
        }

        self.process_pending_style_elements();

        // Process messages that have been sent to our message channel (e.g. loaded resource)
        self.handle_messages();

        // While render-blocking resources (e.g. stylesheets linked from the `<head>`) are
        // still loading, don't resolve styles or layout (matching how browsers block
        // rendering). Resolving styles before the document's stylesheets have loaded would
        // give elements computed styles based on an incomplete cascade, and a later restyle
        // (once the stylesheet loads) would treat those as genuine "before-change styles",
        // spuriously starting CSS transitions from unstyled values. See issue #689.
        //
        // `handle_messages` above must still run so that loaded resources are ingested and
        // this state can clear.
        if self.has_pending_critical_resources() {
            return;
        }

        self.resolve_scroll_animation();

        // Drop scrollbar-activity entries whose fade-out has finished (also
        // sheds entries for removed nodes).
        {
            use crate::node::scrollbar::{FADE_DELAY, FADE_DURATION};
            self.scrollbar_activity
                .retain(|_, last| last.elapsed() < FADE_DELAY + FADE_DURATION);
        }

        let root_node_id = self.root_element().id;
        debug_timer!(timer, feature = "log-phase-times");

        // Apply any device changes (viewport resize, zoom, color-scheme, etc)
        // accumulated since the last resolve as a single device rebuild.
        self.flush_pending_device_changes();
        // Slot assignment shapes the flat tree that styling and box
        // construction both consume; recompute before either runs.
        self.flush_slot_assignments();

        // we need to resolve stylist first since it will need to drive our layout bits
        self.resolve_stylist(current_time_for_animations);
        timer.record_time("style");

        // Computed styles are fresh: fetch any pending
        // @font-face whose family just became used (lazy web-font
        // loading; see flush_needed_font_faces).
        self.flush_needed_font_faces();
        timer.record_time("fontface");

        // Fold this resolve's per-node damage (mutations +
        // restyle diffs) into the cross-resolve summary before anyone
        // consumes or clears it (and before non-incremental mode marks
        // everything damaged), and record the construct-damage bit the
        // counters gate below reads. Consumed by the embedder via
        // take_damage_summary (partial-frame gating).
        self.accumulate_damage_summary();
        timer.record_time("dmgsummary");

        // @container rules resolve against the container's laid-out
        // size, which the cascade below can only read from last resolve's box.
        // Snapshot the sizes it styles against, so the pass after layout can
        // spot the containers whose answer is now stale.
        let container_sizes_before = container_sizes(self);

        // CSS counters: list-item values + counter()/attr() pseudo content.
        // Needs computed styles; box construction consumes the side tables.
        self.resolve_counters();
        timer.record_time("counters");

        // Non-incremental mode is "every node is damaged every frame": mark the
        // whole tree and let the same damage pipeline reconstruct and relayout
        // everything.
        if !self.incremental_layout {
            self.mark_all_damaged();
            timer.record_time("mark_all");
        }

        // Propagate damage flags (from mutation and restyles) up and down the tree
        self.propagate_damage_flags(root_node_id);
        timer.record_time("damage");

        // Fix up tree for layout (insert anonymous blocks as necessary, etc)
        self.resolve_layout_children();
        #[cfg(debug_assertions)]
        self.assert_layout_parents_consistent();
        timer.record_time("construct");

        self.resolve_deferred_tasks();
        // Flush background/mask images from style to dedicated storage on the
        // nodes whose style changed (queued by the style traversal and by
        // pseudo-element box construction), fetching any not-yet-loaded images.
        self.flush_pending_style_images();
        timer.record_time("pconstruct");

        // Next we resolve layout with the data resolved by stlist
        self.resolve_layout();
        timer.record_time("layout");

        // A resolve that moved no box and constructed nothing styled every
        // container against the size it still has, so steady state pays for
        // the snapshot above and nothing else. Construction alone is enough
        // to need the scan: a container built this pass is not in the
        // snapshot, and its @container rules cascaded against no size at all.
        if self.any_construct_damage || (self.layout_changed && !container_sizes_before.is_empty()) {
            // One pass is not enough: an outer query can resize a nested container,
            // whose own rules just cascaded against its pre-pass box. Settle instead
            // (restyle what moved, lay out, and look again) until every container
            // matches the size it was styled against.
            let mut sizes_styled_against = container_sizes_before;
            for _ in 0..CONTAINER_QUERY_SETTLE_PASSES {
                let changed_containers: Vec<_> = self
                    .nodes
                    .iter()
                    .filter(|(id, node)| {
                        is_size_container(node)
                            && sizes_styled_against
                                .binary_search_by_key(id, |(before_id, _)| *before_id)
                                .map_or(true, |i| {
                                    sizes_styled_against[i].1 != node.final_layout().size
                                })
                    })
                    .map(|(id, _)| id)
                    .collect();
                if changed_containers.is_empty() {
                    break;
                }
                // The cascade below reads the boxes the layout above produced.
                sizes_styled_against = container_sizes(self);
                for id in &changed_containers {
                    self.nodes[*id].set_restyle_hint(
                        style::invalidation::element::restyle_hints::RestyleHint::restyle_subtree(),
                    );
                }
                self.resolve_stylist(current_time_for_animations);
                self.accumulate_damage_summary();
                self.resolve_counters();
                if !self.incremental_layout {
                    self.mark_all_damaged();
                }
                self.propagate_damage_flags(root_node_id);
                self.resolve_layout_children();
                self.resolve_deferred_tasks();
                // The corrective cascade can change a node's background/mask
                // image style, so drain the style-image queue like the main
                // resolve path does; skipping it keeps the old layer (or
                // defers the fetch) until another resolve.
                self.flush_pending_style_images();
                self.resolve_layout();
                timer.record_time("container");
            }
        }

        self.nodes.bump_geometry_generation();

        // Resolve transforms
        self.resolve_transforms(root_node_id);
        timer.record_time("transform");

        // Build paint children / stacking contexts for damaged subtrees. Runs
        // after layout and transforms so that stacking-context roots created by
        // this frame's transforms are picked up, and before damage is cleared.
        self.build_paint_tree(root_node_id);
        timer.record_time("paint_tree");

        // Re-homed paint boxes live outside their flow parent's taffy
        // bounds; compute the reach rects paint-cull and hit-prune widen
        // by. Must run after taffy layout (the one geometry pass in resolve
        // that is never stale), after the transform pass, whose transforms
        // and overflow it maps each re-homed box through, and after the
        // paint tree records re-homes.
        self.compute_rehomed_reach();
        timer.record_time("rehomed");

        // Clear all damage and dirty flags, walking only subtrees which are
        // marked as (potentially) containing damage.
        let doc_node_id = self.root_node().id;
        self.clear_damage_and_dirty_flags(doc_node_id);
        timer.record_time("c_damage");

        // Re-resolve the hover node from the pointer position against the fresh
        // layout. This must run *after* the damage/dirty flags are cleared
        // above, so that the restyle hint and ancestor `dirty_descendants`
        // flags set by any resulting hover change survive into the next resolve
        // pass (the clearing loop would otherwise wipe them). Any resulting
        // restyle is picked up on the next resolve pass; a redraw is requested
        // if the hovered node actually changes.
        self.refresh_hover();

        let mut subdoc_is_animating = false;
        for &node_id in &self.sub_document_nodes {
            let node = &mut self.nodes[node_id];
            let size = node.final_layout().size;
            if let Some(mut sub_doc) = node.subdoc_mut().map(|doc| doc.inner_mut()) {
                // Set viewport
                // viewport_mut handles change detection. So we just unconditionally set the values;
                let mut sub_viewport = sub_doc.viewport_mut();
                sub_viewport.hidpi_scale = self.viewport.hidpi_scale;
                sub_viewport.zoom = self.viewport.zoom;
                sub_viewport.color_scheme = self.viewport.color_scheme;

                let viewport_scale = self.viewport.scale();
                sub_viewport.window_size = (
                    (size.width * viewport_scale) as u32,
                    (size.height * viewport_scale) as u32,
                );
                drop(sub_viewport);

                sub_doc.resolve(current_time_for_animations);

                subdoc_is_animating |= sub_doc.is_animating();
            }
        }
        self.subdoc_is_animating = subdoc_is_animating;
        timer.record_time("subdocs");

        timer.print_times(&format!("Resolve({}): ", self.id()));
    }

    fn resolve_transforms(&mut self, node_id: NodeId) -> Rect {
        if !self.nodes.contains_key(node_id) {
            return Rect::ZERO;
        }

        let scale = self.viewport.scale_f64();

        if !self.nodes[node_id]
            .damage()
            .map(|d| d.contains(style::selector_parser::RestyleDamage::RECALCULATE_OVERFLOW))
            .unwrap_or(false)
        {
            let node = &self.nodes[node_id];
            let location = node.final_layout().location.map(|v| v as f64 * scale);

            let mut transform = Affine::translate((location.x, location.y));
            if let Some(t) = node.transform().as_deref() {
                transform *= *t
            }

            let overflow = *node.scrollable_overflow();
            return transform.transform_rect_bbox(overflow);
        }

        let transform = self.nodes[node_id].set_transform(scale as f32);

        let w = self.nodes[node_id].final_layout().size.width as f64 * scale;
        let h = self.nodes[node_id].final_layout().size.height as f64 * scale;
        let mut overflow = Rect::new(0.0, 0.0, w, h);

        let layout_children = std::mem::take(self.nodes[node_id].layout_children.get_mut());

        if let Some(ref children) = layout_children {
            for &child_id in children {
                // Out-of-flow children are laid out relative to their containing
                // block, not their DOM parent: they are visited (and their overflow
                // accounted for) via the containing block's hoisted list below.
                if self.nodes[child_id].is_out_of_flow() {
                    continue;
                }
                let child_rect_in_self = self.resolve_transforms(child_id);
                overflow = overflow.union(child_rect_in_self);
            }
        }
        let hoisted_children =
            std::mem::take(&mut *self.nodes[node_id].hoisted_children.borrow_mut());
        for &child_id in &hoisted_children {
            if !self.nodes.contains_key(child_id) {
                continue;
            }
            let child_rect_in_self = self.resolve_transforms(child_id);
            overflow = overflow.union(child_rect_in_self);
        }
        *self.nodes[node_id].hoisted_children.borrow_mut() = hoisted_children;
        for pseudo in [self.nodes[node_id].before(), self.nodes[node_id].after()] {
            let Some(pseudo) = pseudo else { continue };
            if self.nodes[pseudo].is_out_of_flow() {
                continue;
            }
            let child_rect_in_self = self.resolve_transforms(pseudo);
            overflow = overflow.union(child_rect_in_self);
        }

        *self.nodes[node_id].scrollable_overflow_mut() = overflow;
        *self.nodes[node_id].layout_children.get_mut() = layout_children;

        let scaled_x = self.nodes[node_id].final_layout().location.x as f64 * scale;
        let scaled_y = self.nodes[node_id].final_layout().location.y as f64 * scale;

        let full = if let Some(t) = transform {
            Affine::translate((scaled_x, scaled_y)) * t
        } else {
            Affine::translate((scaled_x, scaled_y))
        };

        full.transform_rect_bbox(overflow)
    }

    /// Ensure that the layout_children field is populated for all nodes
    /// Post-layout: for every node that hosts re-homed paint children,
    /// union their boxes (in the host's coordinate space) and propagate
    /// the rect up the host's layout-parent chain. Paint-cull and
    /// hit-prune treat the result as extra reach beyond taffy's bounds,
    /// which cannot cover a re-homed box (it is not a layout child of
    /// its flow parent). Direct rects use max(size, content_size), so a
    /// re-homed box's own descendants are covered through taffy; boxes
    /// re-homed into a subtree from an outer containing block are
    /// covered because their flow parent is itself a host.
    ///
    /// Simply disabling the cull along flagged chains does not work: on
    /// heavy pages the flag reaches the root, culling effectively turns
    /// off, and the recorded scene grows until the renderer's wasm aborts.
    pub fn compute_rehomed_reach(&mut self) {
        // Reach is rebuilt from fresh layout every resolve, but only the
        // chains we actually set last time can be holding a stale value.
        for id in std::mem::take(&mut self.rehomed_reach_touched) {
            if let Some(node) = self.nodes.get(id) {
                node.rehomed_reach.set(None);
            }
        }

        // rehomed_hosts is a superset appended at the single push site, so
        // prune it here rather than rescanning every node to find hosts.
        let mut host_ids = std::mem::take(&mut self.rehomed_hosts);
        host_ids.sort_unstable_by_key(|id| id.as_u64());
        host_ids.dedup();
        host_ids.retain(|&id| {
            self.nodes.contains_key(id)
                && !self.nodes[id].rehomed_paint_children.borrow().is_empty()
        });

        let union =
            |acc: Option<(f32, f32, f32, f32)>, r: (f32, f32, f32, f32)| match acc {
                None => Some(r),
                Some(a) => Some((
                    a.0.min(r.0),
                    a.1.min(r.1),
                    a.2.max(r.2),
                    a.3.max(r.3),
                )),
            };

        let scale = self.viewport.scale_f64();
        // A re-homed viewport-fixed box moves through its host's
        // space as the page scrolls, and a pure-scroll frame skips resolve,
        // so its reach covers every scroll position: from where it sits at
        // scroll 0 to that plus the document's extent. Too wide only costs
        // culling along one chain; too narrow drops the box from paint and
        // hit-testing after a scroll.
        let fixed_anchor = self.nodes.fixed_anchor_scroll();
        let doc_extent = self.try_root_element().map_or((0.0, 0.0), |root| {
            let layout = root.final_layout();
            (
                layout.size.width.max(layout.scrollable_overflow_rect.right),
                layout.size.height.max(layout.scrollable_overflow_rect.bottom),
            )
        });
        for host_id in host_ids.iter().copied() {
            let mut rect: Option<(f32, f32, f32, f32)> = None;
            let rehomed: Vec<NodeId> = self.nodes[host_id]
                .rehomed_paint_children
                .borrow()
                .iter()
                .copied()
                .filter(|id| self.nodes.contains_key(*id))
                .collect();
            for child_id in rehomed {
                let child = &self.nodes[child_id];
                let mut r = child.rehomed_paint_rect(host_id, scale);
                if child.is_viewport_fixed() {
                    let (sx, sy) = (fixed_anchor.x as f32, fixed_anchor.y as f32);
                    r = (r.0 - sx, r.1 - sy, r.2 - sx + doc_extent.0, r.3 - sy + doc_extent.1);
                }
                rect = union(rect, r);
            }
            let Some(mut rect) = rect else { continue };

            // Union into the host, then propagate up the chain paint and
            // hit-test descend through (the paint parent of a re-homed box,
            // otherwise the containing block its location is relative to),
            // shifting into each ancestor's coordinate space.
            let mut cur = host_id;
            for _ in 0..512 {
                let node = &self.nodes[cur];
                node.rehomed_reach
                    .set(union(node.rehomed_reach.get(), rect));
                self.rehomed_reach_touched.push(cur);
                let mut loc = node.final_layout().location;
                let parent = match node.paint_parent_override.get() {
                    Some(paint_parent) => {
                        let t = node.rehomed_paint_translate(paint_parent);
                        loc.x += t.x;
                        loc.y += t.y;
                        paint_parent
                    }
                    None => {
                        let Some(parent) = node.containing_block() else { break };
                        parent
                    }
                };
                if parent == cur || !self.nodes.contains_key(parent) {
                    break;
                }
                let scroll = *node.scroll_offset();
                rect = (
                    rect.0 + loc.x - scroll.x as f32,
                    rect.1 + loc.y - scroll.y as f32,
                    rect.2 + loc.x - scroll.x as f32,
                    rect.3 + loc.y - scroll.y as f32,
                );
                cur = parent;
            }
        }

        self.rehomed_hosts = host_ids;
    }

    pub fn resolve_layout_children(&mut self) {
        resolve_layout_children_recursive(self, self.root_node().id);

        fn resolve_layout_children_recursive(doc: &mut BaseDocument, node_id: NodeId) {
            // Anonymous blocks and pseudo-elements can be removed from the slab
            // between render passes. Bail out rather than panicking on a stale key.
            if doc.nodes.get(node_id).is_none() {
                return;
            }

            // Nothing under here changed, so the walk is dead work. The gate
            // mirrors propagate_damage_flags' skip: damage marking walks the
            // DOM parent chain, which bypasses anonymous boxes, so those are
            // never skipped, and a shadow host is always entered because its
            // shadow tree is not on that chain either. Anonymous blocks and
            // pseudo-elements created during this walk have no cached
            // children to skip to, which is the same fact-42 escape hatch.
            {
                let node = &doc.nodes[node_id];
                if doc.incremental_layout
                    && node.damage().is_some_and(|d| d.is_empty())
                    && !node.has_damaged_descendants()
                    && !node.is_anonymous()
                    && node.attached_shadow_root_id().is_none()
                    && node.layout_children.borrow().is_some()
                {
                    return;
                }
            }

            let mut damage = doc.nodes[node_id].damage().unwrap_or(ALL_DAMAGE);
            let _flags = doc.nodes[node_id].flags;

            // A node with no cached children has nothing to skip
            // TO, so it must re-collect whatever its damage says. The skip
            // branch below take()s the list and restores it at the end, so
            // a panic anywhere in between (the embedder recovers from wasm
            // panics) leaves it None forever. Without this the
            // whole page silently resolves to zero height on the next pass.
            let ks_gate_open = damage.intersects(CONSTRUCT_FC | CONSTRUCT_BOX)
                || doc.nodes[node_id].layout_children.borrow().is_none();
            if ks_gate_open {
                //} || flags.contains(NodeFlags::IS_INLINE_ROOT) {

                // Deallocate the anonymous blocks created for this node in the
                // previous construction round. They live only in the slab, so
                // reconstructing without freeing them would leak a slab entry per
                // anonymous block per reconstruction.
                let old_anonymous_blocks = std::mem::take(&mut doc.nodes[node_id].anonymous_blocks);
                for anon_id in old_anonymous_blocks {
                    doc.deallocate_anonymous_block(anon_id);
                }

                let mut collected = LayoutChildren::default();
                collect_layout_children(doc, node_id, &mut collected);
                let layout_children = collected.children;
                doc.nodes[node_id].anonymous_blocks = collected.anonymous_blocks;
                // Float registration is a product of this node's boxes, so
                // a rebuild retires the cache bust its last inline pass
                // left behind: otherwise a block that ever held a float
                // is locked out of taffy's cache for the document's life.
                doc.nodes[node_id].placed_floats.set(false);

                // Recurse into newly collected layout children
                for child_id in layout_children.iter().copied() {
                    // Anonymous blocks and pseudo-elements can be removed
                    // from the slab between render passes (or by the
                    // recursive call itself collapsing siblings); skip
                    // stale IDs. Mirrors the else-branch guard below.
                    if !doc.nodes.contains_key(child_id) {
                        continue;
                    }
                    resolve_layout_children_recursive(doc, child_id);
                    if !doc.nodes.contains_key(child_id) {
                        continue;
                    }
                    doc.nodes[child_id].layout_parent.set(Some(node_id));
                    if let Some(mut data) = doc.nodes[child_id]
                        .try_stylo_element_data_mut()
                        .and_then(|s| s.get_mut())
                    {
                        data.damage
                            .remove(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                    }
                }

                *doc.nodes[node_id].layout_children.borrow_mut() = Some(layout_children.clone());
                // *doc.nodes[node_id].paint_children.borrow_mut() = Some(layout_children);

                damage.remove(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                // damage.insert(RestyleDamage::RELAYOUT | RestyleDamage::REPAINT);
            } else {
                //if damage.contains(CONSTRUCT_DESCENDENT) {
                let layout_children = doc.nodes[node_id].layout_children.borrow_mut().take();
                if let Some(layout_children) = layout_children {
                    for child_id in layout_children.iter().copied() {
                        // Anonymous blocks and pseudo-elements can be removed from the
                        // slab between render passes; skip stale IDs.
                        if !doc.nodes.contains_key(child_id) {
                            continue;
                        }
                        resolve_layout_children_recursive(doc, child_id);
                        doc.nodes[child_id].layout_parent.set(Some(node_id));
                    }

                    *doc.nodes[node_id].layout_children.borrow_mut() = Some(layout_children);
                }

                // damage.remove(CONSTRUCT_DESCENDENT);
                // damage.insert(RestyleDamage::RELAYOUT | RestyleDamage::REPAINT);
            }

            doc.nodes[node_id].set_damage(damage);
        }
    }

    /// Every layout child must point back at its container via
    /// `layout_parent`, which `propagate_damage_flags` relies on to reach
    /// anonymous boxes.
    #[cfg(debug_assertions)]
    fn assert_layout_parents_consistent(&self) {
        use crate::node::NodeFlags;

        for (parent_id, node) in self.nodes.iter() {
            if !node.flags.contains(NodeFlags::IS_IN_DOCUMENT) {
                continue;
            }
            let Some(children) = node.layout_children.borrow().clone() else {
                continue;
            };
            for child_id in children {
                let Some(child) = self.nodes.get(child_id) else {
                    panic!("layout child {child_id:?} of {parent_id:?} is not in the slab");
                };
                debug_assert_eq!(
                    child.layout_parent.get(),
                    Some(parent_id),
                    "layout_parent of {child_id:?} does not point at its layout container {parent_id:?}"
                );
            }
        }
    }

    pub fn resolve_deferred_tasks(&mut self) {
        let mut deferred_construction_nodes = std::mem::take(&mut self.deferred_construction_nodes);

        // Deduplicate deferred tasks by node_id to avoid redundant work
        deferred_construction_nodes.sort_unstable_by_key(|task| task.node_id);
        deferred_construction_nodes.dedup_by_key(|task| task.node_id);

        #[cfg(feature = "parallel-construct")]
        let iter = deferred_construction_nodes.into_par_iter();
        #[cfg(not(feature = "parallel-construct"))]
        let iter = deferred_construction_nodes.into_iter();

        let results: Vec<ConstructionTaskResult> = iter
            .map(|task: ConstructionTask| match task.data {
                ConstructionTaskData::InlineLayout(mut layout) => {
                    #[cfg(feature = "parallel-construct")]
                    let mut layout_ctx = LAYOUT_CTX
                        .take()
                        .unwrap_or_else(|| Box::new(LayoutContext::new()));
                    #[cfg(feature = "parallel-construct")]
                    let layout_ctx_mut = &mut layout_ctx;

                    #[cfg(feature = "parallel-construct")]
                    let mut font_ctx = self
                        .thread_font_contexts
                        .get_or(|| RefCell::new(Box::new(self.font_ctx.lock().unwrap().clone())))
                        .borrow_mut();
                    #[cfg(feature = "parallel-construct")]
                    let font_ctx_mut = &mut *font_ctx;

                    #[cfg(not(feature = "parallel-construct"))]
                    let layout_ctx_mut = &mut self.layout_ctx;
                    #[cfg(not(feature = "parallel-construct"))]
                    let font_ctx_mut = &mut *self.font_ctx.lock().unwrap();

                    layout.content_widths = None;
                    // The rebuild throws the lines away with the tree, so
                    // the recorded break width describes nothing.
                    layout.last_break_width = f32::NAN;
                    build_inline_layout_into(
                        &self.nodes,
                        layout_ctx_mut,
                        font_ctx_mut,
                        &mut layout,
                        self.viewport.scale(),
                        task.node_id,
                    );

                    #[cfg(feature = "parallel-construct")]
                    {
                        LAYOUT_CTX.set(Some(layout_ctx));
                    }

                    // If layout doesn't contain any inline boxes, then it is safe to populate the content_widths
                    // cache during this parallelized stage.
                    // if layout.layout.inline_boxes().is_empty() {
                    //     layout.content_widths();
                    // }

                    ConstructionTaskResult {
                        node_id: task.node_id,
                        data: ConstructionTaskResultData::InlineLayout(layout),
                    }
                }
            })
            .collect();

        for result in results {
            match result.data {
                ConstructionTaskResultData::InlineLayout(layout) => {
                    self.nodes[result.node_id].clear_layout_cache();
                    self.nodes[result.node_id]
                        .element_data_mut()
                        .unwrap()
                        .inline_layout_data = Some(layout);
                    self.inline_roots.push(result.node_id);
                }
            }
        }

        self.deferred_construction_nodes.clear();
    }

    /// Walk the nodes now that they're properly styled and transfer their styles to the taffy style system
    ///
    /// TODO: update taffy to use an associated type instead of slab key
    /// TODO: update taffy to support traited styles so we don't even need to rely on taffy for storage
    pub fn resolve_layout(&mut self) {
        // Track info is layout-pass-scoped: stale entries would pin
        // recycled slab ids to a different element's tracks.
        let size = self.stylist.device().au_viewport_size();

        let available_space = taffy::Size {
            width: AvailableSpace::Definite(size.width.to_f32_px()),
            height: AvailableSpace::Definite(size.height.to_f32_px()),
        };

        let root_element_id = crate::taffy_node_id(self.root_element().id);

        // println!("\n\nRESOLVE LAYOUT\n===========\n");

        self.layout_changed = false;
        let mut state = LayoutPassState::new(self);
        #[cfg(feature = "writing-mode")]
        let available_space = {
            state.layout_wm = state.layout_wm_of(crate::dom_node_id(root_element_id));
            if state.layout_wm.is_vertical() {
                available_space.transpose()
            } else {
                available_space
            }
        };
        taffy::compute_root_layout(&mut state, root_element_id, available_space);
        // Rounding derives entirely from the unrounded layouts, so when taffy's
        // cache answered every node last resolve's final_layout still holds.
        //
        // Except a rebuilt inline tree has no lines, and the restore pass below
        // re-breaks it against final_layout, which must not be a stale box.
        if state.layout_changed || state.inline_roots.len() != state.inline_roots_settled {
            #[cfg(feature = "writing-mode")]
            state.physicalise_and_round_layout(root_element_id);
            #[cfg(not(feature = "writing-mode"))]
            taffy::round_layout(&mut state, root_element_id);
        }

        // Paint reads table tracks rather than the cell layouts rounded above.
        let (grid_x, grid_y) = self.round_grid;
        if grid_x > 1.0 || grid_y > 1.0 {
            for (node_id, node) in self.nodes.iter() {
                let Some(element) = node.element_data() else {
                    continue;
                };
                let SpecialElementData::TableRoot(table) = &element.special_data else {
                    continue;
                };
                let Some(mut grid_info) = table.computed_grid_info.borrow().clone() else {
                    continue;
                };
                let mut table_x = 0.0;
                let mut table_y = 0.0;
                let mut ancestor_id = Some(node_id);
                while let Some(id) = ancestor_id {
                    let ancestor = &self.nodes[id];
                    table_x += ancestor.unrounded_layout().location.x;
                    table_y += ancestor.unrounded_layout().location.y;
                    ancestor_id = ancestor.containing_block();
                }
                snap_table_tracks(
                    &mut grid_info.columns,
                    table_x,
                    (table_x / grid_x).round() * grid_x,
                    grid_x,
                );
                snap_table_tracks(
                    &mut grid_info.rows,
                    table_y,
                    (table_y / grid_y).round() * grid_y,
                    grid_y,
                );
                *table.paint_grid_info.borrow_mut() = Some(grid_info);
            }
        }

        self.restore_clobbered_inline_breaks();

        // println!("\n\n");
        // taffy::print_tree(self, root_node_id)
    }

    /// A measure pass can run after a container's final layout pass and
    /// leave parley broken at max-content, so card text painted on one
    /// 474px line inside a 249px block. Nothing dirties afterwards,
    /// so there is no later resolve to correct it. Re-break any container
    /// whose lines do not match the box it committed to, back to that
    /// box. Steady state finds none.
    ///
    /// NaN is the other half of "doesn't match": box construction rebuilds
    /// the parley tree with no lines at all, and taffy only re-breaks it
    /// if an ancestor relaid out: so construct damage under a warm
    /// ancestor left todomvc's quote paragraph correctly sized and blank.
    ///
    /// Both triggers have exactly one writer, so the walk is skippable:
    /// content_w moves only when round_layout wrote a final_layout, and a
    /// break width is clobbered only where the rebuild also pushes the root.
    /// Non-inline nodes bail on the downcast, well under the cascade above.
    fn restore_clobbered_inline_breaks(&mut self) {
        // Debug builds run the walk anyway and assert it finds nothing, so
        // every test exercises the skip predicate rather than trusting it.
        let skip = !self.layout_changed && self.inline_roots.len() == self.inline_roots_settled;
        if skip && !cfg!(debug_assertions) {
            return;
        }
        // Rounding runs after line breaking and rounds both box edges.
        // On a cell grid, a sub-cell width change cannot move a glyph.
        let width_tol = self.round_grid.0.max(0.5);
        let mut stale: Vec<(NodeId, f32)> = Vec::new();
        let mut roots = std::mem::take(&mut self.inline_roots);
        // Only the push sites can add duplicates or unsorted ids; if none ran
        // this frame the vec is still sorted from last time.
        if roots.len() > self.inline_roots_settled {
            roots.sort_unstable();
            roots.dedup();
        }
        debug_assert!(
            !self.nodes.iter().any(|(id, n)| n
                .data
                .downcast_element()
                .is_some_and(|el| el.inline_layout_data.is_some())
                && roots.binary_search(&id).is_err()),
            "an inline root escaped the index; its breaks would never restore"
        );
        // Prune dead entries in the same pass that checks them: retain keeps
        // the sort order, so the invariant above survives.
        let nodes = &self.nodes;
        roots.retain(|&id| {
            let Some(node) = nodes.get(id) else {
                return false;
            };
            let Some(inline) = node
                .data
                .downcast_element()
                .and_then(|el| el.inline_layout_data.as_ref())
            else {
                return false;
            };
            let l = node.final_layout();
            let content_w =
                l.size.width - l.padding.left - l.padding.right - l.border.left - l.border.right;
            if content_w > 0.0 {
                let unbroken = !inline.last_break_width.is_finite();
                if unbroken || (content_w - inline.last_break_width).abs() > width_tol {
                    stale.push((id, content_w));
                }
            }
            true
        });
        self.inline_roots_settled = roots.len();
        self.inline_roots = roots;
        debug_assert!(
            !skip || stale.is_empty(),
            "skipped a resolve with {} stale inline roots: {stale:?}",
            stale.len()
        );
        for (id, content_w) in stale {
            self.rebreak_inline_to_width(id, content_w);
        }
    }
}
