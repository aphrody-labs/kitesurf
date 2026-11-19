//! Dropping the shaped text of inline roots far from the viewport.
//!
//! Shaped runs, glyphs and lines are the largest derived data of a long
//! document. Once a root's box is settled they are only read by paint and
//! geometry queries, so far-away roots drop them and `TextLayout::layout`
//! rebuilds them on the next read, replaying the line break they last had.
//!
//! With eviction on, box construction collects only each root's text, the
//! layout pass shapes a root inside its own layout call and evicts it right
//! after, so no pass holds the shaped text of the whole document at once.

use crate::BaseDocument;
use crate::node::{
    AlignInputs, EvictedBreak, EvictedLayout, EvictedSummary, OutOfFlow, StaticX, SummaryCell,
    TextLayout,
};
use blitz_traits::node_id::NodeId;
use style::values::specified::box_::DisplayOutside;

/// Roots laid out most recently in a pass keep their text until this many
/// more are laid out: about 2.5 KB each on long documents.
const IN_PASS_WINDOW: usize = 256;

/// Roots a pass keeps shaped because they lie near the viewport.
const IN_PASS_KEEP_BUDGET: usize = 512;

/// The state an eviction scan ran against, so an unchanged resolve can skip it.
#[derive(Clone, Copy)]
pub(crate) struct InlineEvictionScan {
    scroll_y: f64,
    inner_scroll: f64,
    layout_epoch: u64,
    spared_read_layouts: bool,
}

/// The document-space rows a root must overlap to keep its shaped text.
#[derive(Clone, Copy)]
struct KeepBand {
    top: f64,
    bottom: f64,
    viewport_height: f64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Eviction {
    Evicted,
    /// Kept for now: the eviction scan visits the root again.
    Refused,
    /// Kept until a layout pass breaks the root again, as only that can
    /// give it a summary: the scan stops visiting it.
    Unsummarized,
}

/// What an eviction scan does with one resident inline root.
#[derive(Clone, Copy)]
enum RootScan {
    /// Holds no shaped text to evict, or is not far enough away.
    Keep,
    /// Far, but read since the last scan: this scan spares it and the next one ages it.
    Spare,
    /// Far and unread: evict it.
    Far,
}

impl BaseDocument {
    /// Evict inline roots farther than `viewports` viewport heights above or
    /// below the viewport (`None` disables eviction).
    pub fn set_inline_layout_eviction(&mut self, viewports: Option<f32>) {
        if self.inline_layout_eviction_viewports.is_none() && viewports.is_some() {
            // Layouts built while eviction was off were never tracked.
            self.inline_resident_unknown = true;
        }
        self.inline_layout_eviction_viewports = viewports;
        self.last_inline_eviction_scan = None;
    }

    /// Keep the shaped text of inline roots overlapping the document rows
    /// `top..bottom` as well as those near the viewport (`None` stops). A
    /// print sets the rows of its pages before laying them out, so painting
    /// them does not rebuild what the layout pass just shaped.
    pub fn set_inline_keep_extent(&mut self, extent: Option<(f64, f64)>) {
        if self.inline_keep_extent != extent {
            self.inline_keep_extent = extent;
            // Roots the old extent kept may now be far.
            self.last_inline_eviction_scan = None;
        }
    }

    /// Record an inner scroller scrolling by `dy`: it moves roots through the
    /// keep band as viewport scroll does, so the next scan must not skip.
    pub fn note_inner_scroll(&mut self, dy: f64) {
        self.inline_eviction_inner_scroll += dy.abs();
    }

    /// Number of inline roots the next eviction scan will visit (an upper bound
    /// on the roots holding shaped text, before pruning).
    pub fn inline_resident_count(&self) -> usize {
        self.inline_resident.len() + self.inline_rebuilt.lock().unwrap().len()
    }

    /// Drop `node_id`'s shaped text if it can be rebuilt with identical lines.
    /// Returns whether it was evicted.
    pub fn evict_inline_layout(&mut self, node_id: NodeId) -> bool {
        self.try_evict_inline_layout(node_id) == Eviction::Evicted
    }

