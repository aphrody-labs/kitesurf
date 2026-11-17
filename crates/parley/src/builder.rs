// Copyright 2021 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Context for layout.

use super::FontContext;
use super::context::LayoutContext;
use super::style::{Brush, StyleProperty, TextStyle};

use super::layout::Layout;
use super::layout::SpanBoundary;

use alloc::string::String;
use core::ops::{Bound, Range, RangeBounds};
use parley_engine::break_overrides::LineBreakOverrideFn;

use crate::InlineBoxKind;
use crate::bidi::BaseDirection;
use crate::inline_box::{InlineBox, LayoutInlineBox};
use crate::resolve::{ResolvedStyle, StyleRun};

#[derive(Clone, Copy)]
pub(crate) struct BuilderOptions<'a> {
    scale: f32,
    quantize: bool,
    base_direction: BaseDirection,
    line_break_override: Option<&'a LineBreakOverrideFn>,
}

impl BuilderOptions<'_> {
    pub(crate) fn new(scale: f32, quantize: bool) -> Self {
        Self {
            scale,
            quantize,
            base_direction: BaseDirection::Auto,
            line_break_override: None,
        }
    }
}

/// Builder for constructing a text layout with ranged attributes.
///
/// This builder does not collapse whitespace. For whitespace collapsing, use the [`TreeBuilder`]
#[must_use]
pub struct RangedBuilder<'a, B: Brush> {
    pub(crate) options: BuilderOptions<'a>,
    pub(crate) lcx: &'a mut LayoutContext<B>,
    pub(crate) fcx: &'a mut FontContext,
}

impl<'b, B: Brush> RangedBuilder<'b, B> {
    pub fn push_default<'a>(&mut self, property: impl Into<StyleProperty<'a, B>>) {
        let resolved =
            self.lcx
                .rcx
                .resolve_property(self.fcx, &property.into(), self.options.scale);
        self.lcx.ranged_style_builder.push_default(resolved);
    }

    pub fn push<'a>(
        &mut self,
        property: impl Into<StyleProperty<'a, B>>,
        range: impl RangeBounds<usize>,
    ) {
        let resolved =
            self.lcx
                .rcx
                .resolve_property(self.fcx, &property.into(), self.options.scale);
        self.lcx.ranged_style_builder.push(resolved, range);
    }

    pub fn push_inline_box(&mut self, inline_box: InlineBox) {
        self.lcx.inline_boxes.push(LayoutInlineBox {
            inline_box,
            parent_style_index: 0,
            baseline_offset: 0.,
        });
    }

    /// Sets the paragraph's base direction.
    ///
    /// The default is [`BaseDirection::Auto`], which infers the direction from the text.
    pub fn set_base_direction(&mut self, base_direction: BaseDirection) {
        self.options.base_direction = base_direction;
    }

    /// Set the callback which will be called as a first provider of line breaking decisions.
    ///
    /// See [`LineBreakOverrideFn`] for more details.
    pub fn set_line_break_override(&mut self, overrides: Option<&'b LineBreakOverrideFn>) {
        self.options.line_break_override = overrides;
    }

    pub fn build_into(self, layout: &mut Layout<B>, text: impl AsRef<str>) {
        // Apply RangedStyleBuilder styles directly to style-table/style-run state.
        self.lcx
            .ranged_style_builder
            .finish(&mut self.lcx.style_table, &mut self.lcx.style_runs);

        // Call generic layout builder method
        build_into_layout(layout, text.as_ref(), self.lcx, self.fcx, self.options);
    }

    pub fn build(self, text: impl AsRef<str>) -> Layout<B> {
        let mut layout = Layout::default();
        self.build_into(&mut layout, text);
        layout
    }
}

/// Builder for constructing a text layout from a style table and
/// indexed style runs.
///
/// This builder does not collapse whitespace. For whitespace collapsing, use the [`TreeBuilder`]
#[must_use]
pub struct StyleRunBuilder<'a, B: Brush> {
    pub(crate) options: BuilderOptions<'a>,
    pub(crate) len: usize,
    pub(crate) lcx: &'a mut LayoutContext<B>,
    pub(crate) fcx: &'a mut FontContext,
    pub(crate) cursor: usize,
}

