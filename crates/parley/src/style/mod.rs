// Copyright 2021 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Rich styling support.

mod brush;
mod font;
mod styleset;

use alloc::borrow::Cow;

#[cfg(feature = "libm")]
#[allow(unused_imports)]
use core_maths::CoreFloat;
use parley_engine::FontMetrics;

pub use brush::*;
pub use font::{
    FontFamily, FontFamilyName, FontFeature, FontFeatures, FontStyle, FontVariation,
    FontVariations, FontWeight, FontWidth, GenericFamily,
};
pub use fontique::Language;
pub use parlance::{LineBreak, OverflowWrap, TextWrapMode, WordBreak};
pub use styleset::StyleSet;

use crate::util::nearly_eq;

/// CSS `tab-size`: how far a horizontal tab advances the pen.
///
/// `Chars(n)` is the CSS `<number>` form: the tab advance is `n` times the
/// advance of the space glyph, and the pen advances to the next multiple of
/// that distance from the line start. `Px(l)` is the CSS `<length>` form: the
/// tab advance is exactly `l`. Either way the pen jumps to the next tab stop,
/// mirroring how Chromium lays out `white-space: pre` text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TabSize {
    /// Number of space-advances per tab (CSS initial is 8).
    Chars(f32),
    /// Absolute advance per tab, in the layout's coordinate units.
    Px(f32),
}

impl Default for TabSize {
    fn default() -> Self {
        Self::Chars(8.0)
    }
}

impl TabSize {
    /// The distance between tab stops for a run whose space glyph advances
    /// `space_advance`. For `Chars` this is `n × space_advance`; for `Px` the
    /// absolute value itself.
    pub(crate) fn stop_spacing(self, space_advance: f32) -> f32 {
        match self {
            Self::Chars(n) => n * space_advance,
            Self::Px(l) => l,
        }
    }

    /// The advance of a tab whose pen sits at `pen` from the line start.
    /// Chromium gives every tab at least half a space of visible width,
    /// advancing an extra stop when the next one is closer than that. A pen
    /// exactly on a stop therefore advances a full stop.
    pub(crate) fn stop_advance(self, pen: f32, space_advance: f32) -> f32 {
        let spacing = self.stop_spacing(space_advance);
        if spacing > 0.0 {
            let distance = spacing - (pen % spacing);
            if distance < space_advance / 2.0 {
                distance + spacing
            } else {
                distance
            }
        } else {
            0.0
        }
    }
}

/// Whitespace collapsing and hanging behavior.
///
/// Note: Currently only the [`TreeBuilder`](crate::TreeBuilder) collapses white space. The
/// [`RangedBuilder`](crate::RangedBuilder) and [`StyleRunBuilder`](crate::StyleRunBuilder)
/// builders expect already-transformed text and use this property only to influence whitespace hanging behaviour.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum WhiteSpaceCollapse {
    /// Collapse whitespace, including ASCII segment breaks, to a space.
    Collapse,
    /// Preserve spaces, tabs, and segment breaks.
    #[default]
    Preserve,
    /// Collapse spaces and tabs while preserving segment breaks.
    PreserveBreaks,
    /// Preserve whitespace without hanging, allowing wrapping after each space or tab.
    BreakSpaces,
}

/// The height that this text takes up. The default is `MetricsRelative(1.0)`, which is the given
/// font's preferred line height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LineHeight {
    /// The line's height is a multiple of the "line height" defined by the font's metrics--the sum
    /// of the ascender height, descender height, and line gap / leading.
    MetricsRelative(f32),
    /// Line height specified as a multiple of the font size. This is how the CSS `line-height`
    /// property behaves if given a unitless number. Useful if you're using system-defined generic
    /// font families and want the line heights to be consistent across platforms.
    FontSizeRelative(f32),
    /// Line height specified in absolute units. This can be useful for ensuring all lines are
    /// spaced a whole number of pixels apart, or fitting lines into a given layout container
    /// height.
    Absolute(f32),
}

impl Default for LineHeight {
    fn default() -> Self {
        Self::NORMAL
    }
}

impl LineHeight {
    /// The recommended `LineHeight` for CSS's `line-height: normal`.
    pub const NORMAL: Self = Self::MetricsRelative(1.0);

