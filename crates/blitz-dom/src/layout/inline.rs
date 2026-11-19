use blitz_traits::node_id::NodeId;
use parley::{AlignmentOptions, BreakReason, IndentOptions};
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use style::values::{
    computed::{BaselineSource, CSSPixelLength, Contain},
    generics::text::GenericTextIndent,
};
use taffy::{
    AvailableSpace, AxisStaticEdge, AxisStaticPosition, BlockContainerStyle, BlockContext, BlockFormattingContext,
    BoxSizing, CollapsibleMarginSet, CompactLength, CoreStyle as _, Direction, LayoutInput,
    LayoutOutput, LayoutPartialTree as _, MaybeMath as _, MaybeResolve as _, OofCandidate,
    OofCandidates, OofItemStyle, OofPositioningArea, Overflow, Point, RequestedAxis,
    ResolveOrZero as _, RunMode, Size, SizingMode,
};

#[cfg(feature = "floats")]
use parley::YieldData;
#[cfg(feature = "floats")]
use taffy::{BlockItemStyle as _, Clear, Float, prelude::TaffyMaxContent};

use stylo_taffy::StyleFlags;

use super::resolve_calc_value;
use crate::layout::LayoutPassState;
use crate::node::{TextBrush, TextLayout, count_inline_summary};
use crate::stylo_to_parley;

/// Subtract a child's margins from the definite axes of the available space it is laid out in.
/// Taffy's convention is that the parent subtracts a child's margins from the available space
/// before passing it to the child, and that the child does not subtract them again.
fn subtract_margins(mut child_inputs: LayoutInput, margin: taffy::Rect<f32>) -> LayoutInput {
    child_inputs.available_space = child_inputs
        .available_space
        .maybe_sub(margin.sum_axes())
        .maybe_max(Size::ZERO);
    child_inputs
}

/// Layout inputs for an atomic inline box, with any sizing keyword on its `width` style
/// (`min-content`, `max-content`, `fit-content`, `fit-content(...)`, `stretch`) resolved
/// into the available space or known width the box is measured with.
///
/// `height` keywords resolve here too. taffy only resolves a sizing
/// keyword on a node's children, and in an inline formatting context blitz is
/// that parent: `min-content`/`max-content` measure the box against that space,
/// and `stretch` fills the definite space left after the margins. Every
/// measurement of the box goes through this, so both agree.
fn inline_box_inputs(
    width_style: taffy::Dimension,
    height_style: taffy::Dimension,
    margin: taffy::Rect<f32>,
    child_inputs: LayoutInput,
) -> LayoutInput {
    let mut inputs = subtract_margins(child_inputs, margin);
    let stretch_width = inputs.available_space.width.into_option();
    let percent_basis = child_inputs.parent_size.width;

    match width_style.tag() {
        CompactLength::MIN_CONTENT_TAG => inputs.available_space.width = AvailableSpace::MinContent,
        CompactLength::MAX_CONTENT_TAG => inputs.available_space.width = AvailableSpace::MaxContent,
        CompactLength::FIT_CONTENT_PX_TAG => {
            inputs.available_space.width = AvailableSpace::Definite(width_style.value())
        }
        CompactLength::FIT_CONTENT_PERCENT_TAG => {
            if let Some(basis) = percent_basis {
                inputs.available_space.width =
                    AvailableSpace::Definite(basis * width_style.value());
            }
        }
        CompactLength::FIT_CONTENT_KEYWORD_TAG => {
            if let Some(width) = stretch_width {
                inputs.available_space.width = AvailableSpace::Definite(width);
            }
        }
        _ if width_style.is_fit_content_calc() => {
            if let Some(basis) = percent_basis {
                inputs.available_space.width =
                    AvailableSpace::Definite(resolve_calc_value(width_style.calc_value(), basis));
            }
        }
        CompactLength::STRETCH_TAG => {
            if let Some(width) = stretch_width {
                inputs.known_dimensions.width = Some(width);
                inputs.available_space.width = AvailableSpace::Definite(width);
            }
        }
        _ => {}
    }

    match height_style.tag() {
        CompactLength::MIN_CONTENT_TAG => {
            inputs.available_space.height = AvailableSpace::MinContent
        }
        CompactLength::MAX_CONTENT_TAG => {
            inputs.available_space.height = AvailableSpace::MaxContent
        }
        CompactLength::STRETCH_TAG => {
            if let Some(height) = inputs.available_space.height.into_option() {
                inputs.known_dimensions.height = Some(height);
                inputs.available_space.height = AvailableSpace::Definite(height);
            }
        }
        _ => {}
    }
    inputs
}