impl<'b, B: Brush> StyleRunBuilder<'b, B> {
    /// Reserves additional capacity for styles and runs.
    ///
    /// This is an optional optimization for callers that know counts
    /// up front; call it before pushing styles and runs to reduce
    /// reallocations.
    pub fn reserve(&mut self, additional_styles: usize, additional_runs: usize) {
        self.lcx.style_table.reserve(additional_styles);
        self.lcx.style_runs.reserve(additional_runs);
    }

    /// Adds a fully-specified style to the shared style table and
    /// returns its index.
    pub fn push_style<'family, 'settings>(
        &mut self,
        style: TextStyle<'family, 'settings, B>,
    ) -> u16 {
        let resolved = self
            .lcx
            .rcx
            .resolve_entire_style_set(self.fcx, &style, self.options.scale);
        let style_index = self.lcx.style_table.len();
        assert!(style_index <= u16::MAX as usize, "too many styles");
        self.lcx.style_table.push(resolved);
        style_index as u16
    }

    /// Adds a style run referencing an entry from the style table.
    ///
    /// Runs must be contiguous and non-overlapping, and must cover
    /// `0..text.len()` once all runs have been added.
    pub fn push_style_run(&mut self, style_index: u16, range: impl RangeBounds<usize>) {
        let range = resolve_range(range, self.len);
        assert!(
            range.start == self.cursor,
            "StyleRunBuilder expects contiguous non-overlapping runs"
        );
        assert!(
            range.start <= range.end,
            "StyleRunBuilder expects ordered ranges"
        );
        assert!(
            (style_index as usize) < self.lcx.style_table.len(),
            "StyleRunBuilder expects style indices that were previously added via push_style"
        );
        self.lcx.style_runs.push(StyleRun {
            style_index,
            range: range.clone(),
            tree_span_index: None,
            // Same rationale as the ranged-builder StyleRun emit:
            // this builder has no nested span concept, so each
            // explicit `push_style_run` is treated as a complete
            // singleton span. Box decorations on the referenced
            // style charge enter+leave advance once for this run.
            is_span_first: true,
            is_span_last: true,
        });
        self.cursor = range.end;
    }

    pub fn push_inline_box(&mut self, inline_box: InlineBox) {
        self.lcx.inline_boxes.push(LayoutInlineBox {
            inline_box,
            parent_style_index: 0,
            baseline_offset: 0.,
        });
    }

    /// Sets the paragraph's base direction.
    ///
    /// The default is [`BaseDirection::Auto`], which infers the direction from the text.
    pub fn set_base_direction(&mut self, base_direction: BaseDirection) {
        self.options.base_direction = base_direction;
    }

    /// Set the callback which will be called as a first provider of line breaking decisions.
    ///
    /// See [`LineBreakOverrideFn`] for more details.
    pub fn set_line_break_override(&mut self, overrides: Option<&'b LineBreakOverrideFn>) {
        self.options.line_break_override = overrides;
    }

    pub fn build_into(self, layout: &mut Layout<B>, text: impl AsRef<str>) {
        assert!(
            self.cursor == self.len,
            "StyleRunBuilder requires runs that cover the full text"
        );
        if self.lcx.style_runs.is_empty() {
            // This is reachable for empty text. In this case, the layout still needs a style and an
            // empty style run, e.g. to size a cursor.
            if self.lcx.style_table.is_empty() {
                let style = self.lcx.rcx.resolve_entire_style_set(
                    self.fcx,
                    &TextStyle::default(),
                    self.options.scale,
                );
                self.lcx.style_table.push(style);
            }
            self.lcx.style_runs.push(StyleRun {
                style_index: 0,
                range: 0..0,
                tree_span_index: None,
                // Empty layout: flag conservatively as a singleton.
                // The flag values are inert: there are no clusters
                // to charge span advance to.
                is_span_first: true,
                is_span_last: true,
            });
        }
        build_into_layout(layout, text.as_ref(), self.lcx, self.fcx, self.options);
    }