    pub(crate) fn nearly_eq(self, other: Self) -> bool {
        match (self, other) {
            (Self::MetricsRelative(a), Self::MetricsRelative(b))
            | (Self::FontSizeRelative(a), Self::FontSizeRelative(b))
            | (Self::Absolute(a), Self::Absolute(b)) => nearly_eq(a, b),
            _ => false,
        }
    }

    pub(crate) fn scale(self, scale: f32) -> Self {
        match self {
            Self::Absolute(value) => Self::Absolute(value * scale),
            // The other variants are relative to the font size, so scaling here needn't do anything
            value => value,
        }
    }

    /// The line height in layout units for text set in a font with `metrics` at `font_size`.
    ///
    /// With `quantize`, the font's ascent, descent and leading are each rounded to whole pixels
    /// before being summed for [`MetricsRelative`](Self::MetricsRelative), as browsers do for
    /// `line-height: normal`, so that such lines are a whole number of pixels tall.
    pub(crate) fn resolve(self, font_size: f32, metrics: &FontMetrics, quantize: bool) -> f32 {
        match self {
            Self::Absolute(value) => value,
            Self::FontSizeRelative(value) => value * font_size,
            Self::MetricsRelative(value) if quantize => {
                (metrics.ascent.round() + metrics.descent.round() + metrics.leading.round()) * value
            }
            Self::MetricsRelative(value) => {
                (metrics.ascent + metrics.descent + metrics.leading) * value
            }
        }
    }
}

/// Which reference of a span (or [`InlineBox`]) is aligned with the same reference of its
/// *parent* span (the enclosing style span, or the root style for top-level content). Mirrors the
/// CSS `alignment-baseline` property.
///
/// [`InlineBox`]: crate::InlineBox
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlignmentBaseline {
    /// Align the baseline of the box with the baseline of the parent span.
    #[default]
    Baseline,
    /// Align the top of the box with the top of the parent's content area (font ascent).
    TextTop,
    /// Align the bottom of the box with the bottom of the parent's content area (font descent).
    TextBottom,
    /// Align the vertical midpoint of the box with the parent span's baseline plus half the
    /// x-height of the parent.
    Middle,
}

/// A shift applied to a span (or [`InlineBox`]) after [`AlignmentBaseline`] alignment. Mirrors
/// the CSS `baseline-shift` property.
///
/// [`Top`](Self::Top) and [`Bottom`](Self::Bottom) are *line-relative*: the box, together with
/// the descendants that are aligned relative to it, is placed against the line box instead of
/// its parent span, and the [`AlignmentBaseline`] is ignored. All other values are relative to
/// the parent span.
///
/// [`InlineBox`]: crate::InlineBox
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BaselineShift {
    /// Raise the box by the given distance (in layout units). Negative values lower it.
    Length(f32),
    /// Lower the box to the proper position for subscripts of the parent span.
    Sub,
    /// Raise the box to the proper position for superscripts of the parent span.
    Super,
    /// Align the top of the box (and its descendants) with the top of the line box.
    Top,
    /// Align the bottom of the box (and its descendants) with the bottom of the line box.
    Bottom,
}

impl Default for BaselineShift {
    fn default() -> Self {
        Self::Length(0.)
    }
}

/// Vertical alignment of a span (a styled range of text) or an [`InlineBox`] within its line.
/// This mirrors the CSS `vertical-align` shorthand, which sets [`AlignmentBaseline`] and
/// [`BaselineShift`]; the associated constants cover the CSS 2 keywords.
///
/// [`InlineBox`]: crate::InlineBox
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct VerticalAlign {
    /// The `alignment-baseline` component.
    pub alignment: AlignmentBaseline,
    /// The `baseline-shift` component.
    pub shift: BaselineShift,
}

impl VerticalAlign {
    /// `vertical-align: baseline`.
    pub const BASELINE: Self = Self::new(AlignmentBaseline::Baseline, BaselineShift::Length(0.));
    /// `vertical-align: sub`.
    pub const SUB: Self = Self::new(AlignmentBaseline::Baseline, BaselineShift::Sub);
    /// `vertical-align: super`.
    pub const SUPER: Self = Self::new(AlignmentBaseline::Baseline, BaselineShift::Super);
    /// `vertical-align: text-top`.
    pub const TEXT_TOP: Self = Self::new(AlignmentBaseline::TextTop, BaselineShift::Length(0.));
    /// `vertical-align: text-bottom`.
    pub const TEXT_BOTTOM: Self =
        Self::new(AlignmentBaseline::TextBottom, BaselineShift::Length(0.));
    /// `vertical-align: middle`.
    pub const MIDDLE: Self = Self::new(AlignmentBaseline::Middle, BaselineShift::Length(0.));
    /// `vertical-align: top`.
    pub const TOP: Self = Self::new(AlignmentBaseline::Baseline, BaselineShift::Top);
    /// `vertical-align: bottom`.
    pub const BOTTOM: Self = Self::new(AlignmentBaseline::Baseline, BaselineShift::Bottom);

