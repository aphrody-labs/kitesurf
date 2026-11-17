//! Inline-span box-decoration painting.
//!
//! Reads `parley::Layout::span_fragments()` (from the patched
//! parley) to produce per-line rectangles for
//! every inline element with non-default CSS box decorations
//! (padding / border / background / border-radius). Without this
//! pass, blitz only renders glyphs for inline-flow elements like
//! `<a>` and `<span>`; the box decorations from the cascade exist
//! but never make it onto the screen.
//!
//! Two entry points:
//! - [`paint_span_fragment_backgrounds`] runs before glyphs are
//!   stroked, so the background fills sit under the text.
//! - [`paint_span_fragment_borders`] runs after glyphs are
//!   stroked, so the border edges paint over any glyph that
//!   crosses them (matches CSS spec: borders are above content
//!   in the inline-fragment paint order).
//!
//! Limitations (matching parley's fragment emission):
//! - Single-line spans only. Spans that wrap to a new line are
//!   not painted at all (they fall back to plain glyph
//!   rendering).
//! - Border-radius is applied as a clip on the background fill;
//!   borders themselves are square strokes. For symmetric
//!   single-color borders + matching radius this looks correct;
//!   for thick asymmetric or multi-color borders the corner
//!   geometry will be slightly off vs. a real browser.
//! - Single-color borders are emitted by reading any of the four
//!   `border-*-color` computed values from the styled node and
//!   stroking each edge independently. Mixed colors per edge
//!   work; mitered joins between adjacent differently-colored
//!   edges are not implemented.

use anyrender::PaintScene;
use blitz_dom::{BaseDocument, node::TextBrush};
use kurbo::{Affine, Rect, RoundedRect, RoundedRectRadii};
use parley::layout::Layout;
use peniko::{Fill, Mix};

use crate::color::ToColorColor;

/// Paint background fills for every single-line inline-span
/// fragment whose style has a non-transparent
/// `background-color`. Call before [`crate::text::stroke_text`]
/// so glyphs render on top of the fill.
pub(crate) fn paint_span_fragment_backgrounds(
    scene: &mut impl PaintScene,
    layout: &Layout<TextBrush>,
    doc: &BaseDocument,
    transform: Affine,
    inline_root_id: blitz_dom::NodeId,
) {
    let fragments = layout.span_fragments();
    if fragments.is_empty() {
        return;
    }
    let styles = layout.styles();
    // Materialize once to index by fragment.line_index.
    // Line is Copy and small, so this is cheap.
    let lines: Vec<parley::layout::Line<'_, TextBrush>> = layout.lines().collect();

    for fragment in fragments {
        let style = match styles.get(fragment.style_index as usize) {
            Some(s) => s,
            None => continue,
        };
        let decos = &style.box_decorations;
        if decos.is_empty() {
            // Defensive: if a fragment was emitted with empty
            // decorations, there's nothing to paint. The line
            // breaker only emits fragments when at least one of
            // padding/border crossed the threshold, so the trail
            // path can't have produced one, but the type system
            // doesn't know that.
            continue;
        }

        let line = match lines.get(fragment.line_index as usize) {
            Some(l) => l,
            None => continue,
        };
        let _ = line.metrics();

        // Look up the styled node so we can resolve the actual
        // CSS background-color from the cascade (the brush
        // carries only a node id; per-element colors live on
        // the node).
        let node = match doc.get_node(style.brush.id) {
            Some(n) => n,
            None => continue,
        };
        let primary = match node.primary_styles() {
            Some(p) if crate::text::is_visible(&p) => p,
            _ => continue,
        };
        let current_color = primary.clone_color();
        let bg = primary
            .get_background()
            .background_color
            .resolve_to_absolute(&current_color)
            .as_srgb_color()
            .multiply_alpha(crate::text::inline_opacity(doc, node.id, inline_root_id));
        if bg.components[3] == 0.0 {
            continue;
        }

        let rect = fragment_rect(fragment, line);
        if decos.border_radius.is_zero() {
            scene.fill(Fill::NonZero, transform, bg, None, &rect);
        } else {
            let radii = fragment_radii(decos, fragment);
            let rounded = RoundedRect::from_rect(rect, radii);
            scene.fill(Fill::NonZero, transform, bg, None, &rounded);
        }
    }
}