    pub fn build(self, text: impl AsRef<str>) -> Layout<B> {
        let mut layout = Layout::default();
        self.build_into(&mut layout, text);
        layout
    }
}

/// Builder for constructing a text layout with a tree of attributes.
#[must_use]
pub struct TreeBuilder<'a, B: Brush> {
    pub(crate) options: BuilderOptions<'a>,
    pub(crate) lcx: &'a mut LayoutContext<B>,
    pub(crate) fcx: &'a mut FontContext,
}

impl<'b, B: Brush> TreeBuilder<'b, B> {
    pub fn push_style_span(&mut self, style: TextStyle<'_, '_, B>) {
        let resolved = self
            .lcx
            .rcx
            .resolve_entire_style_set(self.fcx, &style, self.options.scale);
        self.lcx.tree_style_builder.push_style_span(resolved);
    }

    pub fn push_style_modification_span<'s, 'iter>(
        &mut self,
        properties: impl IntoIterator<Item = &'iter StyleProperty<'s, B>>,
    ) where
        's: 'iter,
        B: 'iter,
    {
        self.lcx
            .tree_style_builder
            .push_style_modification_span(properties.into_iter().map(|p| {
                self.lcx
                    .rcx
                    .resolve_property(self.fcx, p, self.options.scale)
            }));
    }

    pub fn pop_style_span(&mut self) {
        self.lcx.tree_style_builder.pop_style_span();
    }

    pub fn push_text(&mut self, text: &str) {
        self.lcx.tree_style_builder.push_text(text);
    }

    /// Returns the pushed text so far, after whitespace processing, but excluding any trailing
    /// collapsible whitespace.
    ///
    /// This is a prefix of the text that [`Self::build`] will return. Whether that trailing
    /// collapsible whitespace ends up being kept (as a single space) depends on where it is and on
    /// what follows it. See [`Self::has_pending_whitespace`].
    pub fn text(&self) -> &str {
        self.lcx.tree_style_builder.text()
    }

    /// Returns whether [`Self::text`] is followed by collapsible whitespace.
    ///
    /// That white space is not yet part of [`Self::text`]: depending on where it is and what
    /// follows, it is either removed or kept as a single space.
    pub fn has_pending_whitespace(&self) -> bool {
        self.lcx.tree_style_builder.has_pending_whitespace()
    }

    /// Current layout-text length, counting pending whitespace only when it would be kept.
    pub fn current_text_len(&self) -> usize {
        self.lcx
            .tree_style_builder
            .text_len_with_pending_whitespace()
    }

    pub fn push_inline_box(&mut self, mut inline_box: InlineBox) {
        if inline_box.kind == InlineBoxKind::InFlow {
            self.lcx.tree_style_builder.flush_pending_whitespace();
            self.lcx.tree_style_builder.set_last_item_is_inline_box();
        }

        // TODO: arrange type better here to factor out the index
        inline_box.index = self.lcx.tree_style_builder.text().len();
        let parent_style_index = self.lcx.tree_style_builder.resolve_current_style_id();
        self.lcx.inline_boxes.push(LayoutInlineBox {
            inline_box,
            parent_style_index,
            baseline_offset: 0.,
        });
    }

    /// Sets the paragraph's base direction.
    ///
    /// The default is [`BaseDirection::Auto`], which infers the direction from the text.
    pub fn set_base_direction(&mut self, base_direction: BaseDirection) {
        self.options.base_direction = base_direction;
    }

    /// Set the callback which will be called as a first provider of line breaking decisions.
    ///
    /// See [`LineBreakOverrideFn`] for more details.
    pub fn set_line_break_override(&mut self, overrides: Option<&'b LineBreakOverrideFn>) {
        self.options.line_break_override = overrides;
    }