    /// Combine an `alignment-baseline` with a `baseline-shift`.
    pub const fn new(alignment: AlignmentBaseline, shift: BaselineShift) -> Self {
        Self { alignment, shift }
    }

    /// `vertical-align: <length>`: raise the baseline of the box by `length` (in layout units)
    /// above the baseline of the parent span. Negative values lower the baseline.
    pub const fn length(length: f32) -> Self {
        Self::new(AlignmentBaseline::Baseline, BaselineShift::Length(length))
    }

    /// Whether this value is line-relative (`top` or `bottom`), i.e. aligns the box to the line
    /// box rather than to its parent span.
    pub fn is_line_relative(self) -> bool {
        matches!(self.shift, BaselineShift::Top | BaselineShift::Bottom)
    }

    pub(crate) fn nearly_eq(self, other: Self) -> bool {
        self.alignment == other.alignment
            && match (self.shift, other.shift) {
                (BaselineShift::Length(a), BaselineShift::Length(b)) => nearly_eq(a, b),
                (a, b) => a == b,
            }
    }

    pub(crate) fn scale(self, scale: f32) -> Self {
        match self.shift {
            BaselineShift::Length(value) => Self {
                shift: BaselineShift::Length(value * scale),
                ..self
            },
            _ => self,
        }
    }
}

/// CSS `border-radius`. Eight values: an X- and Y-radius for each
/// of the four corners. Default (`0.0`) means a square corner.
///
/// Sized in layout units (post-scale). For inline-span fragments
/// that wrap across line breaks, the painter only rounds the
/// corners belonging to the *first* and *last* fragment of the
/// span (start-of-first-line corners, end-of-last-line corners);
/// interior fragment corners are square. This matches the CSS
/// `box-decoration-break: slice` default.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BorderRadius {
    /// Top-left corner X radius (LTR start-of-first-line).
    pub top_left_x: f32,
    /// Top-left corner Y radius.
    pub top_left_y: f32,
    /// Top-right corner X radius (LTR end-of-first-line).
    pub top_right_x: f32,
    /// Top-right corner Y radius.
    pub top_right_y: f32,
    /// Bottom-right corner X radius (LTR end-of-last-line).
    pub bottom_right_x: f32,
    /// Bottom-right corner Y radius.
    pub bottom_right_y: f32,
    /// Bottom-left corner X radius (LTR start-of-last-line).
    pub bottom_left_x: f32,
    /// Bottom-left corner Y radius.
    pub bottom_left_y: f32,
}

impl BorderRadius {
    /// All-corners-equal convenience.
    pub fn all(r: f32) -> Self {
        Self {
            top_left_x: r,
            top_left_y: r,
            top_right_x: r,
            top_right_y: r,
            bottom_right_x: r,
            bottom_right_y: r,
            bottom_left_x: r,
            bottom_left_y: r,
        }
    }

    /// `true` if every radius is zero (a square box). Painters can
    /// fast-path this case and skip the rounded-corner clip pass.
    pub fn is_zero(&self) -> bool {
        self.top_left_x == 0.0
            && self.top_left_y == 0.0
            && self.top_right_x == 0.0
            && self.top_right_y == 0.0
            && self.bottom_right_x == 0.0
            && self.bottom_right_y == 0.0
            && self.bottom_left_x == 0.0
            && self.bottom_left_y == 0.0
    }

    pub(crate) fn scale(self, scale: f32) -> Self {
        Self {
            top_left_x: self.top_left_x * scale,
            top_left_y: self.top_left_y * scale,
            top_right_x: self.top_right_x * scale,
            top_right_y: self.top_right_y * scale,
            bottom_right_x: self.bottom_right_x * scale,
            bottom_right_y: self.bottom_right_y * scale,
            bottom_left_x: self.bottom_left_x * scale,
            bottom_left_y: self.bottom_left_y * scale,
        }
    }
}

