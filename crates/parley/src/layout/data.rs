// Copyright 2021 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::inline_box::{InlineBox, LayoutInlineBox};
use crate::layout::spacing::{EffectiveSpacing, Justification, SpanEdges, Spacing};
use crate::layout::style_metrics::{BoxMetrics, StyleMetrics};
use crate::layout::whitespace::whitespace_hangs;
use crate::layout::{ContentWidths, LineMetrics, Style};
use crate::resolve::ResolvedStyle;
use crate::style::Brush;
use crate::{
    IndentOptions, InlineBoxKind, LineHeight, OverflowWrap, TextWrapMode, WhiteSpaceCollapse,
};
use core::ops::Range;

use alloc::vec::Vec;
use parlance::BidiLevel;
use parley_engine::shape::Whitespace;
use parley_engine::{Atom, ShapedRun, ShapedSlice, ShapedText};

/// `HarfRust`-based run data
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RunData {
    /// Font attributes, needed for accessibility.
    pub(crate) font_attrs: fontique::Attributes,
    /// Synthesis for rendering (contains variation settings)
    pub(crate) synthesis: fontique::Synthesis,
    /// The line height
    pub line_height: f32,
    /// Additional spacing inserted between this run's atoms.
    ///
    /// TODO: Letter spacing in the form of gaps should not be applied between cursive scripts, see
    /// [CSS Text 4 § 8.2.1][css-spacing-cursive]. Currently we erroneously *do* apply it.
    ///
    /// [css-spacing-cursive]: https://www.w3.org/TR/css-text-4/#cursive-tracking
    pub(crate) spacing: Spacing,
    /// The box metrics this run adds to the line box, if this run has uniform box metrics.
    ///
    /// If the metrics change within the run, this is `None`. The run's boxes should then be
    /// resolved per style.
    pub(crate) run_box: Option<BoxMetrics>,
    /// Whether this run has atoms with styles changing mid-atom.
    ///
    /// This can happen when ligatures span across styles, or (awkwardly) if a style changes within
    /// a grapheme.
    pub(crate) has_mixed_style_atoms: bool,
    /// Whether any of this run's clusters carries inline-span box-decoration advance (see
    /// [`LayoutData::cluster_lead_advance`]).
    pub(crate) has_span_edges: bool,
    /// Glyph id of the run's space character. It has no outlines, so swapping a
    /// tab glyph for this id makes paint draw nothing while the advance
    /// still moves the pen (kitesurf tab-stop expansion).
    pub(crate) space_glyph_id: u32,
    /// Advance of the run's space glyph in layout units, the unit CSS
    /// `tab-size` multiplies to compute a tab stop.
    pub(crate) space_advance: f32,
}

/// The metrics of a run's box. This differs from the style-derived box by taking into account any
/// fallback fonts (rather than just using the first available font).
///
/// Per [CSS Inline 3 § 4.1], glyphs from fonts other than the first available font only
/// contribute to the line when the `line-height` is `normal` ([`LineHeight::MetricsRelative`]);
/// otherwise the style's span box alone sizes the line and this returns `None`.
///
/// [CSS Inline 3 § 4.1]: https://drafts.csswg.org/css-inline-3/#inline-height
#[inline]
pub(crate) fn run_box_metrics<B: Brush>(
    style: &Style<B>,
    shaped_run: &ShapedRun,
    quantize: bool,
) -> Option<BoxMetrics> {
    match style.line_height {
        LineHeight::MetricsRelative(_) => {
            let (size, metrics) = (shaped_run.font_size, &shaped_run.font_metrics);
            let line_height = style.line_height.resolve(size, metrics, quantize);
            Some(BoxMetrics::from_font(metrics, line_height, quantize))
        }
        LineHeight::FontSizeRelative(_) | LineHeight::Absolute(_) => None,
    }
}

#[derive(Copy, Clone, Default, PartialEq, Debug)]
pub enum BreakReason {
    #[default]
    None,
    Regular,
    Explicit,
    Emergency,
}