    #[inline]
    pub fn build_into(self, layout: &mut Layout<B>) -> String {
        // Apply TreeStyleBuilder styles to LayoutContext.
        let text = self
            .lcx
            .tree_style_builder
            .finish(&mut self.lcx.style_table, &mut self.lcx.style_runs);

        // Call generic layout builder method
        build_into_layout(layout, &text, self.lcx, self.fcx, self.options);

        text
    }

    #[inline]
    pub fn build(self) -> (Layout<B>, String) {
        let mut layout = Layout::default();
        let text = self.build_into(&mut layout);
        (layout, text)
    }
}

fn build_into_layout<B: Brush>(
    layout: &mut Layout<B>,
    text: &str,
    lcx: &mut LayoutContext<B>,
    fcx: &mut FontContext,
    options: BuilderOptions<'_>,
) {
    if text.is_empty() && lcx.style_runs.is_empty() {
        lcx.style_table.push(ResolvedStyle::default());
        lcx.style_runs.push(StyleRun {
            style_index: 0,
            range: 0..0,
            tree_span_index: None,
            // Empty layout: flag conservatively as a singleton.
            // The default ResolvedStyle has empty BoxDecorations
            // anyway, so the line-breaker will not charge any
            // span advance for it; the flag values are inert.
            is_span_first: true,
            is_span_last: true,
        });
    }
    assert!(
        !lcx.style_runs.is_empty(),
        "at least one style run is required"
    );

    crate::analysis::analyze_text(
        lcx,
        text,
        options.base_direction,
        options.line_break_override,
    );

    layout.data.clear();
    layout.data.scale = options.scale;
    layout.data.quantize = options.quantize;
    layout.data.base_level = lcx.analysis.paragraph_level();
    layout.data.text_len = text.len();

    // Characters not covered by any style run (e.g. the space substituted for empty text during
    // analysis) use the root style.
    lcx.char_style_indices.clear();
    lcx.char_style_indices
        .resize(lcx.analysis.char_info().len(), 0);
    let mut char_index = 0;
    for style_run in &lcx.style_runs {
        for _ in text[style_run.range.clone()].chars() {
            lcx.char_style_indices[char_index] = style_run.style_index;
            char_index += 1;
        }
    }

    // Copy the visual styles into the layout
    layout
        .data
        .styles
        .extend(lcx.style_table.iter().map(|s| s.as_layout_style()));

    // Sort the inline boxes as subsequent code assumes that they are in text index order.
    // Note: It's important that this is a stable sort to allow users to control the order of contiguous inline boxes
    //
    // TODO: consider dropping the sort and requiring `push_inline_box` callers to push boxes in text index order
    // (as `TreeBuilder` already does).
    lcx.inline_boxes.sort_by_key(|b| b.inline_box.index);

    {
        super::shape::shape_text(
            &lcx.rcx,
            fcx,
            &lcx.style_table,
            &lcx.inline_boxes,
            &lcx.analysis,
            &lcx.char_style_indices,
            &mut lcx.scx,
            text,
            layout,
            &lcx.analysis_data_sources,
        );
    }

    // After shaping, so that the metrics of fonts used for shaping can be reused.
    crate::layout::style_metrics::resolve_style_metrics(
        &lcx.rcx,
        fcx,
        &lcx.style_table,
        &layout.data.shaped_text,
        options.scale,
        options.quantize,
        &mut layout.data.style_metrics,
    );

    // Move inline boxes into the layout
    layout.data.inline_boxes.clear();
    core::mem::swap(&mut layout.data.inline_boxes, &mut lcx.inline_boxes);

    compute_span_edges(layout, lcx);
}