/// CSS box decorations on an inline style span (padding, border,
/// background, rounded corners). All zero / `None` means "no
/// decoration", which is upstream parley's behavior: a span
/// only affects glyph styling.
///
/// The line-breaker uses `padding_inline_*` and
/// `border_inline_*_width` to charge horizontal advance at span
/// enter/leave, so `<a class="pill">` correctly takes up its full
/// padded width on the line. The painter uses every field to
/// emit per-line fragment rectangles between glyphs and
/// underlines.
///
/// Logical edges (`*_inline_*`, `*_block_*`) follow CSS logical
/// properties: in horizontal-tb LTR (the only mode parley
/// currently lays out), `inline-start` = left, `inline-end` =
/// right, `block-start` = top, `block-end` = bottom.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BoxDecorations<B: Brush> {
    /// Inline-start padding (LTR: left).
    pub padding_inline_start: f32,
    /// Inline-end padding (LTR: right).
    pub padding_inline_end: f32,
    /// Block-start padding (top). Affects background-fill height.
    pub padding_block_start: f32,
    /// Block-end padding (bottom). Affects background-fill height.
    pub padding_block_end: f32,
    /// Inline-start border thickness.
    pub border_inline_start_width: f32,
    /// Inline-end border thickness.
    pub border_inline_end_width: f32,
    /// Block-start border thickness (top).
    pub border_block_start_width: f32,
    /// Block-end border thickness (bottom).
    pub border_block_end_width: f32,
    /// Brush for the inline-start border. `None` falls back to the
    /// span's text brush.
    pub border_inline_start_brush: Option<B>,
    /// Brush for the inline-end border. `None` falls back to text.
    pub border_inline_end_brush: Option<B>,
    /// Brush for the block-start border. `None` falls back to text.
    pub border_block_start_brush: Option<B>,
    /// Brush for the block-end border. `None` falls back to text.
    pub border_block_end_brush: Option<B>,
    /// Background fill behind glyphs. `None` means no fill.
    pub background_brush: Option<B>,
    /// Per-corner border radii (CSS `border-radius`).
    pub border_radius: BorderRadius,
}

impl<B: Brush> BoxDecorations<B> {
    /// `true` if every dimension is zero and every brush `None`,
    /// meaning this span carries no box decorations and the painter
    /// can skip its fragment pass.
    pub fn is_empty(&self) -> bool {
        self.padding_inline_start == 0.0
            && self.padding_inline_end == 0.0
            && self.padding_block_start == 0.0
            && self.padding_block_end == 0.0
            && self.border_inline_start_width == 0.0
            && self.border_inline_end_width == 0.0
            && self.border_block_start_width == 0.0
            && self.border_block_end_width == 0.0
            && self.background_brush.is_none()
            && self.border_radius.is_zero()
    }

    pub(crate) fn scale(&self, scale: f32) -> Self {
        Self {
            padding_inline_start: self.padding_inline_start * scale,
            padding_inline_end: self.padding_inline_end * scale,
            padding_block_start: self.padding_block_start * scale,
            padding_block_end: self.padding_block_end * scale,
            border_inline_start_width: self.border_inline_start_width * scale,
            border_inline_end_width: self.border_inline_end_width * scale,
            border_block_start_width: self.border_block_start_width * scale,
            border_block_end_width: self.border_block_end_width * scale,
            border_inline_start_brush: self.border_inline_start_brush.clone(),
            border_inline_end_brush: self.border_inline_end_brush.clone(),
            border_block_start_brush: self.border_block_start_brush.clone(),
            border_block_end_brush: self.border_block_end_brush.clone(),
            background_brush: self.background_brush.clone(),
            border_radius: self.border_radius.scale(scale),
        }
    }
}