/// One inline-span box-decoration fragment rectangle on a single
/// line of a `Layout`.
///
/// A "fragment" is the visible piece of an inline element (`<a>`,
/// `<span>`, etc.) on one line. CSS lets a single styled inline
/// element wrap across multiple lines, where each line gets its
/// own fragment with possibly different border edges (the
/// `box-decoration-break` property controls the details). Parley
/// only emits fragments for single-line spans; spans that wrap
/// produce no fragments at all and render as plain glyph runs.
///
/// The painter consumes this struct to draw, in order:
/// 1. Fill `BoxDecorations.background_brush` over the rectangle.
/// 2. Stroke top and bottom borders along the full width.
/// 3. If `is_first_fragment`, stroke the left border.
/// 4. If `is_last_fragment`, stroke the right border.
/// 5. Apply rounded-corner clipping for `border_radius`. Outer
///    corners (start-of-first-line, end-of-last-line) round; the
///    others stay square.
///
/// Coordinates are line-relative: `x_start` and `x_end` are
/// distances from the line's `inline_min_coord` (left edge of
/// the line's content area). Combine with the line's
/// `LineMetrics::baseline` ± ascent/descent to get absolute
/// painter coordinates.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SpanFragment {
    /// Index into `LayoutData::styles[]` for the span's own style.
    /// An ancestor span can have a different style index from its
    /// descendant glyph runs; the painter reads its box decorations
    /// through this index.
    pub style_index: u16,
    /// Index of the line this fragment belongs to. The painter
    /// pairs this with `LayoutData::lines[line_index]` to get
    /// vertical metrics (baseline, ascent, descent).
    pub line_index: u32,
    /// Line-relative left edge of the fragment, including any
    /// leading padding + border charged into the breaker's `line.x`
    /// via `cluster_lead_advance`.
    pub x_start: f32,
    /// Line-relative right edge, including any trailing padding +
    /// border charged via `cluster_trail_advance`.
    pub x_end: f32,
    /// `true` if this fragment is the first one for its span
    /// (i.e. the span begins on this line, at its first cluster).
    /// Painter draws the left border + radii only when this is
    /// true.
    pub is_first_fragment: bool,
    /// `true` if this fragment is the last one for its span
    /// (i.e. the span ends on this line, at its last cluster).
    /// Painter draws the right border + radii only when this is
    /// true. Only single-line spans emit fragments, so both flags
    /// are always true on the sole emitted fragment.
    pub is_last_fragment: bool,
}

/// A span boundary at a shaped-cluster index. The span's horizontal
/// advance can be zero, as with a background-only inline span.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct SpanBoundary {
    pub(crate) cluster_index: usize,
    pub(crate) style_index: u16,
}

#[derive(Clone, Default, Debug, PartialEq)]
pub(crate) struct LineData {
    /// Range of the source text.
    pub(crate) text_range: Range<usize>,
    /// Range of line items.
    pub(crate) item_range: Range<usize>,
    /// Metrics for the line.
    pub(crate) metrics: LineMetrics,
    /// The cause of the line break.
    pub(crate) break_reason: BreakReason,
    /// Maximum advance for the line.
    pub(crate) max_advance: f32,
    /// The number of justification opportunities on the line.
    pub(crate) num_justification_opportunities: u32,
    pub(crate) justification: Justification,
    /// This line's entries in [`LayoutData::aligned_subtree_offsets`].
    ///
    /// Empty for lines with only baseline-relative content because the root aligned subtree
    /// for each line trivially has an offset of 0
    pub(crate) aligned_subtree_offsets: Range<u32>,
}

/// Position of an [independent aligned subtree] rooted at a span with
/// `vertical-align: top | bottom` on a line. Computed for each such subtree on the line in
/// `BreakLines::finish_line`.
///
/// [independent aligned subtree]: crate::layout::style_metrics#aligned-subtrees
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AlignedSubtreeOffset {
    /// Style index of the subtree root (a span with `vertical-align: top | bottom`)
    pub(crate) root: u16,
    /// Offset from the line's baseline to the subtree's baseline (positive upwards).
    pub(crate) baseline_offset: f32,
}

impl LineData {
    /// Offset from the line's baseline to the baseline of the aligned subtree rooted at style
    /// `root` (positive upwards). `offsets` is [`LayoutData::aligned_subtree_offsets`].
    pub(crate) fn aligned_subtree_offset(
        &self,
        offsets: &[AlignedSubtreeOffset],
        root: u16,
    ) -> f32 {
        if root == 0 {
            return 0.;
        }
        let range =
            self.aligned_subtree_offsets.start as usize..self.aligned_subtree_offsets.end as usize;
        offsets[range]
            .iter()
            .find(|subtree| subtree.root == root)
            .map_or(0., |subtree| subtree.baseline_offset)
    }