/// Computes per-cluster inline advances and span boundaries for
/// decorated inline spans. Boundaries are separate from advances
/// because a background-only span still needs a fragment.
/// Tree-built runs contribute to their source span and every ancestor;
/// flat runs use their explicit first/last flags.
///
/// Why post-shaping: clusters don't exist before shaping.
/// Why on absolute text offsets: a single StyleRun can span
/// multiple shaped runs (font / script / bidi splits). A cluster's
/// absolute byte offset is the `text_byte_start` of its first
/// character, comparable to StyleRun.range.
///
/// The contributions are applied as spacing gaps (see
/// `layout::spacing::EffectiveSpacing::gaps`): the lead is a gap
/// visually before the cluster's atom and the trail a gap after
/// it, so line breaking, content widths, glyph positions and
/// cluster geometry all see the padding+border, the same way they
/// see letter spacing. Like letter spacing, the gaps are applied in
/// visual order, so this is LTR-only.
///
fn compute_span_edges<B: Brush>(layout: &mut Layout<B>, lcx: &LayoutContext<B>) {
    let data = &mut layout.data;
    data.cluster_lead_advance.clear();
    data.cluster_trail_advance.clear();
    data.span_starts.clear();
    data.span_ends.clear();

    let decoration_edges = |style_index: u16| {
        let decos = &lcx.style_table[usize::from(style_index)].box_decorations;
        let lead = decos.padding_inline_start + decos.border_inline_start_width;
        let trail = decos.padding_inline_end + decos.border_inline_end_width;
        (lead, trail, decos.background_brush.is_some())
    };
    let needs_span = |style_index: u16| {
        let (lead, trail, has_background) = decoration_edges(style_index);
        lead > 0.0 || trail > 0.0 || has_background
    };
    let has_decorated_span = lcx.style_runs.iter().any(|style_run| {
        if needs_span(style_run.style_index) {
            return true;
        }
        let Some(mut span_index) = style_run.tree_span_index else {
            return false;
        };
        loop {
            if needs_span(span_index) {
                return true;
            }
            let parent = lcx.style_table[usize::from(span_index)].parent;
            if parent == span_index {
                return false;
            }
            span_index = parent;
        }
    });
    if !has_decorated_span {
        return;
    }

    let shaped_text = &data.shaped_text;
    let characters = shaped_text.characters();
    let clusters = shaped_text.shaped_clusters();

    // Shaped clusters are stored in logical order, so their
    // absolute offsets ascend and a single sweep finds the first
    // and last cluster of each (sorted, non-overlapping) style run.
    // A run with no clusters in range, which happens when a span
    // contains only collapsed whitespace, contributes nothing.
    let mut style_run_idx = 0;
    let mut first_last: alloc::vec::Vec<Option<(usize, usize)>> =
        alloc::vec![None; lcx.style_runs.len()];
    for (ci, cluster) in clusters.iter().enumerate() {
        let abs = characters[cluster.chars_range().start as usize].text_byte_start as usize;
        while style_run_idx < lcx.style_runs.len()
            && lcx.style_runs[style_run_idx].range.end <= abs
        {
            style_run_idx += 1;
        }
        let Some(style_run) = lcx.style_runs.get(style_run_idx) else {
            break;
        };
        if abs < style_run.range.start {
            continue;
        }
        let entry = &mut first_last[style_run_idx];
        match entry {
            Some((_, last)) => *last = ci,
            None => *entry = Some((ci, ci)),
        }
    }

    // A tree span can receive text through any descendant. Accumulate
    // each run's cluster range into every enclosing tree span before
    // placing that span's decoration boundaries.
    let mut tree_span_first_last = alloc::vec::Vec::new();
    for (style_run, run_clusters) in lcx.style_runs.iter().zip(first_last.iter().copied()) {
        let (Some(mut span_index), Some((first, last))) = (style_run.tree_span_index, run_clusters)
        else {
            continue;
        };
        if tree_span_first_last.is_empty() {
            tree_span_first_last.resize(lcx.style_table.len(), None);
        }
        loop {
            let entry = &mut tree_span_first_last[usize::from(span_index)];
            match entry {
                Some((_, span_last)) => *span_last = last,
                None => *entry = Some((first, last)),
            }
            let parent = lcx.style_table[usize::from(span_index)].parent;
            if parent == span_index {
                break;
            }
            span_index = parent;
        }
    }

    for (style_run, entry) in lcx.style_runs.iter().zip(first_last) {
        let Some((first, last)) = entry else {
            continue;
        };
        if style_run.tree_span_index.is_some() {
            continue;
        }
        let (span_lead, span_trail, has_background) = decoration_edges(style_run.style_index);
        let lead = if style_run.is_span_first {
            span_lead
        } else {
            0.0
        };
        let trail = if style_run.is_span_last {
            span_trail
        } else {
            0.0
        };
        if lead != 0.0 || trail != 0.0 {
            if data.cluster_lead_advance.is_empty() {
                data.cluster_lead_advance.resize(clusters.len(), 0.0);
                data.cluster_trail_advance.resize(clusters.len(), 0.0);
            }
            data.cluster_lead_advance[first] += lead;
            data.cluster_trail_advance[last] += trail;
        }
        if lead <= 0.0 && trail <= 0.0 && !has_background {
            continue;
        }
        if style_run.is_span_first {
            data.span_starts.push(SpanBoundary {
                cluster_index: first,
                style_index: style_run.style_index,
            });
        }
        if style_run.is_span_last {
            data.span_ends.push(SpanBoundary {
                cluster_index: last,
                style_index: style_run.style_index,
            });
        }
    }

    // Tree style IDs are allocated parent-first, so starts on the
    // same cluster are pushed outer-to-inner. Ends are reversed by
    // cluster below to pop inner-to-outer.
    for (style_index, entry) in tree_span_first_last.into_iter().enumerate() {
        let Some((first, last)) = entry else {
            continue;
        };
        let style_index = u16::try_from(style_index).expect("style index fits u16");
        let (lead, trail, has_background) = decoration_edges(style_index);
        if lead != 0.0 || trail != 0.0 {
            if data.cluster_lead_advance.is_empty() {
                data.cluster_lead_advance.resize(clusters.len(), 0.0);
                data.cluster_trail_advance.resize(clusters.len(), 0.0);
            }
            data.cluster_lead_advance[first] += lead;
            data.cluster_trail_advance[last] += trail;
        }
        if lead <= 0.0 && trail <= 0.0 && !has_background {
            continue;
        }
        data.span_starts.push(SpanBoundary {
            cluster_index: first,
            style_index,
        });
        data.span_ends.push(SpanBoundary {
            cluster_index: last,
            style_index,
        });
    }

    data.span_starts
        .sort_by_key(|boundary| boundary.cluster_index);
    data.span_ends
        .sort_by_key(|boundary| boundary.cluster_index);
    let mut end = 0;
    while end < data.span_ends.len() {
        let cluster_index = data.span_ends[end].cluster_index;
        let mut next = end + 1;
        while next < data.span_ends.len() && data.span_ends[next].cluster_index == cluster_index {
            next += 1;
        }
        data.span_ends[end..next].reverse();
        end = next;
    }

    for (run, shaped_run) in data.runs.iter_mut().zip(shaped_text.runs()) {
        let range = shaped_run.shaped_clusters_range.start as usize
            ..shaped_run.shaped_clusters_range.end as usize;
        let leads = data.cluster_lead_advance.get(range.clone()).unwrap_or(&[]);
        let trails = data.cluster_trail_advance.get(range).unwrap_or(&[]);
        run.has_span_edges = leads.iter().chain(trails).any(|advance| *advance != 0.0);
    }
}

fn resolve_range(range: impl RangeBounds<usize>, len: usize) -> Range<usize> {
    let start = match range.start_bound() {
        Bound::Unbounded => 0,
        Bound::Included(n) => *n,
        Bound::Excluded(n) => *n + 1,
    };
    let end = match range.end_bound() {
        Bound::Unbounded => len,
        Bound::Included(n) => *n + 1,
        Bound::Excluded(n) => *n,
    };
    start.min(len)..end.min(len)
}