/// Properties that define a style.
#[derive(Clone, PartialEq, Debug)]
pub enum StyleProperty<'a, B: Brush> {
    /// CSS `font-family` property value.
    FontFamily(FontFamily<'a>),
    /// Font size.
    FontSize(f32),
    /// Font width.
    FontWidth(FontWidth),
    /// Font style.
    FontStyle(FontStyle),
    /// Font weight.
    FontWeight(FontWeight),
    /// Font variation settings.
    FontVariations(FontVariations<'a>),
    /// Font feature settings.
    FontFeatures(FontFeatures<'a>),
    /// Locale.
    Locale(Option<Language>),
    /// Brush for rendering text.
    Brush(B),
    /// Underline decoration.
    Underline(bool),
    /// Offset of the underline decoration.
    UnderlineOffset(Option<f32>),
    /// Size of the underline decoration.
    UnderlineSize(Option<f32>),
    /// Brush for rendering the underline decoration.
    UnderlineBrush(Option<B>),
    /// Strikethrough decoration.
    Strikethrough(bool),
    /// Offset of the strikethrough decoration.
    StrikethroughOffset(Option<f32>),
    /// Size of the strikethrough decoration.
    StrikethroughSize(Option<f32>),
    /// Brush for rendering the strikethrough decoration.
    StrikethroughBrush(Option<B>),
    /// Line height.
    LineHeight(LineHeight),
    /// Vertical alignment within the line.
    VerticalAlign(VerticalAlign),
    /// Extra spacing between words.
    WordSpacing(f32),
    /// Extra spacing between letters.
    LetterSpacing(f32),
    /// Control over where words can wrap.
    WordBreak(WordBreak),
    /// Strictness of line-breaking rules.
    LineBreak(LineBreak),
    /// Control over "emergency" line-breaking.
    OverflowWrap(OverflowWrap),
    /// Control over non-"emergency" line-breaking.
    TextWrapMode(TextWrapMode),
    /// Whitespace collapsing and hanging behavior.
    WhiteSpaceCollapse(WhiteSpaceCollapse),
    /// Box decorations (padding, border, background,
    /// border-radius). Replaces the whole `BoxDecorations` struct
    /// atomically. There's no per-field property because callers
    /// (chiefly blitz) build the whole struct at construction time
    /// and push it via `push_style_span(TextStyle{..})` rather
    /// than property-by-property.
    BoxDecorations(BoxDecorations<B>),
}

/// Unresolved styles.
#[derive(Clone, PartialEq, Debug)]
pub struct TextStyle<'family, 'settings, B: Brush> {
    /// CSS `font-family` property value.
    pub font_family: FontFamily<'family>,
    /// Font size.
    pub font_size: f32,
    /// Font width.
    pub font_width: FontWidth,
    /// Font style.
    pub font_style: FontStyle,
    /// Font weight.
    pub font_weight: FontWeight,
    /// Font variation settings.
    pub font_variations: FontVariations<'settings>,
    /// Font feature settings.
    pub font_features: FontFeatures<'settings>,
    /// Locale.
    pub locale: Option<Language>,
    /// Brush for rendering text.
    pub brush: B,
    /// Underline decoration.
    pub has_underline: bool,
    /// Offset of the underline decoration.
    pub underline_offset: Option<f32>,
    /// Size of the underline decoration.
    pub underline_size: Option<f32>,
    /// Brush for rendering the underline decoration.
    pub underline_brush: Option<B>,
    /// Strikethrough decoration.
    pub has_strikethrough: bool,
    /// Offset of the strikethrough decoration.
    pub strikethrough_offset: Option<f32>,
    /// Size of the strikethrough decoration.
    pub strikethrough_size: Option<f32>,
    /// Brush for rendering the strikethrough decoration.
    pub strikethrough_brush: Option<B>,
    /// Line height.
    pub line_height: LineHeight,
    /// Vertical alignment within the line.
    pub vertical_align: VerticalAlign,
    /// Extra spacing between words.
    pub word_spacing: f32,
    /// Extra spacing between letters.
    pub letter_spacing: f32,
    /// Control over where words can wrap.
    pub word_break: WordBreak,
    /// Strictness of line-breaking rules.
    pub line_break: LineBreak,
    /// Control over "emergency" line-breaking.
    pub overflow_wrap: OverflowWrap,
    /// Control over non-"emergency" line-breaking.
    pub text_wrap_mode: TextWrapMode,
    /// Whitespace collapsing and hanging behavior.
    pub white_space_collapse: WhiteSpaceCollapse,
    /// CSS box decorations (padding, border, background,
    /// border-radius). Default is "no decorations" (every
    /// dimension `0.0`, every brush `None`), which preserves
    /// upstream parley behavior. Set on a span pushed via
    /// `TreeBuilder::push_style_span` to make that span lay out
    /// and paint as a real CSS inline fragment with box geometry.
    pub box_decorations: BoxDecorations<B>,
    /// CSS `tab-size`: how far a horizontal tab advances the pen.
    /// Defaults to `Chars(8.0)`, the CSS initial value.
    pub tab_size: TabSize,
}

impl<B: Brush> Default for TextStyle<'static, 'static, B> {
    fn default() -> Self {
        TextStyle {
            font_family: FontFamily::Source(Cow::Borrowed("sans-serif")),
            font_size: 16.0,
            font_width: FontWidth::default(),
            font_style: FontStyle::default(),
            font_weight: FontWeight::default(),
            font_variations: FontVariations::empty(),
            font_features: FontFeatures::empty(),
            locale: None,
            brush: B::default(),
            has_underline: false,
            underline_offset: None,
            underline_size: None,
            underline_brush: None,
            has_strikethrough: false,
            strikethrough_offset: None,
            strikethrough_size: None,
            strikethrough_brush: None,
            line_height: LineHeight::default(),
            vertical_align: VerticalAlign::default(),
            word_spacing: 0.0,
            letter_spacing: 0.0,
            word_break: WordBreak::default(),
            line_break: LineBreak::default(),
            overflow_wrap: OverflowWrap::default(),
            text_wrap_mode: TextWrapMode::default(),
            white_space_collapse: WhiteSpaceCollapse::default(),
            box_decorations: BoxDecorations::default(),
            tab_size: TabSize::default(),
        }
    }
}

impl<'a, B: Brush> From<FontFamily<'a>> for StyleProperty<'a, B> {
    fn from(value: FontFamily<'a>) -> Self {
        StyleProperty::FontFamily(value)
    }
}

impl<'a, B: Brush> From<&'a [FontFamilyName<'a>]> for StyleProperty<'a, B> {
    fn from(value: &'a [FontFamilyName<'a>]) -> Self {
        StyleProperty::FontFamily(value.into())
    }
}

impl<'a, B: Brush> From<FontFamilyName<'a>> for StyleProperty<'a, B> {
    fn from(value: FontFamilyName<'a>) -> Self {
        StyleProperty::FontFamily(value.into())
    }
}