impl LayoutPassState<'_> {
    pub(crate) fn compute_inline_layout(
        &mut self,
        node_id: NodeId,
        inputs: taffy::tree::LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> taffy::LayoutOutput {
        let LayoutInput {
            known_dimensions,
            parent_size,
            run_mode,
            ..
        } = inputs;
        let style = self.nodes[node_id].layout_style();

        // Pull these out earlier to avoid borrowing issues
        let is_scroll_container =
            style.overflow().x.is_scroll_container() || style.overflow().y.is_scroll_container();
        let padding = style
            .padding()
            .resolve_or_zero(parent_size.width, resolve_calc_value);
        let border = style
            .border()
            .resolve_or_zero(parent_size.width, resolve_calc_value);
        let padding_border_size = (padding + border).sum_axes();
        let box_sizing_adjustment = if style.box_sizing() == BoxSizing::ContentBox {
            padding_border_size
        } else {
            Size::ZERO
        };

        // Resolve node's preferred/min/max sizes (width/heights) against the available space (percentages resolve to pixel values)
        // For ContentSize mode, we pretend that the node has no size styles as these should be ignored.
        let (clamped_style_size, min_size, max_size, _aspect_ratio) = match inputs.sizing_mode {
            SizingMode::ContentSize => {
                let node_size = known_dimensions;
                let node_min_size = Size::NONE;
                let node_max_size = Size::NONE;
                (node_size, node_min_size, node_max_size, None)
            }
            SizingMode::InherentSize => {
                let aspect_ratio = style.aspect_ratio();
                let style_size = style
                    .size()
                    .maybe_resolve(parent_size, resolve_calc_value)
                    .maybe_apply_aspect_ratio(aspect_ratio)
                    .maybe_add(box_sizing_adjustment);
                let style_min_size = style
                    .min_size()
                    .maybe_resolve(parent_size, resolve_calc_value)
                    .maybe_apply_aspect_ratio(aspect_ratio)
                    .maybe_add(box_sizing_adjustment);
                let style_max_size = style
                    .max_size()
                    .maybe_resolve(parent_size, resolve_calc_value)
                    .maybe_add(box_sizing_adjustment);

                let node_size =
                    known_dimensions.or(style_size.maybe_clamp(style_min_size, style_max_size));
                (node_size, style_min_size, style_max_size, aspect_ratio)
            }
        };

        // If both min and max in a given axis are set and max <= min then this determines the size in that axis
        let min_max_definite_size = min_size.zip_map(max_size, |min, max| match (min, max) {
            (Some(min), Some(max)) if max <= min => Some(min),
            _ => None,
        });

        let styled_based_known_dimensions = known_dimensions
            .or(min_max_definite_size)
            .or(clamped_style_size)
            .maybe_max(padding_border_size);

        // Short-circuit layout if the container's size is fully determined by the container's size and the run mode
        // is ComputeSize (and thus the container's size is all that we're interested in)
        if run_mode == RunMode::ComputeSize {
            if let Size {
                width: Some(width),
                height: Some(height),
            } = styled_based_known_dimensions
            {
                return LayoutOutput::from_outer_size(Size { width, height });
            }
        }

        drop(style);

        // Unwrap the block formatting context if one was passed, or else create a new one
        match block_ctx {
            Some(inherited_bfc) if !is_scroll_container => self.compute_inline_layout_inner(
                node_id,
                LayoutInput {
                    known_dimensions: styled_based_known_dimensions,
                    ..inputs
                },
                inherited_bfc,
            ),
            _ => {
                let mut root_bfc = BlockFormattingContext::new();
                let mut root_ctx = root_bfc.root_block_context();
                self.compute_inline_layout_inner(
                    node_id,
                    LayoutInput {
                        known_dimensions: styled_based_known_dimensions,
                        ..inputs
                    },
                    &mut root_ctx,
                )
            }
        }
    }

    /// The out-of-flow candidate for the absolutely positioned inline box `box_id` on `line`,
    /// in a container whose content box spans `content_inline`.
    fn abspos_inline_candidate(
        &self,
        box_id: NodeId,
        order: u32,
        line: AbsposLine,
        content_inline: taffy::Line<f32>,
        container_direction: Direction,
    ) -> OofCandidate {
        let style = self.child_layout_style(&self.nodes[box_id]);
        let item_direction = style.direction();
        // Inline formatting contexts have no `justify-items`/`align-items` for an
        // `auto` self-alignment to defer to, so it behaves as `normal`.
        let justify_self = OofItemStyle::justify_self(&style).unwrap_or(taffy::AlignItems::NORMAL);
        let align_self = OofItemStyle::align_self(&style).unwrap_or(taffy::AlignItems::NORMAL);
        // The static position of an absolutely positioned box depends on the
        // display its hypothetical box would have had (the display specified
        // before position:absolute blockified it): inline-level boxes sit at
        // their position within the line, while block-level boxes start at the
        // content-box left edge of their containing block.
        let is_inline_level =
            style.style.get_box().original_display.outside() == DisplayOutside::Inline;
        // The static-position rectangle
        // (https://www.w3.org/TR/css-position-3/#staticpos-rect):
        // - An inline-level box's rectangle is zero-width at its position
        //   within the line and spans the line box in the block axis.
        // - A block-level box's rectangle spans the containing block's content
        //   box in the inline axis and is zero-height below the line box.
        let (inline_area, block_area) = if is_inline_level {
            (
                taffy::Line {
                    start: line.x,
                    end: line.x,
                },
                taffy::Line {
                    start: line.top,
                    end: line.bottom,
                },
            )
        } else {
            (
                content_inline,
                taffy::Line {
                    start: line.bottom,
                    end: line.bottom,
                },
            )
        };
        OofCandidate {
            node: crate::taffy_node_id(box_id),
            order,
            position: style.position(),
            static_position: taffy::Point {
                x: AxisStaticPosition::from_alignment(
                    justify_self.resolve_self_relative(item_direction, container_direction, true),
                    inline_area,
                    container_direction.is_rtl(),
                ),
                y: AxisStaticPosition::from_alignment(
                    align_self.resolve_self_relative(item_direction, container_direction, false),
                    block_area,
                    false,
                ),
            },
        }
    }

    /// The out-of-flow positions for which an inline span between the inline box `box_id` and its
    /// inline root `root_id` is a containing block (as `SPAN_*_CB` flags)
    fn inline_span_cb_flags(&self, root_id: NodeId, box_id: NodeId) -> StyleFlags {
        let root = &self.nodes[root_id];
        // The spans of an anonymous inline root are descendants of its parent
        let root_parent = root
            .is_anonymous()
            .then_some(root.parent_id())
            .flatten();

        let mut flags = StyleFlags::empty();
        let mut current = self.nodes[box_id].parent_id();
        while let Some(id) = current {
            if id == root_id || Some(id) == root_parent {
                return flags;
            }
            let ancestor = &self.nodes[id];
            if let Some(style) = ancestor.primary_styles() {
                let display = style.clone_display();
                if display.outside() == DisplayOutside::Inline
                    && display.inside() == DisplayInside::Flow
                {
                    let claims = stylo_taffy::convert::inline_containing_block_claims(&style);
                    if claims.absolute {
                        flags |= StyleFlags::SPAN_ABSOLUTE_CB;
                    }
                    if claims.fixed {
                        flags |= StyleFlags::SPAN_FIXED_CB;
                    }
                }
            }
            current = ancestor.parent_id();
        }
        StyleFlags::empty()
    }

    fn compute_inline_layout_inner(
        &mut self,
        node_id: NodeId,
        inputs: taffy::tree::LayoutInput,
        block_ctx: &mut BlockContext<'_>,
    ) -> taffy::LayoutOutput {
        let scale = self.viewport.scale();
        let LayoutInput {
            known_dimensions,
            parent_size,
            available_space,
            sizing_mode,
            ..
        } = inputs;

        // Take inline layout to satisfy borrow checker
        let mut inline_layout = self.nodes[node_id]
            .data
            .downcast_element_mut()
            .unwrap()
            .take_inline_layout()
            .unwrap();
        // A cache miss on an evicted root: break it from its summary if it has
        // one, and reshape it before laying out otherwise. From here on, a root
        // still evicted has a summary.
        if inline_layout.evicted_summary().is_none() {
            if inline_layout.evicted_break().is_some() {
                count_inline_summary(|stats| stats.unsummarized += 1);
            }
            inline_layout.layout_mut(&self.nodes);
        }

        let style = self.nodes[node_id].layout_style();

        // Note: both horizontal and vertical percentage padding/borders are resolved against the container's inline size (i.e. width).
        // This is not a bug, but is how CSS is specified (see: https://developer.mozilla.org/en-US/docs/Web/CSS/padding#values)
        let padding = style
            .padding()
            .resolve_or_zero(parent_size.width, resolve_calc_value);
        let border = style
            .border()
            .resolve_or_zero(parent_size.width, resolve_calc_value);
        let container_pb = padding + border;
        let pb_sum = container_pb.sum_axes();
        let box_sizing_adjustment = if style.box_sizing() == BoxSizing::ContentBox {
            pb_sum
        } else {
            Size::ZERO
        };

        // Scrollbar gutters are reserved when the `overflow` property is set to `Overflow::Scroll`.
        // However, the axis are switched (transposed) because a node that scrolls vertically needs
        // *horizontal* space to be reserved for a scrollbar
        let scrollbar_gutter = style.overflow().transpose().map(|overflow| match overflow {
            Overflow::Scroll => style.scrollbar_width(),
            _ => 0.0,
        });
        // TODO: make side configurable based on the `direction` property
        let mut content_box_inset = container_pb;
        content_box_inset.right += scrollbar_gutter.x;
        content_box_inset.bottom += scrollbar_gutter.y;

        let has_styles_preventing_being_collapsed_through = !style.is_block()
            || style.overflow().x.is_scroll_container()
            || style.overflow().y.is_scroll_container()
            || style.position().is_out_of_flow()
            || padding.top > 0.0
            || padding.bottom > 0.0
            || border.top > 0.0
            || border.bottom > 0.0;
        // || matches!(node_size.height, Some(h) if h > 0.0)
        // || matches!(node_min_size.height, Some(h) if h > 0.0)
        // || !inline_layout.text.is_empty();
        // || inline_layout.live().inline_boxes().len() > 0;

        // Resolve node's preferred/min/max sizes (width/heights) against the available space (percentages resolve to pixel values)
        // For ContentSize mode, we pretend that the node has no size styles as these should be ignored.
        let (node_size, node_min_size, node_max_size, aspect_ratio) = match sizing_mode {
            SizingMode::ContentSize => {
                let node_size = known_dimensions;
                let node_min_size = Size::NONE;
                let node_max_size = Size::NONE;
                (node_size, node_min_size, node_max_size, None)
            }
            SizingMode::InherentSize => {
                let aspect_ratio = style.aspect_ratio();
                let style_size = style
                    .size()
                    .maybe_resolve(parent_size, resolve_calc_value)
                    .maybe_apply_aspect_ratio(aspect_ratio)
                    .maybe_add(box_sizing_adjustment);
                let style_min_size = style
                    .min_size()
                    .maybe_resolve(parent_size, resolve_calc_value)
                    .maybe_apply_aspect_ratio(aspect_ratio)
                    .maybe_add(box_sizing_adjustment);
                let style_max_size = style
                    .max_size()
                    .maybe_resolve(parent_size, resolve_calc_value)
                    .maybe_add(box_sizing_adjustment);

                let node_size =
                    known_dimensions.or(style_size.maybe_clamp(style_min_size, style_max_size));
                (node_size, style_min_size, style_max_size, aspect_ratio)
            }
        };

        let suppresses_baseline = style.contain().suppresses_baseline();
        drop(style);

        // An inline with `unicode-bidi: isolate` (UA style for `[dir]`) or
        // `embed` wraps its content in bidi controls at construction, and
        // those are text to parley: a lone `<span dir=ltr>` became a 19px
        // line of nothing. They render nothing, so the text is empty for
        // the line-box rules below. tldraw's canvas sits in Radix's
        // `<span dir="ltr">` and every panel below it slid down a line.
        let text_kind = inline_layout.text_kind(&self.nodes);
        let text_is_only_bidi_controls = text_kind.only_bidi_controls;

        // Short circuit if inline context contains no text or inline boxes
        if !has_styles_preventing_being_collapsed_through
            && text_is_only_bidi_controls
            && match inline_layout.live_layout() {
                Some(layout) => layout.inline_boxes().len() == 0,
                None => inline_layout
                    .evicted_summary()
                    .is_none_or(|summary| summary.out_of_flow.is_none()),
            }
        {
            inline_layout.replayable_break = false;
            self.put_back_inline_layout(node_id, inline_layout);
            return LayoutOutput::from_outer_size(
                Size::ZERO.maybe_max(container_pb.sum_axes().map(Some)),
            );
        }

        // Compute available space
        let available_space = Size {
            width: known_dimensions
                .width
                .map(AvailableSpace::from)
                .unwrap_or(available_space.width)
                .maybe_set(known_dimensions.width)
                .maybe_set(node_size.width)
                .map_definite_value(|size| {
                    (size.maybe_clamp(node_min_size.width, node_max_size.width)
                        - content_box_inset.horizontal_axis_sum())
                    .max(0.0)
                }),
            height: known_dimensions
                .height
                .map(AvailableSpace::from)
                .unwrap_or(available_space.height)
                .maybe_set(known_dimensions.height)
                .maybe_set(node_size.height)
                .map_definite_value(|size| {
                    (size.maybe_clamp(node_min_size.height, node_max_size.height)
                        - content_box_inset.vertical_axis_sum())
                    .max(0.0)
                }),
        };

        // Compute size of inline boxes
        let child_inputs = taffy::tree::LayoutInput {
            known_dimensions: Size::NONE,
            available_space,
            sizing_mode: SizingMode::InherentSize,
            parent_size: Size {
                width: available_space.width.into_option(),
                // Anonymous blocks do not establish the containing block for percentages.
                height: if self.nodes[node_id].is_anonymous() {
                    parent_size.height
                } else {
                    available_space.height.into_option()
                },
            },
            // Atomic inlines (e.g. inline-block) establish independent formatting
            // contexts: their margins never collapse with their children's margins.
            vertical_margins_are_collapsible: taffy::Line::FALSE,
            ..inputs
        };
        #[cfg(feature = "floats")]
        let float_child_inputs = taffy::tree::LayoutInput {
            available_space: Size::MAX_CONTENT,
            ..child_inputs
        };

        // Update inline boxes
        for ibox in inline_layout
            .live_layout_mut()
            .into_iter()
            .flat_map(|layout| layout.inline_boxes_mut())
        {
            let style = self.child_layout_style(&self.nodes[NodeId::from_u64(ibox.id)]);
            let margin = style
                .margin()
                .resolve_or_zero(inputs.parent_size, resolve_calc_value);

            #[cfg(feature = "floats")]
            let is_floated = style.float().is_floated();
            #[cfg(not(feature = "floats"))]
            let is_floated = false;

            let is_out_of_flow = style.position().is_out_of_flow();
            // `baseline-source: auto` selects the last baseline for inline-blocks and the first
            // for other atomic inlines. A block-axis scroll-container inline-block (except buttons)
            // instead uses its bottom margin edge, but only with `auto` (css-align-3 §9.1).
            // `overflow: clip` is not a scroll container. Exported scroll-container baselines
            // are clamped to the border box.
            // A layout-contained box is treated as having no baseline (css-contain-1 §3.3).
            let overflow = style.overflow();
            let box_style = style.style.get_box();
            let is_flow = matches!(
                style.effective_display().inside(),
                DisplayInside::Flow | DisplayInside::FlowRoot
            );
            let baseline_source = box_style.clone_baseline_source();
            let uses_last_baseline = match baseline_source {
                BaselineSource::Auto => is_flow,
                BaselineSource::First => false,
                BaselineSource::Last => true,
            };
            let is_scroll_container = !matches!(overflow.y, Overflow::Visible | Overflow::Clip);
            let is_block_axis_scroll_container = is_flow
                && is_scroll_container
                && !style
                    .flags
                    .intersects(StyleFlags::IS_BUTTON | StyleFlags::IS_BUTTON_INPUT);
            let contain_layout = box_style.clone_contain().contains(Contain::LAYOUT);
            let exports_baseline = !(baseline_source == BaselineSource::Auto
                && is_block_axis_scroll_container)
                && !contain_layout;
            let box_inputs = inline_box_inputs(style.size().width, style.size().height, margin, child_inputs);
            drop(style);

            if is_out_of_flow || is_floated {
                ibox.width = 0.0;
                ibox.height = 0.0;
                ibox.baseline = None;
            } else {
                let output = self.compute_child_layout(taffy::NodeId::from(ibox.id), box_inputs);
                ibox.width = (margin.left + margin.right + output.size.width) * scale;
                ibox.baseline = if exports_baseline {
                    let baseline = if uses_last_baseline {
                        output.baselines.last.or(output.baselines.first)
                    } else {
                        output.baselines.first.or(output.baselines.last)
                    };
                    baseline.map(|baseline| {
                        let baseline = if is_scroll_container {
                            baseline.clamp(0.0, output.size.height)
                        } else {
                            baseline
                        };
                        (margin.top + baseline) * scale
                    })
                } else {
                    None
                };
                // Vertical margins adjust the space the box reserves in the line. A box with a
                // baseline splits that space into ascent (`margin.top + baseline`) and descent
                // (`margin.bottom + height - baseline`), either of which may be negative. A box
                // without a baseline sits on the baseline and cannot reserve negative space.
                // Kept finite: huge author lengths can sum to infinity, which is taller than
                // the line breaker's `f32::MAX` height limit, and it then yields
                // `MaxHeightExceeded` for this box forever without advancing.
                let margin_box_height = margin.top + margin.bottom + output.size.height;
                ibox.height = if ibox.baseline.is_some() {
                    (margin_box_height * scale).min(f32::MAX)
                } else {
                    (margin_box_height.max(0.0) * scale).min(f32::MAX)
                };
            }
        }

        let text_indent = self.nodes[node_id]
            .primary_styles()
            .map(|s| s.clone_text_indent())
            .unwrap_or_else(GenericTextIndent::zero);
        let indent_options = IndentOptions {
            each_line: text_indent.each_line,
            hanging: text_indent.hanging,
        };
        // Percentage indents do not contribute to intrinsic widths.
        let fixed_indent = text_indent.length.resolve(CSSPixelLength::new(0.0)).px() * scale;
        if let Some(layout) = inline_layout.live_layout_mut() {
            layout.set_text_indent(fixed_indent, indent_options);
        }

        let pbw = container_pb.horizontal_components().sum() * scale;
        let width = known_dimensions
            .width
            .map(|w| (w * scale) - pbw)
            .unwrap_or_else(|| {
                // TODO: Cache content widths.
                //
                // This is a little tricky as the size of the inline boxes may depend on whether we are sizing under
                // and a min-content or max-content constraint. So if we want to compute both widths in one pass then
                // we need to store both a min-content and max-content size on each box.
                let stored = inline_layout
                    .content_widths
                    .filter(|&(indent, _)| inline_layout.is_evicted() && indent == fixed_indent);
                let content_sizes = match stored {
                    Some((_, widths)) => widths,
                    None => {
                        let layout = inline_layout.layout_mut(&self.nodes);
                        layout.set_text_indent(fixed_indent, indent_options);
                        let widths = layout.calculate_content_widths();
                        let independent = layout.inline_boxes().all(|ibox| {
                            ibox.kind == parley::InlineBoxKind::OutOfFlow
                        });
                        inline_layout.content_widths =
                            independent.then_some((fixed_indent, widths));
                        widths
                    }
                };
                let min_content_width = content_sizes.min;
                let max_content_width = content_sizes.max;

                #[cfg(feature = "floats")]
                let float_width = match available_space.width {
                    AvailableSpace::Definite(_) => 0.0,
                    AvailableSpace::MinContent => {
                        let mut width: f32 = 0.0;
                        for ibox in inline_layout
                            .live_layout_mut()
                            .into_iter()
                            .flat_map(|layout| layout.inline_boxes_mut())
                        {
                            let (is_floated, margin) = {
                                let style =
                                    self.child_layout_style(&self.nodes[NodeId::from_u64(ibox.id)]);
                                (
                                    style.float().is_floated(),
                                    style
                                        .margin()
                                        .resolve_or_zero(inputs.parent_size, resolve_calc_value),
                                )
                            };

                            if is_floated {
                                let output = self.compute_child_layout(
                                    taffy::NodeId::from(ibox.id),
                                    subtract_margins(child_inputs, margin),
                                );
                                width = width.max(output.size.width + margin.left + margin.right);
                            }
                        }

                        width * scale
                    }
                    AvailableSpace::MaxContent => {
                        // When computing a max-content size the available width is effectively
                        // infinite, so floats never wrap onto a new "band" due to a lack of
                        // horizontal space. They only move below preceding floats when the `clear`
                        // property forces them to.
                        //
                        // Floats that share a band sit side-by-side and so their widths sum, whereas
                        // floats pushed onto a new band (via `clear`) stack vertically and so we
                        // take the maximum extent across bands rather than summing.
                        let mut left_band: f32 = 0.0;
                        let mut right_band: f32 = 0.0;
                        let mut width: f32 = 0.0;
                        for ibox in inline_layout
                            .live_layout_mut()
                            .into_iter()
                            .flat_map(|layout| layout.inline_boxes_mut())
                        {
                            let (float, clear, margin) = {
                                let style =
                                    self.child_layout_style(&self.nodes[NodeId::from_u64(ibox.id)]);
                                (
                                    style.float(),
                                    style.clear(),
                                    style
                                        .margin()
                                        .resolve_or_zero(inputs.parent_size, resolve_calc_value),
                                )
                            };

                            if float.is_floated() {
                                if matches!(clear, Clear::Left | Clear::Both) {
                                    left_band = 0.0;
                                }
                                if matches!(clear, Clear::Right | Clear::Both) {
                                    right_band = 0.0;
                                }

                                let output = self.compute_child_layout(
                                    taffy::NodeId::from(ibox.id),
                                    subtract_margins(child_inputs, margin),
                                );
                                let box_width = output.size.width + margin.left + margin.right;

                                match float {
                                    Float::Left => left_band += box_width,
                                    Float::Right => right_band += box_width,
                                    Float::None => {}
                                }
                                width = width.max(left_band + right_band);
                            }
                        }

                        width * scale
                    }
                };

                #[cfg(not(feature = "floats"))]
                let float_width = 0.0;

                let computed_width = match available_space.width {
                    AvailableSpace::MinContent => min_content_width.max(float_width),
                    AvailableSpace::MaxContent => max_content_width + float_width,
                    AvailableSpace::Definite(limit) => (limit * scale)
                        .min(max_content_width + float_width)
                        .max(min_content_width),
                }
                .ceil();

                let style_width = node_size.width.map(|w| w * scale);
                let min_width = node_min_size.width.map(|w| w * scale);
                let max_width = node_max_size.width.map(|w| w * scale);

                (style_width)
                    .unwrap_or(computed_width + pbw)
                    .max(computed_width)
                    .maybe_clamp(min_width, max_width)
                    - pbw
            });

        #[cfg(not(feature = "floats"))]
        let _ = block_ctx; // Suppress unused variable warning

        // Set block context width if this is a block context root
        #[cfg(feature = "floats")]
        let is_bfc_root = block_ctx.is_bfc_root();
        #[cfg(feature = "floats")]
        if is_bfc_root {
            block_ctx.set_width((width + pbw) / scale);
        }

        // Create sub-context to account for the inline layout's padding/border
        #[cfg(feature = "floats")]
        let outer_block_ctx = block_ctx;
        #[cfg(feature = "floats")]
        let mut block_ctx =
            outer_block_ctx.sub_context(container_pb.top, [container_pb.left, container_pb.right]);
        // block_ctx.apply_content_box_inset([container_pb.left, container_pb.right]);

        if inputs.run_mode == taffy::RunMode::ComputeSize
            && inputs.axis == RequestedAxis::Horizontal
        {
            // The box sizes and indent above no longer match the lines. An
            // evicted root's lines are in its rebuild inputs, which still hold.
            if !inline_layout.is_evicted() {
                inline_layout.replayable_break = false;
            }
            self.put_back_inline_layout(node_id, inline_layout);

            let measured_size = inputs.known_dimensions.unwrap_or(taffy::Size {
                width: width.ceil() / scale,
                // Height is ignored if RequestedAxis if Horizontal
                height: 0.0,
            });

            let clamped_size = inputs
                .known_dimensions
                .or(node_size)
                .unwrap_or(measured_size + content_box_inset.sum_axes())
                .maybe_clamp(node_min_size, node_max_size)
                .maybe_max(container_pb.sum_axes().map(Some));

            return LayoutOutput::from_outer_size(clamped_size);
        }

        let resolved_text_indent = text_indent
            .length
            .resolve(CSSPixelLength::new((width / scale).max(0.0)))
            .px();
        let indent = (resolved_text_indent * scale, indent_options);

        // An evicted root breaks from its summary when its lines are what
        // `break_all_lines(width)` gives, and is rebuilt when the summary
        // cannot decide a break exactly.
        let mut summary_lines = None;
        if inline_layout.is_evicted() {
            #[cfg(feature = "floats")]
            let clear_of_floats = slot_is_clear(
                &block_ctx.find_content_slot(0.0, Clear::None, None),
                width,
                scale,
            );
            #[cfg(not(feature = "floats"))]
            let clear_of_floats = true;
            // Boxes whose static position needs their place along the line
            // reuse the recorded one only when it is what aligning these lines
            // again gives; a measure pass places no boxes.
            let summary = inline_layout
                .evicted_summary()
                .filter(|summary| {
                    let static_x = summary.out_of_flow.as_deref().and_then(|oof| oof.static_x.as_ref());
                    !super::evict::is_vertical(&self.nodes[node_id])
                        && (summary
                            .out_of_flow_ids()
                            .iter()
                            .all(|&id| self.abspos_static_x_is_unused(id))
                            || static_x.is_some_and(|static_x| {
                                inputs.run_mode != RunMode::PerformLayout
                                    || static_x.aligned
                                        == self.inline_align_inputs(node_id, width, indent)
                            }))
                })
                .map(|summary| &summary.lines)
                .filter(|_| clear_of_floats);
            summary_lines =
                summary.and_then(|summary| summary.break_lines(width, indent.0, indent.1));
            count_inline_summary(|stats| match (summary.is_some(), summary_lines.is_some()) {
                (true, true) => stats.summarized += 1,
                (true, false) => stats.ambiguous += 1,
                (false, _) => stats.unsummarized += 1,
            });
            if summary_lines.is_none() {
                inline_layout.layout_mut(&self.nodes);
            }
        }

        if summary_lines.is_none() {
            inline_layout.live_mut().set_text_indent(indent.0, indent.1);
            // Remember the content width these lines were broken
            // at, in CSS px. A later measure pass can clobber parley, and
            // resolve re-breaks to this width to put it back.
            inline_layout.last_break_width = width / scale;
        }
        // Whether `break_all_lines(width)` reproduces these lines, which
        // eviction relies on: any float band in the way makes it false.
        #[cfg(not(feature = "floats"))]
        let replayable_break = true;
        #[cfg(feature = "floats")]
        let mut replayable_break = false;

        #[cfg(not(feature = "floats"))]
        if summary_lines.is_none() {
            inline_layout.live_mut().break_all_lines(Some(width));
        }

        // Out-of-flow candidates bubbled up from this container and its in-flow subtree.
        // These are laid out by the out-of-flow positioning pass (`compute_oof_layout`).
        let mut oof_candidates = OofCandidates::new();

        // Perform inline layout
        #[cfg(feature = "floats")]
        if summary_lines.is_none() {
            let mut breaker = inline_layout.live_mut().break_lines();
            let initial_slot = block_ctx.find_content_slot(0.0, Clear::None, None);
            let mut has_active_floats = initial_slot.segment_id.is_some();
            replayable_break = slot_is_clear(&initial_slot, width, scale);
            let state = breaker.state_mut();
            state.set_layout_max_advance(width);
            if replayable_break {
                // Break as without floats, not at the slot's rounded copy of the width.
                state.set_line_max_advance(width);
                state.set_line_x(0.0);
                state.set_line_y(0.0);
            } else {
                state.set_line_max_advance(initial_slot.width * scale);
                state.set_line_x(initial_slot.x * scale);
                state.set_line_y((initial_slot.y * scale) as f64);
            }

            // TODO: revert state and retry layout if a line doesn't fit
            //
            // Save initial state. Saved state is used to revert the layout to a previous state if needed
            // (e.g. to revert a line that doesn't fit in the space it was laid out into)
            //
            // let mut saved_state = breaker.state().clone();

            while let Some(yield_data) = breaker.break_next() {
                match yield_data {
                    YieldData::LineBreak(_line_break_data) => {
                        let state = breaker.state_mut();

                        if has_active_floats {
                            replayable_break = false;
                            // TODO: revert state and retry layout if a line doesn't fit
                            // saved_state = state.clone();

                            let min_y = state.line_y() / scale as f64;
                            let next_slot =
                                block_ctx.find_content_slot(min_y as f32, Clear::None, None);
                            has_active_floats = next_slot.segment_id.is_some();

                            state.set_line_max_advance(next_slot.width * scale);
                            state.set_line_x(next_slot.x * scale);
                            state.set_line_y((next_slot.y * scale) as f64);
                        } else {
                            state.set_line_x(0.0);
                            state.set_line_max_advance(width);
                        }

                        continue;
                    }
                    YieldData::MaxHeightExceeded(_data) => {
                        // TODO
                        continue;
                    }
                    YieldData::InlineBoxBreak(box_break_data) => {
                        replayable_break = false;
                        let state = breaker.state_mut();
                        // Not the container: naming it node_id
                        // shadowed the container and sent its cache bust here.
                        let float_id = NodeId::from_u64(box_break_data.inline_box_id);

                        // Relative insets shift the float from where it was placed.
                        let relative_inset = {
                            let container_direction =
                                self.nodes[node_id].layout_style().direction();
                            let child_style = self.child_layout_style(&self.nodes[float_id]);
                            if child_style.position() == taffy::Position::Relative {
                                let containing_block_size = Size {
                                    width: width / scale,
                                    height: known_dimensions
                                        .height
                                        .or(node_size.height)
                                        .map(|height| {
                                            (height - content_box_inset.vertical_axis_sum())
                                                .max(0.0)
                                        })
                                        .unwrap_or(0.0),
                                };
                                let inset_style = child_style.inset();
                                let left = inset_style
                                    .left
                                    .maybe_resolve(containing_block_size.width, resolve_calc_value);
                                let right = inset_style
                                    .right
                                    .maybe_resolve(containing_block_size.width, resolve_calc_value);
                                Point {
                                    x: if container_direction == Direction::Rtl {
                                        right.map(|value| -value).or(left).unwrap_or(0.0)
                                    } else {
                                        left.or(right.map(|value| -value)).unwrap_or(0.0)
                                    },
                                    y: inset_style
                                        .top
                                        .maybe_resolve(
                                            containing_block_size.height,
                                            resolve_calc_value,
                                        )
                                        .or_else(|| {
                                            inset_style
                                                .bottom
                                                .maybe_resolve(
                                                    containing_block_size.height,
                                                    resolve_calc_value,
                                                )
                                                .map(|value| -value)
                                        })
                                        .unwrap_or(0.0),
                                }
                            } else {
                                Point::ZERO
                            }
                        };

                        let (direction, clear, margin) = {
                            let style = self.child_layout_style(&self.nodes[float_id]);
                            // We can assume that the box is a float because we only set `break_on_box: true` for floats
                            let direction = match style.float() {
                                Float::Left => taffy::FloatDirection::Left,
                                Float::Right => taffy::FloatDirection::Right,
                                Float::None => unreachable!(),
                            };
                            (
                                direction,
                                style.clear(),
                                style
                                    .margin()
                                    .resolve_or_zero(inputs.parent_size, resolve_calc_value),
                            )
                        };

                        let margin_sum = margin.sum_axes();

                        let mut output = self.compute_child_layout(
                            crate::taffy_node_id(float_id),
                            float_child_inputs,
                        );
                        let min_y = state.line_y() as f32 / scale;

                        // Note: `pos` is content-box relative
                        let pos = block_ctx.place_floated_box(
                            output.size + margin_sum,
                            min_y,
                            direction,
                            clear,
                            false,
                        );
                        self.nodes[node_id].set_placed_floats(true);

                        let min_y = state.line_y() / scale as f64; //.max(pos.y as f64);
                        let next_slot =
                            block_ctx.find_content_slot(min_y as f32, Clear::None, None);
                        has_active_floats = next_slot.segment_id.is_some();

                        state.set_line_max_advance(next_slot.width * scale);
                        state.set_line_x(next_slot.x * scale);
                        state.set_line_y((next_slot.y * scale) as f64);

                        let location = taffy::Point {
                            x: pos.x + margin.left + container_pb.left + relative_inset.x,
                            y: pos.y + margin.top + container_pb.top + relative_inset.y,
                        };
                        let float_node = &mut self.nodes[float_id];
                        let mut layout = float_node.unrounded_layout();
                        layout.size = output.size;
                        layout.location = location;
                        layout.margin = margin;
                        float_node.set_unrounded_layout(&layout);

                        // Translate anchors from item-relative to container-relative
                        // coordinates and collect candidates bubbled from the float's subtree
                        if !output.oof_candidates.is_empty() {
                            output.oof_candidates.translate(location);
                            oof_candidates.append(&mut output.oof_candidates);
                        }

                        // dbg!(&layout.size);
                        // dbg!(&layout.location);

                        // Floats are out-of-flow and must not contribute to the line's height.
                        state.append_inline_box_to_line(
                            box_break_data.advance,
                            f32::NEG_INFINITY,
                            f32::NEG_INFINITY,
                        );

                        // if float.is_floated() {
                        //     println!("INLINE FLOATED BOX ({}) {:?}", ibox.id, float);
                        //     println!(
                        //         "w:{} h:{} x:{}, y:{}",
                        //         layout.size.width, layout.size.height, 0, 0
                        //     );
                        // }
                    }
                }
            }
            breaker.finish();
        }

        let clamp_lines = self.inline_clamp_lines(node_id);
        let (alignment, last_line_alignment) = self.inline_alignment(node_id);
        if let Some(lines) = &mut summary_lines {
            if let Some(max_lines) = clamp_lines {
                lines.truncate_lines(max_lines);
            }
            // The rebuild replays the committed lines, not measured ones.
            if inputs.run_mode == RunMode::PerformLayout {
                let breaking = inline_layout
                    .evicted_break_mut()
                    .expect("a summarized root is still evicted");
                breaking.max_advance = width;
                breaking.set_indent(indent);
                breaking.alignment = Some(alignment);
                breaking.last_line_alignment = last_line_alignment;
                breaking.set_clamp_lines(clamp_lines);
                inline_layout.last_break_width = width / scale;
            }
        } else {
            inline_layout.replayable_break = replayable_break;
            if let Some(max_lines) = clamp_lines {
                inline_layout.live_mut().truncate_lines(max_lines);
            }
        }

        // Propagate the height consumed by floats placed within this container to the
        // enclosing block context (so that the BFC root can contain them)
        #[cfg(feature = "floats")]
        let float_height_contribution = block_ctx.floated_content_height_contribution();
        #[cfg(feature = "floats")]
        outer_block_ctx.add_child_floated_content_height_contribution(
            container_pb.top + float_height_contribution,
        );

        if let Some(layout) = inline_layout.live_layout_mut() {
            layout.align(
                alignment,
                AlignmentOptions {
                    align_when_overflowing: false,
                    last_line_alignment,
                },
            );
        }

        let lines = match &summary_lines {
            Some(lines) => BrokenLines::Summary(lines),
            None => BrokenLines::Live(inline_layout.live()),
        };

        // Parley lays out empty text as a single strut-height line (text-editor semantics),
        // but a line box containing no text, inline boxes or other in-flow content is a
        // zero-height line box in CSS (CSS2 §9.4.2).
        let has_inline_content = if !text_is_only_bidi_controls {
            true
        } else if text_kind.empty {
            inline_layout
                .live_layout()
                .is_some_and(|layout| layout.inline_boxes().len() > 0)
        } else {
            // On a line of bidi controls an out-of-flow box (tldraw's abspos
            // canvas) is no content either, but parley sizes the line.
            inline_layout.live_layout().is_some_and(|layout| {
                layout
                    .inline_boxes()
                    .any(|inline_box| inline_box.kind == parley::InlineBoxKind::InFlow)
            })
        };

        let mut height = if has_inline_content {
            lines.height()
        } else {
            0.0
        };

        // A forced line break (e.g. `<br>` or a preserved newline) at the end of the inline
        // content ends the final line box without starting a new one. Parley still emits an
        // empty line after it (so that editors have a line to place the cursor on), so that
        // line is excluded from the measured height (and from the last baseline).
        let line_count = lines.len();
        let mut trailing_empty_line = None;
        if line_count >= 2
            && lines.break_reason(line_count - 2) == BreakReason::Explicit
            && lines.is_empty_line(line_count - 1)
        {
            height -= lines.line_height(line_count - 1);
            trailing_empty_line = Some(line_count - 1);
        }
        let lines_width = lines.width();
        let last_line_index = trailing_empty_line.unwrap_or(line_count).checked_sub(1);
        let first_line_baseline = (line_count > 0).then(|| lines.baseline(0));
        let last_line_baseline = last_line_index.map(|i| lines.baseline(i));

        #[cfg(feature = "floats")]
        {
            if is_bfc_root {
                height = height.max(float_height_contribution * scale)
            };
        }

        // Note: `width` and `height` are content-box measurements of the inline content.
        // `known_dimensions` must not be substituted in here: those are border-box sizes, and
        // using them for the scrollable overflow rect (which adds padding below) would
        // double-count padding, incorrectly making the container's content overflow it.
        let measured_size = taffy::Size {
            width: width / scale,
            height: height / scale,
        };

        // Unbreakable content (e.g. `white-space: pre` text) may overflow the max advance the
        // lines were broken at. `width` is the advance the lines were broken at, so the actual
        // extent of the line boxes must be taken from the laid-out lines for `content_size`.
        let content_width = f32_max(width, lines_width) / scale;

        let clamped_size = inputs
            .known_dimensions
            .or(node_size)
            .unwrap_or(measured_size + content_box_inset.sum_axes())
            .maybe_clamp(node_min_size, node_max_size);
        let final_size = Size {
            width: clamped_size.width,
            height: f32_max(
                clamped_size.height,
                aspect_ratio
                    .map(|ratio| clamped_size.width / ratio)
                    .unwrap_or(0.0),
            ),
        }
        .maybe_max(container_pb.sum_axes().map(Some));

        let container_direction = self.nodes[node_id].layout_style().direction();

        // `align-content` aligns the line boxes (as a single unit) within the content box in
        // the block axis. Floats are positioned relative to the formatting context and are not
        // moved. The offset is stored on the text layout for painting and hit-testing.
        let align_content = BlockContainerStyle::align_content(&self.nodes[node_id].layout_style());
        let block_offset = if align_content.keyword() == taffy::AlignContentKeyword::Normal {
            0.0
        } else {
            let free_space =
                final_size.height - content_box_inset.vertical_axis_sum() - measured_size.height;
            taffy::compute_block_align_content_offset(align_content, free_space)
        };
        inline_layout.block_offset = block_offset;
        let line_box_top = container_pb.top + block_offset;

        // Store sizes and positions of inline boxes
        let mut ibox_order: u32 = 0;
        let mut span_cb_flags = StyleFlags::empty();
        // A measure pass must not commit geometry. It runs at
        // max-content, so writing positions here parks an inline box off
        // the right edge of the narrower box the container committed to,
        // and it clips away.
        let commit_positions = inputs.run_mode == taffy::RunMode::PerformLayout;
        // A summarized root's inline boxes are all absolutely positioned, with
        // static positions that need only their line, or recorded offsets
        // along it that the summary check found current (see eviction).
        if let (true, Some(lines), Some(summary)) = (
            commit_positions,
            &summary_lines,
            inline_layout.evicted_summary(),
        ) {
            let content_inline = taffy::Line {
                start: container_pb.left,
                end: final_size.width - container_pb.right,
            };
            let static_x = summary.out_of_flow.as_deref().and_then(|oof| oof.static_x.as_ref());
            let ids = summary.out_of_flow_ids();
            for (index, (&box_id, &line)) in ids.iter().zip(lines.box_lines()).enumerate() {
                // Clamped away: the missing-box pass below offers it.
                let Some(line) = lines.lines().get(line as usize) else {
                    continue;
                };
                span_cb_flags |= self.inline_span_cb_flags(node_id, box_id);
                oof_candidates.push(self.abspos_inline_candidate(
                    box_id,
                    ibox_order,
                    AbsposLine {
                        // Without recorded offsets every box has a horizontal
                        // inset or is block-level, so this is unused.
                        x: static_x.map_or(container_pb.left, |static_x| {
                            (static_x.x[index] / scale) + container_pb.left
                        }),
                        top: (line.block_min_coord / scale) + line_box_top,
                        bottom: (line.block_max_coord / scale) + line_box_top,
                    },
                    content_inline,
                    container_direction,
                ));
                ibox_order += 1;
            }
        }
        for line in inline_layout
            .live_layout()
            .into_iter()
            .flat_map(|layout| layout.lines())
        {
            for item in line.items() {
                if !commit_positions {
                    break;
                }
                if let parley::layout::PositionedLayoutItem::InlineBox(ibox) = item {
                    let order = ibox_order;
                    ibox_order += 1;
                    if inputs.run_mode == RunMode::PerformLayout {
                        span_cb_flags |=
                            self.inline_span_cb_flags(node_id, NodeId::from_u64(ibox.id));
                    }
                    let node = &self.nodes[NodeId::from_u64(ibox.id)];
                    let style = self.child_layout_style(node);
                    let padding = style
                        .padding()
                        .resolve_or_zero(child_inputs.parent_size, resolve_calc_value);
                    let border = style
                        .border()
                        .resolve_or_zero(child_inputs.parent_size, resolve_calc_value);
                    let margin = style
                        .margin()
                        .resolve_or_zero(child_inputs.parent_size, resolve_calc_value);

                    #[cfg(feature = "floats")]
                    let is_floated = style.float() != Float::None;
                    #[cfg(not(feature = "floats"))]
                    let is_floated = false;

                    let position = style.position();
                    let is_absolute = position.is_out_of_flow();

                    // Resolve relative inset offsets against the containing block
                    // (the content box of the inline container).
                    let containing_block_size = Size {
                        width: (final_size - content_box_inset.sum_axes()).width,
                        height: known_dimensions
                            .height
                            .or(node_size.height)
                            .map(|height| (height - content_box_inset.vertical_axis_sum()).max(0.0))
                            .unwrap_or(0.0),
                    };
                    let inset_style = style.inset();
                    let inset = taffy::Rect {
                        left: inset_style
                            .left
                            .maybe_resolve(containing_block_size.width, resolve_calc_value),
                        right: inset_style
                            .right
                            .maybe_resolve(containing_block_size.width, resolve_calc_value),
                        top: inset_style
                            .top
                            .maybe_resolve(containing_block_size.height, resolve_calc_value),
                        bottom: inset_style
                            .bottom
                            .maybe_resolve(containing_block_size.height, resolve_calc_value),
                    };
                    let box_inputs = inline_box_inputs(style.size().width, style.size().height, margin, child_inputs);
                    drop(style);

                    if is_absolute {
                        let line_metrics = line.metrics();
                        oof_candidates.push(self.abspos_inline_candidate(
                            NodeId::from_u64(ibox.id),
                            order,
                            AbsposLine {
                                x: (ibox.x / scale) + container_pb.left,
                                top: (line_metrics.block_min_coord / scale) + line_box_top,
                                bottom: (line_metrics.block_max_coord / scale) + line_box_top,
                            },
                            taffy::Line {
                                start: container_pb.left,
                                end: final_size.width - container_pb.right,
                            },
                            container_direction,
                        ));
                    } else if is_floated {
                        let float_node = &mut self.nodes[NodeId::from_u64(ibox.id)];
                        let mut layout = float_node.unrounded_layout();
                        layout.padding = padding; //.map(|p| p / scale);
                        layout.border = border; //.map(|p| p / scale);
                        layout.margin = margin;
                        float_node.set_unrounded_layout(&layout);
                    } else {
                        // Re-measure the box to get its border-box size (this hits the layout
                        // cache). The size cannot be recovered from `ibox` dimensions as the
                        // space reserved in the line is clamped to be non-negative.
                        let mut output =
                            self.compute_child_layout(taffy::NodeId::from(ibox.id), box_inputs);
                        let size = output.size;
                        let node = &mut self.nodes[NodeId::from_u64(ibox.id)];

                        let is_relative = position == taffy::Position::Relative;
                        let inset_offset = if is_relative {
                            taffy::Point {
                                x: if container_direction == Direction::Rtl {
                                    inset.right.map(|x| -x).or(inset.left).unwrap_or(0.0)
                                } else {
                                    inset.left.or(inset.right.map(|x| -x)).unwrap_or(0.0)
                                },
                                y: inset.top.or(inset.bottom.map(|x| -x)).unwrap_or(0.0),
                            }
                        } else {
                            taffy::Point::ZERO
                        };

                        let mut layout = node.unrounded_layout();
                        layout.size = size;
                        layout.scrollable_overflow_rect = output.scrollable_overflow_rect;
                        layout.location.x =
                            (ibox.x / scale) + margin.left + container_pb.left + inset_offset.x;
                        // Parley positions the margin box; offset to the border box even
                        // when a negative top margin makes it extend above the margin box.
                        layout.location.y =
                            (ibox.y / scale) + margin.top + line_box_top + inset_offset.y;
                        layout.padding = padding; //.map(|p| p / scale);
                        layout.border = border; //.map(|p| p / scale);
                        layout.margin = margin;
                        node.set_unrounded_layout(&layout);

                        // Translate anchors from item-relative to container-relative
                        // coordinates and collect candidates bubbled from the box's subtree
                        if !output.oof_candidates.is_empty() {
                            let location = layout.location;
                            output.oof_candidates.translate(location);
                            oof_candidates.append(&mut output.oof_candidates);
                        }
                    }
                }
            }
        }

        // Post-pass for abs-pos children parley left
        // unprocessed. When the IFC root has no in-flow content
        // (only `position: absolute` descendants), parley's line
        // breaker produces zero lines, so the per-line iteration
        // above sees nothing and the abs-pos children are never
        // offered to the out-of-flow pass. Worse, when the IFC root
        // has `height: 0` styling (a common responsive-image
        // placeholder pattern, e.g. BBC's `.ssrcss-9ozhtx`: `height: 0;
        // padding-top: 56.25%`), parley's `build_inline_layout_into`
        // may not even populate `inline_boxes()` for the abs-pos
        // descendant, so iterating parley's list isn't reliable.
        //
        // We iterate the IFC root's `layout_children` instead.
        // That list is populated by `find_inline_layout_embedded_boxes`
        // during construct and contains the flattened atomic-inline
        // descendants (img/svg/input/etc.) regardless of whether
        // parley managed to lay them out. Any out-of-flow child that
        // did not get a candidate above is offered to the out-of-flow
        // pass with its static position at the start of the content box.
        let missing_oof: Vec<NodeId> = {
            let lc = self.nodes[node_id].layout_children();
            lc.as_deref()
                .map(|children| {
                    children
                        .iter()
                        .copied()
                        .filter(|&cid| {
                            self.nodes[cid].taffy_position().is_out_of_flow()
                                && !oof_candidates
                                    .iter()
                                    .any(|c| c.node == crate::taffy_node_id(cid))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        for cid in missing_oof {
            let inline_start = if container_direction.is_rtl() {
                final_size.width - container_pb.right
            } else {
                container_pb.left
            };
            oof_candidates.push(OofCandidate {
                node: crate::taffy_node_id(cid),
                order: ibox_order,
                position: self.nodes[cid].taffy_position(),
                static_position: taffy::Point {
                    x: AxisStaticPosition::from_edge(
                        inline_start,
                        if container_direction.is_rtl() {
                            AxisStaticEdge::End
                        } else {
                            AxisStaticEdge::Start
                        },
                    ),
                    y: AxisStaticPosition::from_edge(line_box_top, AxisStaticEdge::Start),
                },
            });
            ibox_order += 1;
        }

        // println!("INLINE LAYOUT FOR {:?}. max_advance: {:?}", node_id, max_advance);
        // dbg!(&inline_layout.text);
        // println!("Computed: w: {} h: {}", inline_layout.live().width(), inline_layout.live().height());
        // println!("known_dimensions: w: {:?} h: {:?}", inputs.known_dimensions.width, inputs.known_dimensions.height);
        // println!("\n");

        let line_baseline = |baseline: f32| (baseline / scale) + line_box_top;
        let exports_baseline = has_inline_content && !suppresses_baseline;
        let first_baseline = exports_baseline
            .then_some(first_line_baseline)
            .flatten()
            .map(line_baseline);
        let last_baseline = exports_baseline
            .then_some(last_line_baseline)
            .flatten()
            .map(line_baseline);

        if inputs.run_mode == RunMode::PerformLayout {
            inline_layout.span_cb_flags = span_cb_flags;
        }

        self.put_back_inline_layout(node_id, inline_layout);
        // A measure that broke lines shapes the root as a final layout does:
        // a flex item measures its whole subtree before laying any of it out.
        self.evict_inline_layout_after_break(node_id);

        let oof_position_inset = taffy::Rect {
            left: border.left,
            right: border.right + scrollbar_gutter.x,
            top: border.top,
            bottom: border.bottom + scrollbar_gutter.y,
        };

        LayoutOutput {
            size: final_size,
            scrollable_overflow_rect: {
                let content_extent = taffy::Size {
                    width: content_width,
                    height: measured_size.height,
                } + padding.sum_axes();
                taffy::Rect {
                    left: 0.0,
                    right: content_extent.width,
                    top: 0.0,
                    bottom: content_extent.height + block_offset.max(0.0),
                }
            },
            baselines: taffy::Baselines {
                first: first_baseline,
                last: last_baseline,
            },
            top_margin: CollapsibleMarginSet::ZERO,
            bottom_margin: CollapsibleMarginSet::ZERO,
            margins_can_collapse_through: !has_styles_preventing_being_collapsed_through
                && final_size.height == 0.0
                && measured_size.height == 0.0,
            oof_candidates,
            oof_positioning_area: Some(OofPositioningArea {
                size: final_size - oof_position_inset.sum_axes(),
                offset: Point {
                    x: oof_position_inset.left,
                    y: oof_position_inset.top,
                },
            }),
        }
    }
}

/// Relative difference between a content slot's width and the root's content
/// width that is float rounding, not a float.
#[cfg(feature = "floats")]
const SLOT_ROUNDING: f32 = 1e-5;

/// Whether `slot`, the first content slot of an inline root with content
/// width `width` (in layout units), has no float beside it. The block context
/// derives the slot's edges from its own insets, so they can differ from the
/// root's content box by rounding.
#[cfg(feature = "floats")]
fn slot_is_clear(slot: &taffy::ContentSlot, width: f32, scale: f32) -> bool {
    let tolerance = SLOT_ROUNDING * width.abs().max(1.0);
    slot.segment_id.is_none()
        && slot.y == 0.0
        && (slot.x * scale).abs() <= tolerance
        && (slot.width * scale - width).abs() <= tolerance
}

/// Where an absolutely positioned inline box sits in its container, in CSS px:
/// its inline position on the line and the line box's block extent.
struct AbsposLine {
    x: f32,
    top: f32,
    bottom: f32,
}

/// The broken lines inline layout sizes a root from: the shaped layout's,
/// or those its break summary computed while it is evicted.
enum BrokenLines<'a> {
    Live(&'a parley::Layout<TextBrush>),
    Summary(&'a parley::SummaryLines),
}

impl BrokenLines<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Live(layout) => layout.len(),
            Self::Summary(lines) => lines.lines().len(),
        }
    }

    fn width(&self) -> f32 {
        match self {
            Self::Live(layout) => layout.width(),
            Self::Summary(lines) => lines.width(),
        }
    }

    fn height(&self) -> f32 {
        match self {
            Self::Live(layout) => layout.height(),
            Self::Summary(lines) => lines.height(),
        }
    }

    fn break_reason(&self, line: usize) -> BreakReason {
        match self {
            Self::Live(layout) => layout.get(line).expect("line in range").break_reason(),
            Self::Summary(lines) => lines.lines()[line].break_reason,
        }
    }

    fn line_height(&self, line: usize) -> f32 {
        match self {
            Self::Live(layout) => layout.get(line).expect("line in range").metrics().line_height,
            Self::Summary(lines) => lines.lines()[line].line_height,
        }
    }

    fn baseline(&self, line: usize) -> f32 {
        match self {
            Self::Live(layout) => layout.get(line).expect("line in range").metrics().baseline,
            Self::Summary(lines) => lines.lines()[line].baseline,
        }
    }

    /// Whether `line` has no text and nothing to paint, like the line parley
    /// adds after a final forced break.
    fn is_empty_line(&self, line: usize) -> bool {
        match self {
            Self::Live(layout) => {
                let line = layout.get(line).expect("line in range");
                line.text_range().is_empty() && line.items().next().is_none()
            }
            Self::Summary(lines) => lines.lines()[line].is_trailing_empty,
        }
    }
}

#[inline(always)]
fn f32_max(a: f32, b: f32) -> f32 {
    a.max(b)
}

impl crate::BaseDocument {
    /// Puts an inline root's layout back after a layout pass and indexes the root once. Each
    /// measure and layout of a root used to push it again, so the index of a large page grew to
    /// several times its root count before resolve deduplicated it.
    pub(crate) fn put_back_inline_layout(&mut self, node_id: NodeId, mut layout: Box<TextLayout>) {
        let push = !layout.mark_root_indexed();
        self.nodes[node_id]
            .data
            .downcast_element_mut()
            .unwrap()
            .set_inline_layout_data(Some(layout));
        if push {
            self.inline_roots.push(node_id);
        }
        self.inline_roots_put = true;
    }

    // Re-break a container's text to a width it already
    // committed to, without re-running layout. Repairing at the parley
    // level is deliberate: replaying `compute_inline_layout` would need
    // a block context that is gone by now, and passing `None` builds a
    // fresh BFC whose empty float bands relocate every float to the top.
    pub(crate) fn rebreak_inline_to_width(&mut self, node_id: NodeId, css_width: f32) {
        let scale = self.viewport.scale();
        let clamp_lines = self.inline_clamp_lines(node_id);
        let (alignment, last_line_alignment) = self.inline_alignment(node_id);
        let Some(mut inline_layout) = self.nodes[node_id]
            .data
            .downcast_element_mut()
            .and_then(|el| el.take_inline_layout())
        else {
            return;
        };
        // An evicted root needs no lines until it is read: its rebuild breaks
        // at the new width.
        if let Some(breaking) = inline_layout.evicted_break_mut() {
            breaking.max_advance = css_width * scale;
            breaking.alignment = Some(alignment);
            breaking.last_line_alignment = last_line_alignment;
            breaking.set_clamp_lines(clamp_lines);
            inline_layout.last_break_width = css_width;
            inline_layout.replayable_break = true;
            self.nodes[node_id]
                .element_data_mut()
                .expect("took the layout from this element")
                .set_inline_layout_data(Some(inline_layout));
            return;
        }
        let layout = inline_layout.layout_mut(&self.nodes);
        layout.break_all_lines(Some(css_width * scale));
        if let Some(max_lines) = clamp_lines {
            layout.truncate_lines(max_lines);
        }
        layout.align(
            alignment,
            AlignmentOptions {
                align_when_overflowing: false,
                last_line_alignment,
            },
        );
        inline_layout.last_break_width = css_width;
        inline_layout.replayable_break = true;
        self.nodes[node_id]
            .element_data_mut()
            .expect("took the layout from this element")
            .set_inline_layout_data(Some(inline_layout));
    }

    // -webkit-box + orient:vertical + line-clamp:N was rewritten to
    // flow-root at computed-value time; the stylo flag says clamp here.
    /// The [`AlignInputs`] a pass breaking `node_id` at `width` (layout
    /// units) with `indent` aligns its lines with.
    fn inline_align_inputs(
        &self,
        node_id: NodeId,
        width: f32,
        indent: (f32, parley::IndentOptions),
    ) -> crate::node::AlignInputs {
        let (alignment, last_line_alignment) = self.inline_alignment(node_id);
        crate::node::AlignInputs {
            max_advance: width,
            indent,
            alignment: Some(alignment),
            last_line_alignment,
            clamp_lines: self.inline_clamp_lines(node_id),
        }
    }

    pub(crate) fn inline_clamp_lines(&self, node_id: NodeId) -> Option<usize> {
        self.nodes[node_id].primary_styles().and_then(|s| {
            use style::computed_value_flags::ComputedValueFlags;
            if !s.flags.contains(ComputedValueFlags::IS_WEBKIT_LINE_CLAMP_CONTEXT) {
                return None;
            }
            let clamp = s.get_box().clone_line_clamp();
            clamp.max_lines.lines_value().map(|lines| lines.0.max(1) as usize)
        })
    }

    pub(crate) fn inline_alignment(
        &self,
        node_id: NodeId,
    ) -> (parley::layout::Alignment, Option<parley::layout::Alignment>) {
        self.nodes[node_id]
            .primary_styles()
            .map(|s| {
                (
                    stylo_to_parley::text_align(s.clone_text_align()),
                    stylo_to_parley::text_align_last(s.clone_text_align_last()),
                )
            })
            .unwrap_or((parley::layout::Alignment::Start, None))
    }
}