/// Paint border edges for every single-line inline-span fragment
/// whose style has any `border-*-width > 0` and a non-transparent
/// matching color. Call after [`crate::text::stroke_text`] so the
/// borders render over glyphs that overlap the edge band.
pub(crate) fn paint_span_fragment_borders(
    scene: &mut impl PaintScene,
    layout: &Layout<TextBrush>,
    doc: &BaseDocument,
    transform: Affine,
    inline_root_id: blitz_dom::NodeId,
) {
    let fragments = layout.span_fragments();
    if fragments.is_empty() {
        return;
    }
    let styles = layout.styles();
    let mut lines: Option<Vec<parley::layout::Line<'_, TextBrush>>> = None;

    for fragment in fragments {
        let style = match styles.get(fragment.style_index as usize) {
            Some(s) => s,
            None => continue,
        };
        let decos = &style.box_decorations;
        let top_w = decos.border_block_start_width as f64;
        let right_w = decos.border_inline_end_width as f64;
        let bottom_w = decos.border_block_end_width as f64;
        let left_w = decos.border_inline_start_width as f64;
        if !(top_w > 0.0 || right_w > 0.0 || bottom_w > 0.0 || left_w > 0.0) {
            continue;
        }

        let lines = lines.get_or_insert_with(|| layout.lines().collect());
        let line = match lines.get(fragment.line_index as usize) {
            Some(l) => l,
            None => continue,
        };

        let node = match doc.get_node(style.brush.id) {
            Some(n) => n,
            None => continue,
        };
        let primary = match node.primary_styles() {
            Some(p) if crate::text::is_visible(&p) => p,
            _ => continue,
        };
        let current_color = primary.clone_color();
        let opacity = crate::text::inline_opacity(doc, node.id, inline_root_id);
        let border = primary.get_border();
        let top_color = border
            .border_top_color
            .resolve_to_absolute(&current_color)
            .as_srgb_color()
            .multiply_alpha(opacity);
        let right_color = border
            .border_right_color
            .resolve_to_absolute(&current_color)
            .as_srgb_color()
            .multiply_alpha(opacity);
        let bottom_color = border
            .border_bottom_color
            .resolve_to_absolute(&current_color)
            .as_srgb_color()
            .multiply_alpha(opacity);
        let left_color = border
            .border_left_color
            .resolve_to_absolute(&current_color)
            .as_srgb_color()
            .multiply_alpha(opacity);

        let outer = fragment_rect(fragment, line);

        // Borders are filled rectangles, one per edge, drawn
        // inward from the outer rectangle by `border_*_width`.
        // Top and bottom span the full width; left and right
        // depend on is_first/is_last_fragment so a wrapped span
        // doesn't paint internal vertical edges. Only single-line
        // spans emit fragments today, so both flags are true.
        let push_clip = !decos.border_radius.is_zero();
        let radii = if push_clip {
            Some(fragment_radii(decos, fragment))
        } else {
            None
        };
        if let Some(r) = radii {
            // Clip subsequent border strokes to the rounded
            // outer rect so corners don't paint outside the
            // radius.
            let rounded = RoundedRect::from_rect(outer, r);
            scene.push_layer(
                Fill::NonZero,
                Mix::Normal,
                1.0,
                transform,
                &rounded,
                None,
                None,
            );
        }

        if top_w > 0.0 && top_color.components[3] > 0.0 {
            let r = Rect::new(outer.x0, outer.y0, outer.x1, outer.y0 + top_w);
            scene.fill(Fill::NonZero, transform, top_color, None, &r);
        }
        if bottom_w > 0.0 && bottom_color.components[3] > 0.0 {
            let r = Rect::new(outer.x0, outer.y1 - bottom_w, outer.x1, outer.y1);
            scene.fill(Fill::NonZero, transform, bottom_color, None, &r);
        }
        if fragment.is_first_fragment && left_w > 0.0 && left_color.components[3] > 0.0 {
            let r = Rect::new(outer.x0, outer.y0, outer.x0 + left_w, outer.y1);
            scene.fill(Fill::NonZero, transform, left_color, None, &r);
        }
        if fragment.is_last_fragment && right_w > 0.0 && right_color.components[3] > 0.0 {
            let r = Rect::new(outer.x1 - right_w, outer.y0, outer.x1, outer.y1);
            scene.fill(Fill::NonZero, transform, right_color, None, &r);
        }

        if push_clip {
            scene.pop_layer();
        }
    }
}

/// Builds the outer rectangle of a fragment in the painter's
/// pre-transform coordinate space. The line's `inline_min_coord`
/// gives the line's left edge; `block_min_coord` /
/// `block_max_coord` give the line's vertical extents (these
/// already include leading and have been adjusted by the
/// breaker). The fragment's `x_start` / `x_end` are
/// line-relative, so the line's left edge is added.
///
/// Alignment: parley's `Layout::align()` shifts each line's
/// content by writing `metrics.offset`; that offset is what the
/// glyph iterator adds when emitting positioned glyphs
/// (`GlyphRunIter::next` composes `inline_min_coord + offset`).
/// Span fragments are computed before align (during
/// `BreakLines::Drop`), so `x_start`/`x_end` are line-relative and
/// pre-alignment. Adding `metrics.offset` here makes a centered or
/// right-aligned line draw the box decoration at the same x as the
/// glyphs.
fn fragment_rect(
    fragment: &parley::layout::SpanFragment,
    line: &parley::layout::Line<'_, TextBrush>,
) -> Rect {
    let metrics = line.metrics();
    let x_origin = metrics.inline_min_coord + metrics.offset;
    let x0 = (x_origin + fragment.x_start) as f64;
    let x1 = (x_origin + fragment.x_end) as f64;
    let y0 = metrics.block_min_coord as f64;
    let y1 = metrics.block_max_coord as f64;
    Rect::new(x0, y0, x1, y1)
}

/// Builds non-uniform radii for the rounded outer rect. Only
/// single-line spans emit fragments, so all four corners belong
/// to the same fragment and the BoxDecorations radii are copied
/// directly. Multi-line slicing would need to suppress
/// interior-line corners here.
fn fragment_radii(
    decos: &parley::style::BoxDecorations<TextBrush>,
    _fragment: &parley::layout::SpanFragment,
) -> RoundedRectRadii {
    // kurbo's RoundedRectRadii uses a single radius per corner
    // (assumes circular). CSS supports per-corner X/Y, hence
    // BoxDecorations carries 8 values. Average X and Y per
    // corner: most stylesheets use uniform radii, so this is an
    // exact match, and elliptical pills render visually close.
    let avg = |x: f32, y: f32| ((x + y) * 0.5) as f64;
    RoundedRectRadii {
        top_left: avg(
            decos.border_radius.top_left_x,
            decos.border_radius.top_left_y,
        ),
        top_right: avg(
            decos.border_radius.top_right_x,
            decos.border_radius.top_right_y,
        ),
        bottom_right: avg(
            decos.border_radius.bottom_right_x,
            decos.border_radius.bottom_right_y,
        ),
        bottom_left: avg(
            decos.border_radius.bottom_left_x,
            decos.border_radius.bottom_left_y,
        ),
    }
}