    fn try_evict_inline_layout(&mut self, node_id: NodeId) -> Eviction {
        let scale = self.viewport.scale();
        // Decide before the style lookups: every scan retries the far roots
        // this refuses.
        let Some(inline) = self.inline_layout_mut(node_id) else {
            return Eviction::Refused;
        };
        if !inline.replayable_break || !inline.last_break_width.is_finite() {
            return Eviction::Refused;
        }
        if inline.is_evicted() {
            return Eviction::Refused;
        }
        let layout = inline.live_mut();
        // Scale changes rebuild every layout, but a stale one cannot replay.
        if layout.scale() != scale {
            return Eviction::Refused;
        }
        // Without a summary every width change would reshape the root, so
        // such roots keep their shaped text.
        let Some(lines) = layout.break_summary() else {
            return Eviction::Unsummarized;
        };
        // A summary holds only absolutely positioned boxes, the line each
        // lands on, and where along it when their static position needs that.
        let out_of_flow: Box<[NodeId]> = layout
            .inline_boxes()
            .map(|ibox| NodeId::from_u64(ibox.id))
            .collect();
        if !out_of_flow.is_empty() && is_vertical(&self.nodes[node_id]) {
            return Eviction::Unsummarized;
        }
        let needs_static_x = !out_of_flow
            .iter()
            .all(|&id| self.abspos_static_x_is_unused(id));
        let clamp_lines = self.inline_clamp_lines(node_id);
        let (_, last_line_alignment) = self.inline_alignment(node_id);
        let inline = self
            .inline_layout_mut(node_id)
            .expect("the root was found above");
        let layout = inline.live_mut();
        let max_advance = layout.layout_max_advance();
        let indent = layout.text_indent();
        let alignment = layout.alignment();
        let static_x = if needs_static_x {
            let Some(x) = static_x_of(layout, &out_of_flow) else {
                return Eviction::Unsummarized;
            };
            let aligned = AlignInputs {
                max_advance,
                indent,
                alignment,
                last_line_alignment,
                clamp_lines,
            };
            Some(StaticX { aligned, x })
        } else {
            None
        };
        let boxes = layout
            .inline_boxes()
            .map(|ibox| (ibox.width, ibox.height, ibox.baseline))
            .collect();
        // Content widths the layout pass measured stay in `inline.content_widths`.
        let out_of_flow = (!out_of_flow.is_empty())
            .then(|| Box::new(OutOfFlow { ids: out_of_flow, static_x }));
        let summary = Some(EvictedSummary { lines, out_of_flow });
        let breaking = EvictedBreak::new(
            max_advance,
            indent,
            alignment,
            last_line_alignment,
            clamp_lines,
            boxes,
            SummaryCell::new(summary),
        );
        inline.evict(EvictedLayout {
            node_id,
            scale,
            breaking: Some(Box::new(breaking)),
        });
        Eviction::Evicted
    }

    /// Whether the static position of the absolutely positioned box `id`
    /// does not depend on where along its line it is: a block-level box's
    /// starts at the content box edge, and an inline-axis inset replaces it.
    pub(super) fn abspos_static_x_is_unused(&self, id: NodeId) -> bool {
        let Some(node) = self.nodes.get(id) else {
            return false;
        };
        let Some(style) = node.primary_styles() else {
            return false;
        };
        if is_vertical(node) {
            return false;
        }
        if style.get_box().original_display.outside() != DisplayOutside::Inline {
            return true;
        }
        let position = style.get_position();
        !position.left.is_auto() || !position.right.is_auto()
    }

    fn inline_keep_band(&self, viewports: f32) -> Option<KeepBand> {
        let scale = self.viewport.scale_f64();
        let viewport_height = self.viewport.window_size.1 as f64 / scale;
        if viewport_height <= 0.0 {
            return None;
        }
        let scroll_y = self.viewport_scroll().y;
        let band = viewport_height * viewports as f64;
        Some(KeepBand {
            top: scroll_y - band,
            bottom: scroll_y + viewport_height + band,
            viewport_height,
        })
    }

    /// The document rows of `id`'s box, as last committed.
    fn inline_root_rows(&self, id: NodeId) -> (f64, f64) {
        let (pos, layout) = self.physical_unrounded_geometry(id);
        let top = pos.y as f64;
        (top, top + layout.size.height as f64)
    }

    /// Whether `id`'s box lies outside `band`.
    fn inline_root_is_far(&self, id: NodeId, band: KeepBand) -> bool {
        let (top, bottom) = self.inline_root_rows(id);
        bottom < band.top || top > band.bottom
    }