impl<'a, B: Brush> From<FontVariations<'a>> for StyleProperty<'a, B> {
    fn from(value: FontVariations<'a>) -> Self {
        StyleProperty::FontVariations(value)
    }
}

impl<'a, B: Brush> From<FontFeatures<'a>> for StyleProperty<'a, B> {
    fn from(value: FontFeatures<'a>) -> Self {
        StyleProperty::FontFeatures(value)
    }
}

impl<B: Brush> From<GenericFamily> for StyleProperty<'_, B> {
    fn from(f: GenericFamily) -> Self {
        StyleProperty::FontFamily(f.into())
    }
}

impl<B: Brush> From<LineHeight> for StyleProperty<'_, B> {
    fn from(value: LineHeight) -> Self {
        StyleProperty::LineHeight(value)
    }
}

impl<B: Brush> From<VerticalAlign> for StyleProperty<'_, B> {
    fn from(value: VerticalAlign) -> Self {
        StyleProperty::VerticalAlign(value)
    }
}

impl<B: Brush> From<FontWidth> for StyleProperty<'_, B> {
    fn from(value: FontWidth) -> Self {
        StyleProperty::FontWidth(value)
    }
}

impl<B: Brush> From<FontStyle> for StyleProperty<'_, B> {
    fn from(value: FontStyle) -> Self {
        StyleProperty::FontStyle(value)
    }
}

impl<B: Brush> From<FontWeight> for StyleProperty<'_, B> {
    fn from(value: FontWeight) -> Self {
        StyleProperty::FontWeight(value)
    }
}

impl<B: Brush> From<WordBreak> for StyleProperty<'_, B> {
    fn from(value: WordBreak) -> Self {
        StyleProperty::WordBreak(value)
    }
}

impl<B: Brush> From<LineBreak> for StyleProperty<'_, B> {
    fn from(value: LineBreak) -> Self {
        StyleProperty::LineBreak(value)
    }
}

impl<B: Brush> From<OverflowWrap> for StyleProperty<'_, B> {
    fn from(value: OverflowWrap) -> Self {
        StyleProperty::OverflowWrap(value)
    }
}

impl<B: Brush> From<TextWrapMode> for StyleProperty<'_, B> {
    fn from(value: TextWrapMode) -> Self {
        StyleProperty::TextWrapMode(value)
    }
}