    /// Absolute block-axis coordinate (offset from the top of the layout) of the baseline of the given style's span box.
    // `offsets` is [`LayoutData::aligned_subtree_offsets`].
    pub(crate) fn style_baseline(
        &self,
        offsets: &[AlignedSubtreeOffset],
        metrics: &StyleMetrics,
    ) -> f32 {
        self.metrics.baseline
            - self.aligned_subtree_offset(offsets, metrics.aligned_subtree_root)
            - metrics.baseline_offset
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LineItemData {
    /// Whether the item is a run or an inline box
    pub(crate) kind: LayoutItemKind,
    /// The index of the run or inline box in the runs or `inline_boxes` vec
    pub(crate) index: usize,
    /// Bidi level for the item (used for reordering)
    pub(crate) bidi_level: BidiLevel,

    // Fields that only apply to text runs (Ignored for boxes)
    // TODO: factor this out?
    /// Range of the source text.
    pub(crate) text_range: Range<usize>,
    /// This run's shaped clusters on this line, as a range into [`ShapedText::shaped_clusters`].
    ///
    /// The bounds are atom-aligned.
    pub(crate) shaped_cluster_range: Range<u32>,
}

impl LineItemData {
    #[inline(always)]
    pub(crate) fn is_rtl(&self) -> bool {
        self.bidi_level.is_rtl()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutItemKind {
    TextRun,
    InlineBox,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LayoutItem {
    /// Whether the item is a run or an inline box
    pub(crate) kind: LayoutItemKind,
    /// The index of the run or inline box in the runs or `inline_boxes` vec
    pub(crate) index: usize,
    /// Bidi level for the item (used for reordering)
    pub(crate) bidi_level: BidiLevel,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LayoutData<B: Brush> {
    // General settings (directly from the "builder")
    /// The display scale factor
    pub(crate) scale: f32,
    /// Whether metrics should be quantized to pixel boundaries
    pub(crate) quantize: bool,
    /// The `BiDi` base level
    pub(crate) base_level: BidiLevel,
    /// The length of the text in the layout
    pub(crate) text_len: usize,
    /// Whether any run's source text contains a tab; gates the
    /// line-commit tab expansion pass so tab-free documents pay a
    /// single boolean check per line instead of a cluster re-walk.
    /// Layout-scoped by design: a layout with one tab still walks its
    /// other lines, but tabbed layouts are rare (code blocks), so
    /// per-line tracking would not pay for its breaker-state plumbing.
    pub(crate) has_tabs: bool,

    // Output of style resolution (input to line breaking)
    pub(crate) styles: Vec<Style<B>>,
    pub(crate) style_metrics: Vec<StyleMetrics>,
    pub(crate) inline_boxes: Vec<LayoutInlineBox>,

    // Output of shaping (input to line breaking)
    pub(crate) shaped_text: ShapedText,
    pub(crate) runs: Vec<RunData>,
    pub(crate) items: Vec<LayoutItem>,

    /// Per-cluster *leading* advance contributed by an inline-span
    /// box decoration that begins at this cluster: the
    /// `padding_inline_start + border_inline_start_width` of any
    /// span whose first cluster is this one. Parallel to
    /// [`ShapedText::shaped_clusters`] when any span contributes a
    /// nonzero inline advance, or empty otherwise. Background-only
    /// spans still have fragment boundaries without these vectors.
    ///
    /// Charged as a spacing gap visually before the cluster's atom
    /// (see `EffectiveSpacing::gaps`), so line breaking, content
    /// widths, glyph positions and hit-testing all include it
    /// without the shaped advances changing.
    pub(crate) cluster_lead_advance: Vec<f32>,
    /// Per-cluster *trailing* advance contributed by an inline-
    /// span box decoration that ends at this cluster: the
    /// `padding_inline_end + border_inline_end_width` of any
    /// span whose last cluster is this one. Charged as a gap
    /// visually after the cluster's atom. See
    /// `cluster_lead_advance`.
    pub(crate) cluster_trail_advance: Vec<f32>,

    /// Span boundaries for fragment generation, including boundaries whose
    /// corresponding inline advance is zero.
    pub(crate) span_starts: Vec<SpanBoundary>,
    pub(crate) span_ends: Vec<SpanBoundary>,

    /// Per-line inline-span fragment rectangles. Each entry
    /// describes one rectangle the painter should fill (with
    /// `BoxDecorations.background_brush`) and stroke (with the
    /// per-edge `border_*_brush` values), one per styled span on
    /// each line. Generated by the line-breaker's post-pass after
    /// all lines are committed (see `BreakLines::compute_span_fragments`).
    ///
    /// Limitations:
    /// - Only emits fragments for spans whose first and last
    ///   cluster are on the *same* line (single-line spans). A
    ///   span that wraps mid-text gets no fragment, falling back
    ///   to plain glyph rendering on every line. CSS spec calls
    ///   this `box-decoration-break: slice`; multi-line spans are
    ///   not sliced.
    /// - LTR-only: x_start / x_end are line-relative measured
    ///   left-to-right. RTL bidi runs may produce visually wrong
    ///   fragments. Defer until needed.
    /// - Empty spans (whitespace-collapsed to nothing) emit no
    ///   fragment because they have no clusters.
    ///
    /// Sorted by `line_index` then `x_start` so the painter can
    /// iterate per-line in spatial order without an extra sort.
    pub(crate) span_fragments: Vec<SpanFragment>,

    // Output of line breaking
    /// The lines in the
    pub(crate) lines: Vec<LineData>,
    /// Items within each line
    pub(crate) line_items: Vec<LineItemData>,
    /// Position of each aligned subtree rooted at a `vertical-align: top | bottom` style on each line.
    /// The root aligned subtree of each line doesn't have an entry as its offset is trivially zero.
    ///
    /// Each line owns a contiguous slice ([`LineData::aligned_subtree_offsets`]).
    pub(crate) aligned_subtree_offsets: Vec<AlignedSubtreeOffset>,
    /// The width constraint that was used to line break the layout
    pub(crate) layout_max_advance: f32,
    /// The computed width of the layout excluding hanging whitespace
    pub(crate) width: f32,
    /// The computed width of the layout including hanging whitespace
    pub(crate) full_width: f32,
    /// The computed height of the layout
    pub(crate) height: f32,

    // Output of alignment
    /// The alignment that was applied to the layout, if any.
    pub(crate) alignment: Option<super::Alignment>,
    /// The text-indent amount in layout units.
    pub(crate) indent_amount: f32,
    /// Options controlling text-indent behavior (each-line, hanging).
    pub(crate) indent_options: IndentOptions,
}

impl<B: Brush> Default for LayoutData<B> {
    fn default() -> Self {
        Self {
            scale: 1.,
            quantize: true,
            base_level: BidiLevel::new(0),
            text_len: 0,
            has_tabs: false,
            width: 0.,
            full_width: 0.,
            height: 0.,
            styles: Vec::new(),
            style_metrics: Vec::new(),
            inline_boxes: Vec::new(),
            shaped_text: ShapedText::new(),
            runs: Vec::new(),
            items: Vec::new(),
            cluster_lead_advance: Vec::new(),
            cluster_trail_advance: Vec::new(),
            span_starts: Vec::new(),
            span_ends: Vec::new(),
            span_fragments: Vec::new(),
            lines: Vec::new(),
            line_items: Vec::new(),
            aligned_subtree_offsets: Vec::new(),
            alignment: None,
            layout_max_advance: 0.0,
            indent_amount: 0.0,
            indent_options: IndentOptions::default(),
        }
    }
}

impl<B: Brush> LayoutData<B> {
    pub(crate) fn clear(&mut self) {
        self.scale = 1.;
        self.quantize = true;
        self.base_level = BidiLevel::new(0);
        self.text_len = 0;
        self.has_tabs = false;
        self.width = 0.;
        self.full_width = 0.;
        self.height = 0.;
        self.layout_max_advance = 0.0;
        self.indent_amount = 0.0;
        self.indent_options = IndentOptions::default();
        self.styles.clear();
        self.style_metrics.clear();
        self.inline_boxes.clear();
        self.shaped_text.clear();
        self.runs.clear();
        self.items.clear();
        self.cluster_lead_advance.clear();
        self.cluster_trail_advance.clear();
        self.span_starts.clear();
        self.span_ends.clear();
        self.span_fragments.clear();
        self.lines.clear();
        self.line_items.clear();
        self.aligned_subtree_offsets.clear();
        self.alignment = None;
    }

    /// Push an inline box to the list of items
    pub(crate) fn push_inline_box(&mut self, index: usize, bidi_level: BidiLevel) {
        self.items.push(LayoutItem {
            kind: LayoutItemKind::InlineBox,
            index,
            bidi_level,
        });
    }
    /// Processes a shaped run into [`RunData`] and a [`LayoutItem`].
    ///
    /// `char_styles` are the style indices of the text's characters.
    #[expect(clippy::cast_possible_truncation, reason = "deferred")]
    pub(crate) fn process_shaped_run(
        &mut self,
        shaped_run_idx: usize,
        run_style: &ResolvedStyle<B>,
        spacing: Spacing,
        char_styles: &[u16],
    ) {
        let shaped_run = &self.shaped_text.runs()[shaped_run_idx];
        debug_assert!(
            !shaped_run.shaped_clusters_range.is_empty(),
            "Shaped runs returned by `parley_engine` must be non-empty"
        );
        let style_index =
            self.shaped_text.characters()[shaped_run.characters_range.start as usize].style_index;

        let line_height = self.styles[style_index as usize].line_height.resolve(
            shaped_run.font_size,
            &shaped_run.font_metrics,
            self.quantize,
        );

        // The run's box metrics, if it's uniform across all the styles within the run. A run's box
        // can change mid-run if the line height changes.
        let run_box = {
            let chars = shaped_run.range.char_range.clone();
            let first_style = char_styles[chars.start];
            let line_height = self.styles[usize::from(first_style)].line_height;

            let uniform_line_height = char_styles[chars].iter().all(|&s| {
                s == first_style || self.styles[usize::from(s)].line_height == line_height
            });

            if uniform_line_height {
                run_box_metrics(
                    &self.styles[usize::from(first_style)],
                    shaped_run,
                    self.quantize,
                )
            } else {
                None
            }
        };

        // An atom has mixed styles iff a style changes at one of its characters other than its
        // first.
        let has_mixed_style_atoms = {
            let chars = shaped_run.range.char_range.clone();
            let slice = self.shaped_text.run_slice(shaped_run_idx as u32);
            (chars.start + 1..chars.end).any(|char_idx| {
                char_styles[char_idx - 1] != char_styles[char_idx]
                    && slice
                        .atom_at_char(char_idx as u32)
                        .is_some_and(|atom| atom.char_range().start != char_idx as u32)
            })
        };

        let font = &self.shaped_text.fonts()[shaped_run.font_index];

        // Tab-stop expansion needs the space glyph's id (to
        // substitute for tab glyphs so nothing paints) and its advance
        // (the unit `tab-size` multiplies). Read both with skrifa from
        // the font and variation coordinates the run was shaped with.
        // Only runs whose source text actually contains a tab ever read
        // these, so skip the skrifa lookups for the common tab-free run.
        // Glyph id 0 (`.notdef`) doubles as "no space glyph in this font".
        let has_tab = self.shaped_text.characters()[shaped_run.characters_range.start as usize
            ..shaped_run.characters_range.end as usize]
            .iter()
            .any(|character| character.whitespace == Whitespace::Tab);
        if has_tab {
            self.has_tabs = true;
        }
        // A page can supply the font bytes, so a font that won't parse
        // takes the same no-space-glyph path rather than panicking.
        let font_ref = has_tab
            .then(|| skrifa::FontRef::from_index(font.font.data.as_ref(), font.font.index).ok())
            .flatten();
        let (space_glyph_id, space_advance) = if let Some(font_ref) = font_ref {
            let coords: smallvec::SmallVec<[skrifa::instance::NormalizedCoord; 8]> = self
                .shaped_text
                .normalized_coords()
                .get(shaped_run.normalized_coords_range.clone())
                .unwrap_or(&[])
                .iter()
                .map(|coord| skrifa::instance::NormalizedCoord::from_bits(coord.to_bits()))
                .collect();
            let space_glyph_id = skrifa::charmap::Charmap::new(&font_ref)
                .map(' ')
                .map(|g| g.to_u32())
                .unwrap_or(0);
            let space_advance = skrifa::metrics::GlyphMetrics::new(
                &font_ref,
                skrifa::instance::Size::new(shaped_run.font_size),
                coords.as_slice(),
            )
            .advance_width(skrifa::GlyphId::new(space_glyph_id))
            .unwrap_or(0.0);
            (space_glyph_id, space_advance)
        } else {
            (0, 0.0)
        };

        let run = RunData {
            font_attrs: fontique::Attributes {
                width: run_style.font_width,
                weight: run_style.font_weight,
                style: run_style.font_style,
            },
            synthesis: font.synthesis,
            line_height,
            spacing,
            run_box,
            has_mixed_style_atoms,
            // Set by `compute_span_edges` once every run is shaped.
            has_span_edges: false,
            space_glyph_id,
            space_advance,
        };

        self.runs.push(run);
        self.items.push(LayoutItem {
            kind: LayoutItemKind::TextRun,
            index: self.runs.len() - 1,
            bidi_level: shaped_run.bidi_level,
        });
    }

    /// The advance to charge for `atom` of run `run_idx` when
    /// the pen sits at `pen` within the line. A tab advances to the next
    /// CSS `tab-size` stop, so its width depends on the pen rather than on
    /// the shaped glyph (or on spacing); every other atom keeps its
    /// spaced advance. Callers that measure or fit text must use this
    /// instead of `spacing.atom_advance`, so they agree with the
    /// commit-time expansion in `expand_line_tabs`.
    #[inline(always)]
    pub(crate) fn atom_advance_at(
        &self,
        run_idx: usize,
        spacing: EffectiveSpacing<'_>,
        atom: &Atom<'_>,
        pen: f32,
    ) -> f32 {
        let cluster = &atom.shaped_clusters()[0];
        if !self.has_tabs || cluster.whitespace() != Whitespace::Tab {
            return spacing.atom_advance(atom);
        }
        let style = &self.styles[cluster.style_index as usize];
        style
            .tab_size
            .stop_advance(pen, self.runs[run_idx].space_advance)
    }

    /// The inline-span box decorations charged to the clusters of run `run_idx`, if it has any.
    #[inline(always)]
    pub(crate) fn span_edges(&self, run_idx: usize) -> Option<SpanEdges<'_>> {
        self.runs[run_idx].has_span_edges.then(|| SpanEdges {
            lead: &self.cluster_lead_advance,
            trail: &self.cluster_trail_advance,
        })
    }

    /// Calculates the min- and max-content widths of the layout.
    ///
    /// The max-content width is the widest line when breaking only at forced breaks. The
    /// min-content width is the widest line when breaking at every opportunity.
    ///
    /// This should mirror the line breaker in terms of layout decisions like hanging whitespace (if
    /// it doesn't, one of the calculations is buggy).
    ///
    /// Hanging whitespace is not considered when measuring the line's content for fit, but some
    /// whitespace hangs *conditionally*. See the module-level docs of [`crate::layout::whitespace`]
    /// for an explanation of how conditionally and unconditionally hanging whitespace are
    /// determined. For the content widths this means:
    ///
    /// - max-content width: if there's a conditionally-hanging suffix of whitespace, it fits, so
    ///   nothing hangs; without one, the unconditionally hanging whitespace hangs in full.
    /// - min-content width: any conditionally-hanging suffix hangs in full, so all hanging
    ///   whitespace hangs in full.
    pub(crate) fn calculate_content_widths(&self) -> ContentWidths {
        ContentWidthsMeasurer::new(self.indent_amount, self.indent_options).measure(self)
    }
}

struct ContentWidthsMeasurer {
    min_width: f32,
    max_width: f32,

    running_min_width: f32,
    running_max_width: f32,

    /// Whether anything has been placed on the current line yet. If not, a soft wrap opportunity
    /// doesn't end it, and if the layout ends there (e.g., directly after a forced break), the line
    /// box is not included in the layout.
    line_has_content: bool,

    /// The running advance of whitespace that would hang if a line ended here. Can exceed
    /// `running_min_width` when the hanging whitespace started before the last break
    /// opportunity, in which case the line consists entirely of hanging whitespace.
    running_hanging_whitespace: f32,
    /// The same for `running_max_width`. A tab's advance depends on the pen, which
    /// differs between the min and max accumulators, so the hanging advance actually added to
    /// each is tracked separately.
    running_hanging_whitespace_max: f32,

    /// Whether that running whitespace hangs conditionally before a forced break (following CSS
    /// Text 4 § 4.3.2, that's the case for `WhiteSpaceCollapse::Preserve`). Conditionally hanging
    /// whitespace only hangs if it doesn't fit, which here means it counts towards the max-content
    /// width, but not the min-content width.
    hangs_conditionally: bool,

    text_wrap_mode: TextWrapMode,

    /// Indents for lines following a soft/hard wrap.
    soft_indent: f32,
    hard_indent: f32,
}

impl ContentWidthsMeasurer {
    #[inline(always)]
    fn new(indent_amount: f32, indent_options: IndentOptions) -> Self {
        let first_line_indent = indent_options.indent_for_first_line(indent_amount);
        Self {
            min_width: 0.,
            max_width: 0.,
            running_min_width: first_line_indent,
            running_max_width: first_line_indent,
            running_hanging_whitespace: 0.,
            running_hanging_whitespace_max: 0.,
            hangs_conditionally: false,
            text_wrap_mode: TextWrapMode::Wrap,
            soft_indent: indent_options.indent_following_soft_wrap_break(indent_amount),
            hard_indent: indent_options.indent_following_forced_line_break(indent_amount),
            line_has_content: false,
        }
    }

    /// Ends the current min-content fragment at a soft wrap opportunity.
    ///
    /// Under a min-content constraint, every soft wrap opportunity is taken, unless the line has no
    /// content yet or its running width is still negative (due to a negative indent or inline box).
    /// The next fragment then starts a line after a soft wrap break, which will be indented iff the
    /// indent is "hanging" (see [`IndentOptions::hanging`]).
    #[inline(always)]
    fn soft_break(&mut self, line_has_content: bool) {
        if !line_has_content || self.running_min_width < 0.0 {
            return;
        }
        self.min_width = self
            .min_width
            .max(self.running_min_width - self.running_hanging_whitespace);
        self.running_min_width = self.soft_indent;
    }

    /// Ends the current line at a forced break.
    ///
    /// Note that a line that ends in a forced break is laid out even if it holds nothing else
    /// ([CSS 2.2 § 9.4.2][css-inline-formatting]).
    ///
    /// [css-inline-formatting]: https://www.w3.org/TR/CSS22/visuren.html#inline-formatting
    #[inline(always)]
    fn hard_break(&mut self) {
        self.min_width = self
            .min_width
            .max(self.running_min_width - self.running_hanging_whitespace);
        if !self.hangs_conditionally {
            self.running_max_width -= self.running_hanging_whitespace_max;
        }
        self.max_width = self.max_width.max(self.running_max_width);
        self.running_hanging_whitespace = 0.0;
        self.running_hanging_whitespace_max = 0.0;
        self.line_has_content = false;
        self.running_min_width = self.hard_indent;
        self.running_max_width = self.hard_indent;
    }

    #[inline(always)]
    fn measure<B: Brush>(mut self, layout_data: &LayoutData<B>) -> ContentWidths {
        let mut items = layout_data.items.iter();
        while let Some(item) = items.next() {
            match item.kind {
                LayoutItemKind::TextRun => {
                    let slice = layout_data.shaped_text.run_slice(item.index as u32);
                    let run_spacing = layout_data.runs[item.index].spacing;
                    let span_edges = layout_data.span_edges(item.index);
                    let tab_space_advance = layout_data
                        .has_tabs
                        .then(|| layout_data.runs[item.index].space_advance);
                    // Trailing whitespace can only hang if it ends up at the line's end edge after
                    // bidi reordering. We don't currently apply UAX #9 L1 (resetting trailing
                    // whitespace to paragraph level), so only logically-last items that match the
                    // paragraph level are guaranteed to be at that edge.
                    let can_hang = item.bidi_level == layout_data.base_level;
                    let is_rtl = item.bidi_level.is_rtl();

                    if run_spacing.is_zero() && span_edges.is_none() {
                        self.measure_text_run::<B, false>(
                            &layout_data.styles,
                            slice,
                            run_spacing,
                            None,
                            tab_space_advance,
                            can_hang,
                            is_rtl,
                        );
                    } else {
                        self.measure_text_run::<B, true>(
                            &layout_data.styles,
                            slice,
                            run_spacing,
                            span_edges,
                            tab_space_advance,
                            can_hang,
                            is_rtl,
                        );
                    }
                }
                LayoutItemKind::InlineBox => {
                    let layout_box = &layout_data.inline_boxes[item.index];
                    let inline_box = &layout_box.inline_box;
                    let parent_style_index = usize::from(layout_box.parent_style_index);
                    let box_wrap_mode = layout_data.styles[parent_style_index].text_wrap_mode;
                    if inline_box.kind == InlineBoxKind::InFlow {
                        let next = items.as_slice().iter().find(|next| {
                            next.kind == LayoutItemKind::TextRun
                                || layout_data.inline_boxes[next.index].inline_box.kind
                                    == InlineBoxKind::InFlow
                        });
                        // A forced break right after a box suppresses the box's soft wrap
                        // opportunity.
                        let followed_by_inline_content = match next {
                            Some(next) if next.kind == LayoutItemKind::TextRun => {
                                let slice = layout_data.shaped_text.run_slice(next.index as u32);
                                slice.shaped_clusters()[0].whitespace() != Whitespace::Newline
                            }
                            Some(_) => true,
                            None => false,
                        };
                        self.measure_inline_box(
                            inline_box,
                            box_wrap_mode,
                            followed_by_inline_content,
                        );
                    }
                }
            }
        }

        // The end of a layout is considered to be a forced line break as per CSS Text 4 § 5, so
        // whitespace can hang conditionally.
        if self.line_has_content {
            self.hard_break();
        }

        // Negative-width inline boxes (e.g. negative margins) can make the max-content width
        // smaller than the min-content width. CSS Sizing 3 § 2.1 requires the max-content size to
        // be floored by the min-content size.
        self.max_width = self.max_width.max(self.min_width);

        ContentWidths {
            min: self.min_width,
            max: self.max_width,
        }
    }

    /// Measures a text run.
    ///
    /// The run is scanned one shaped cluster at a time. break opportunities are only considered at
    /// grapheme starts, and the hanging-whitespace accumulator is updated per cluster (a cluster
    /// that doesn't hang resets it, one that does extends it), which is equivalent to the per-atom
    /// "hangs entirely / hangs partially from the logical end" rule of
    /// [`atom_hanging_advance`](crate::layout::whitespace::atom_hanging_advance).
    ///
    /// With `SPACED`, each atom's spacing gap is added to the cluster on its visual end, i.e., its
    /// logically last cluster for LTR and its logically first cluster for RTL. The gap then hangs
    /// with that cluster, as in `atom_hanging_advance`. Inline-span box decorations
    /// (`span_edges`) are added to the clusters they are charged to. Without `SPACED`, `spacing`
    /// and `span_edges` are ignored.
    ///
    /// With `tab_space_advance` (the run's space advance, given when the layout has
    /// tabs), a tab's advance is the jump to the next `tab-size` stop from the pen, which is the
    /// accumulator itself, so min and max get their own advance (without spacing, matching the
    /// breaker's `LayoutData::atom_advance_at`).
    #[inline(always)]
    fn measure_text_run<B: Brush, const SPACED: bool>(
        &mut self,
        styles: &[Style<B>],
        slice: ShapedSlice<'_>,
        spacing: Spacing,
        span_edges: Option<SpanEdges<'_>>,
        tab_space_advance: Option<f32>,
        can_hang: bool,
        is_rtl: bool,
    ) {
        let first_cluster = slice.shaped_clusters_range().start as usize;
        let clusters = slice.shaped_clusters();
        // The cluster index at which nothing has been placed on the current line yet, if any.
        let mut empty_at = if self.line_has_content { usize::MAX } else { 0 };

        // The spacing gap of the current atom.
        let mut gap = 0.;
        let mut skip_atom = false;
        for (i, cluster) in clusters.iter().enumerate() {
            let whitespace = cluster.whitespace();
            let style = &styles[cluster.style_index as usize];
            let is_atom_start = i == 0 || cluster.is_grapheme_start();
            if is_atom_start {
                skip_atom = false;
                let prev_text_wrap_mode = self.text_wrap_mode;
                self.text_wrap_mode = style.text_wrap_mode;
                if prev_text_wrap_mode == TextWrapMode::Wrap
                    && (cluster.is_soft_wrap_opportunity_before()
                        || style.overflow_wrap == OverflowWrap::Anywhere)
                {
                    self.soft_break(i != empty_at);
                }
                // `Whitespace::Newline` are forced breaks. Note newlines have no advance.
                if whitespace == Whitespace::Newline {
                    // Newlines hang, so whitespace before them keeps hanging.
                    self.hard_break();
                    empty_at = i + 1;
                    skip_atom = true;
                    continue;
                }
                if SPACED {
                    gap = spacing.gaps(whitespace).after;
                }
            } else if skip_atom {
                // The next line starts after the whole newline atom.
                empty_at = i + 1;
                continue;
            }

            let mut advance = cluster.advance;
            if SPACED {
                let owns_gap = if is_rtl {
                    is_atom_start
                } else {
                    clusters
                        .get(i + 1)
                        .is_none_or(|next| next.is_grapheme_start())
                };
                if owns_gap {
                    advance += gap;
                }
                if let Some(edges) = span_edges {
                    advance += edges.lead[first_cluster + i] + edges.trail[first_cluster + i];
                }
            }
            let (min_advance, max_advance) = match tab_space_advance {
                Some(space_advance) if whitespace == Whitespace::Tab => (
                    style
                        .tab_size
                        .stop_advance(self.running_min_width, space_advance),
                    style
                        .tab_size
                        .stop_advance(self.running_max_width, space_advance),
                ),
                _ => (advance, advance),
            };
            self.running_min_width += min_advance;
            self.running_max_width += max_advance;

            // A cluster hangs if all of its characters hang. Its first character is checked via the
            // cached flags so the common case never touches `characters`.
            let hangs = can_hang
                && whitespace_hangs(whitespace, style)
                && (cluster.char_len() == 1
                    || slice
                        .characters_in(cluster.chars_range())
                        .iter()
                        .all(|character| {
                            whitespace_hangs(
                                character.whitespace,
                                &styles[character.style_index as usize],
                            )
                        }));
            if hangs {
                self.running_hanging_whitespace += min_advance;
                self.running_hanging_whitespace_max += max_advance;
                self.hangs_conditionally =
                    style.white_space_collapse == WhiteSpaceCollapse::Preserve;
            } else {
                self.running_hanging_whitespace = 0.0;
                self.running_hanging_whitespace_max = 0.0;
            }
        }
        self.line_has_content = empty_at != clusters.len();
    }

    fn measure_inline_box(
        &mut self,
        inline_box: &InlineBox,
        box_wrap_mode: TextWrapMode,
        followed_by_inline_content: bool,
    ) {
        // Inline boxes have soft wrap opportunities on both sides.
        // The prior mode applies before the box; its parent mode applies after it.
        if self.text_wrap_mode == TextWrapMode::Wrap {
            self.soft_break(self.line_has_content);
        }
        // Inline boxes don't hang.
        self.running_hanging_whitespace = 0.0;
        self.running_hanging_whitespace_max = 0.0;
        self.running_min_width += inline_box.width;
        self.running_max_width += inline_box.width;
        self.line_has_content = true;
        self.text_wrap_mode = box_wrap_mode;
        if box_wrap_mode == TextWrapMode::Wrap && followed_by_inline_content {
            self.soft_break(true);
            // Nothing has been placed on the new line yet.
            self.line_has_content = false;
        }
    }
}