    /// Whether `id`'s box overlaps the extent set by [`Self::set_inline_keep_extent`].
    fn inline_root_in_keep_extent(&self, id: NodeId) -> bool {
        self.inline_keep_extent.is_some_and(|(keep_top, keep_bottom)| {
            let (top, bottom) = self.inline_root_rows(id);
            bottom >= keep_top && top <= keep_bottom
        })
    }

    /// Called after a pass breaks a root's lines, to measure or lay it out.
    /// The root stays shaped while it is among the last [`IN_PASS_WINDOW`]
    /// broken, since tables and flex containers lay a child out again soon
    /// after.
    pub(crate) fn evict_inline_layout_after_break(&mut self, node_id: NodeId) {
        if self.inline_layout_eviction_viewports.is_none() {
            return;
        }
        // The low bits of the seq word carry `ROOT_INDEXED`, so step the seq past it.
        self.inline_pass_seq =
            self.inline_pass_seq.wrapping_add(1) & !TextLayout::ROOT_INDEXED;
        let seq = self.inline_pass_seq;
        let Some(inline) = self.inline_layout_mut(node_id) else {
            return;
        };
        inline.set_in_pass_seq(seq);
        self.inline_pass_window.push_back((node_id, seq));
        if self.inline_pass_window.len() > IN_PASS_WINDOW {
            let (id, seq) = self.inline_pass_window.pop_front().expect("over capacity");
            self.leave_inline_pass_window(id, seq);
        }
    }

    /// Decide whether a root leaving the window keeps its text. Ancestors may
    /// still be unplaced, so its position is a lower bound: near roots stay
    /// shaped (the first paint needs them) up to a budget per pass.
    fn leave_inline_pass_window(&mut self, id: NodeId, seq: u32) {
        let Some(viewports) = self.inline_layout_eviction_viewports else {
            return;
        };
        let Some(band) = self.inline_keep_band(viewports) else {
            self.inline_resident.push(id);
            return;
        };
        // A newer entry for this root is still in the window, or it was laid
        // out from its break summary and never rebuilt.
        if self
            .inline_layout_mut(id)
            .is_none_or(|inline| inline.in_pass_seq() != seq || inline.is_evicted())
        {
            return;
        }
        // The keep extent is not budgeted: its owner asked for those roots.
        let kept = self.inline_root_in_keep_extent(id);
        let near = !kept
            && self.inline_pass_kept < IN_PASS_KEEP_BUDGET
            && !self.inline_root_is_far(id, band);
        if kept || near || self.try_evict_inline_layout(id) == Eviction::Refused {
            self.inline_pass_kept += usize::from(near);
            self.inline_resident.push(id);
        }
    }

    fn inline_layout_mut(&mut self, id: NodeId) -> Option<&mut TextLayout> {
        self.nodes
            .get_mut(id)
            .and_then(|node| node.data.downcast_element_mut())
            .and_then(|el| el.inline_layout_data_mut())
    }

    /// Evict far-away inline roots after a resolve or paint. Visits only the
    /// roots that may hold shaped text: all of them when the geometry or the
    /// scroll position changed, or the last scan spared recently read layouts
    /// (one more scan ages them), and otherwise only the ones rebuilt since.
    pub fn evict_offscreen_inline_layouts(&mut self) {
        let Some(viewports) = self.inline_layout_eviction_viewports else {
            return;
        };
        let mut rebuilt = std::mem::take(&mut *self.inline_rebuilt.lock().unwrap());
        rebuilt.extend(self.inline_pass_window.drain(..).map(|(id, _)| id));
        self.inline_pass_kept = 0;
        if self.inline_resident_unknown {
            self.inline_resident_unknown = false;
            rebuilt.extend_from_slice(&self.inline_roots);
        }

        let Some(band) = self.inline_keep_band(viewports) else {
            self.keep_every_resident_root(&mut rebuilt);
            return;
        };

        let scroll_y = self.viewport_scroll().y;
        let inner_scroll = self.inline_eviction_inner_scroll;
        let layout_epoch = self.inline_eviction_layout_epoch;
        let unmoved = self.last_inline_eviction_scan.filter(|last| {
            last.layout_epoch == layout_epoch
                && (scroll_y - last.scroll_y).abs() + (inner_scroll - last.inner_scroll)
                    < band.viewport_height / 2.0
        });
        let has_new_rebuilt = !rebuilt.is_empty();
        let follow_up = unmoved.is_some_and(|last| last.spared_read_layouts);
        let mut visit = match unmoved {
            // Nothing moved: only fresh rebuilds need a look, and aging the
            // others again would evict a layout read once per frame.
            Some(last) if !last.spared_read_layouts => {
                if rebuilt.is_empty() {
                    return;
                }
                rebuilt
            }
            _ => {
                self.inline_resident.append(&mut rebuilt);
                std::mem::take(&mut self.inline_resident)
            }
        };

        visit.sort_unstable();
        visit.dedup();
        let mut spared = false;
        let mut far = Vec::new();
        visit.retain(|&id| match self.scan_inline_root(id, band) {
            RootScan::Keep => true,
            RootScan::Spare => {
                spared = true;
                true
            }
            RootScan::Far => {
                far.push(id);
                false
            }
        });
        self.inline_resident.append(&mut visit);
        // Stable reads do not chain follow-ups, but fresh rebuilds arriving
        // during one still need their own aging scan.
        self.last_inline_eviction_scan = Some(match unmoved {
            Some(last) if !follow_up => InlineEvictionScan {
                spared_read_layouts: spared,
                ..last
            },
            _ => InlineEvictionScan {
                scroll_y,
                inner_scroll,
                layout_epoch,
                spared_read_layouts: spared && (!follow_up || has_new_rebuilt),
            },
        });
        for id in far {
            if self.try_evict_inline_layout(id) == Eviction::Refused {
                self.inline_resident.push(id);
            }
        }
    }

    /// Eviction with no keep band, as with a zero-height viewport: every root that still
    /// holds shaped text stays resident, and the next scan visits all of them.
    fn keep_every_resident_root(&mut self, rebuilt: &mut Vec<NodeId>) {
        self.inline_resident.append(rebuilt);
        self.inline_resident.sort_unstable();
        self.inline_resident.dedup();
        self.inline_resident.retain(|&id| {
            self.nodes.get(id).is_some_and(|node| {
                node.flags.is_in_document()
                    && node
                        .element_data()
                        .and_then(|el| el.inline_layout_data())
                        .is_some_and(|inline| !inline.is_evicted())
            })
        });
        self.last_inline_eviction_scan = None;
    }

    /// What an eviction scan does with one resident root.
    fn scan_inline_root(&self, id: NodeId, band: KeepBand) -> RootScan {
        let Some(node) = self.nodes.get(id) else {
            return RootScan::Keep;
        };
        if !node.flags.is_in_document() {
            return RootScan::Keep;
        }
        // Evicted roots rejoin through the rebuild queue, and roots that
        // cannot replay rejoin when a layout pass leaves them live.
        let Some(inline) = node
            .element_data()
            .and_then(|el| el.inline_layout_data())
            .filter(|inline| inline.replayable_break && !inline.is_evicted())
        else {
            return RootScan::Keep;
        };
        if !self.inline_root_is_far(id, band) || self.inline_root_in_keep_extent(id) {
            return RootScan::Keep;
        }
        // Visible layouts are read every frame: only far ones age.
        if inline.take_recently_read() {
            return RootScan::Spare;
        }
        RootScan::Far
    }
}

/// Each box's offset along its line in `layout`, in `ids` order, or `None`
/// when one was clamped away. Looked up by id: bidi lines list items visually.
fn static_x_of(
    layout: &parley::layout::Layout<crate::node::TextBrush>,
    ids: &[NodeId],
) -> Option<Box<[f32]>> {
    let mut placed: Vec<(u64, f32)> = layout
        .lines()
        .flat_map(|line| line.items())
        .filter_map(|item| match item {
            parley::layout::PositionedLayoutItem::InlineBox(ibox) => Some((ibox.id, ibox.x)),
            parley::layout::PositionedLayoutItem::GlyphRun(_) => None,
        })
        .collect();
    placed.sort_unstable_by_key(|&(id, _)| id);
    ids.iter()
        .map(|id| {
            let at = placed.binary_search_by_key(&id.as_u64(), |&(id, _)| id).ok()?;
            Some(placed[at].1)
        })
        .collect()
}

/// Whether `node` lays out in a vertical writing mode.
pub(super) fn is_vertical(node: &crate::Node) -> bool {
    #[cfg(feature = "writing-mode")]
    return node.writing_mode().is_vertical();
    #[cfg(not(feature = "writing-mode"))]
    {
        let _ = node;
        false
    }
}
