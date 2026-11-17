//! Conversion functions from Stylo types to Parley types
use blitz_traits::node_id::NodeId;
use std::borrow::Cow;

use style::values::computed::Length;

use crate::node::TextBrush;

// Module of type aliases so we can refer to stylo types with nicer names
pub(crate) mod stylo {
    pub(crate) use style::computed_values::direction::T as Direction;
    pub(crate) use style::computed_values::font_variant_caps::T as FontVariantCaps;
    pub(crate) use style::computed_values::font_variant_position::T as FontVariantPosition;
    pub(crate) use style::computed_values::text_wrap_mode::T as TextWrapMode;
    pub(crate) use style::computed_values::unicode_bidi::T as UnicodeBidi;
    pub(crate) use style::computed_values::white_space_collapse::T as WhiteSpaceCollapse;
    pub(crate) use style::properties::ComputedValues;
    pub(crate) use style::properties::style_structs::Font;
    pub(crate) use style::values::computed::AlignmentBaseline;
    pub(crate) use style::values::computed::BaselineShift;
    pub(crate) use style::values::computed::LineBreak;
    pub(crate) use style::values::computed::OverflowWrap;
    pub(crate) use style::values::computed::WordBreak;
    pub(crate) use style::values::computed::font::FontFeatureSettings;
    pub(crate) use style::values::computed::font::FontStyle;
    pub(crate) use style::values::computed::font::FontVariantEastAsian;
    pub(crate) use style::values::computed::font::FontVariantLigatures;
    pub(crate) use style::values::computed::font::FontVariantNumeric;
    pub(crate) use style::values::computed::font::FontVariationSettings;
    pub(crate) use style::values::computed::font::FontWeight;
    pub(crate) use style::values::computed::font::FontWidth;
    pub(crate) use style::values::computed::font::GenericFontFamily;
    pub(crate) use style::values::computed::font::LineHeight;
    pub(crate) use style::values::computed::font::SingleFontFamily;
    pub(crate) use style::values::generics::box_::BaselineShiftKeyword;
    pub(crate) use style::values::specified::TextAlignKeyword;
    pub(crate) use style::values::specified::TextAlignLast;
}

pub(crate) mod parley {
    pub(crate) use parley::BaseDirection;
    pub(crate) use parley::FontFeature;
    pub(crate) use parley::FontVariation;
    pub(crate) use parley::fontique::QueryFamily;
    pub(crate) use parley::layout::Alignment;
    pub(crate) use parley::setting::*;
    pub(crate) use parley::style::*;
}

pub(crate) fn generic_font_family(input: stylo::GenericFontFamily) -> parley::GenericFamily {
    match input {
        stylo::GenericFontFamily::None => parley::GenericFamily::SansSerif,
        stylo::GenericFontFamily::Serif => parley::GenericFamily::Serif,
        stylo::GenericFontFamily::SansSerif => parley::GenericFamily::SansSerif,
        stylo::GenericFontFamily::Monospace => parley::GenericFamily::Monospace,
        stylo::GenericFontFamily::Cursive => parley::GenericFamily::Cursive,
        stylo::GenericFontFamily::Fantasy => parley::GenericFamily::Fantasy,
        stylo::GenericFontFamily::SystemUi => parley::GenericFamily::SystemUi,
    }
}

pub(crate) fn query_font_family(input: &stylo::SingleFontFamily) -> parley::QueryFamily<'_> {
    match input {
        stylo::SingleFontFamily::FamilyName(name) => {
            'ret: {
                let name = name.name.as_str();

                // Legacy web compatibility
                #[cfg(target_vendor = "apple")]
                if name == "-apple-system" {
                    break 'ret parley::QueryFamily::Generic(parley::GenericFamily::SystemUi);
                }
                #[cfg(target_os = "macos")]
                if name == "BlinkMacSystemFont" {
                    break 'ret parley::QueryFamily::Generic(parley::GenericFamily::SystemUi);
                }

                break 'ret parley::QueryFamily::Named(name);
            }
        }
        stylo::SingleFontFamily::Generic(generic) => {
            parley::QueryFamily::Generic(self::generic_font_family(*generic))
        }
    }
}

pub(crate) fn font_weight(input: stylo::FontWeight) -> parley::FontWeight {
    parley::FontWeight::new(input.value())
}

pub(crate) fn font_width(input: stylo::FontWidth) -> parley::FontWidth {
    parley::FontWidth::from_percentage(input.0.to_float())
}

pub(crate) fn font_style(input: stylo::FontStyle) -> parley::FontStyle {
    match input {
        stylo::FontStyle::NORMAL => parley::FontStyle::Normal,
        stylo::FontStyle::ITALIC => parley::FontStyle::Italic,
        val => parley::FontStyle::Oblique(Some(val.oblique_degrees())),
    }
}

pub(crate) fn font_variations(input: &stylo::FontVariationSettings) -> Vec<parley::FontVariation> {
    input
        .0
        .iter()
        .map(|v| parley::FontVariation {
            tag: parley::Tag::from_bytes(v.tag.0.to_be_bytes()),
            value: v.value,
        })
        .collect()
}

#[inline]
fn feature(tag: &[u8; 4], value: u16) -> parley::FontFeature {
    parley::FontFeature {
        tag: parley::Tag::from_bytes(*tag),
        value,
    }
}

/// Convert the `font-feature-settings` property (low-level OpenType feature control)
/// into a list of Parley font features.
pub(crate) fn font_feature_settings(
    input: &stylo::FontFeatureSettings,
    out: &mut Vec<parley::FontFeature>,
) {
    out.extend(input.0.iter().map(|v| parley::FontFeature {
        tag: parley::Tag::from_bytes(v.tag.0.to_be_bytes()),
        value: v.value as u16,
    }));
}

/// Map the `font-variant-ligatures` property to OpenType features.
pub(crate) fn font_variant_ligatures(
    input: stylo::FontVariantLigatures,
    out: &mut Vec<parley::FontFeature>,
) {
    use stylo::FontVariantLigatures as L;

    // `none` disables all optional ligature and contextual features.
    if input.contains(L::NONE) {
        out.push(feature(b"liga", 0));
        out.push(feature(b"clig", 0));
        out.push(feature(b"dlig", 0));
        out.push(feature(b"hlig", 0));
        out.push(feature(b"calt", 0));
        return;
    }

    if input.contains(L::COMMON_LIGATURES) {
        out.push(feature(b"liga", 1));
        out.push(feature(b"clig", 1));
    } else if input.contains(L::NO_COMMON_LIGATURES) {
        out.push(feature(b"liga", 0));
        out.push(feature(b"clig", 0));
    }
    if input.contains(L::DISCRETIONARY_LIGATURES) {
        out.push(feature(b"dlig", 1));
    } else if input.contains(L::NO_DISCRETIONARY_LIGATURES) {
        out.push(feature(b"dlig", 0));
    }
    if input.contains(L::HISTORICAL_LIGATURES) {
        out.push(feature(b"hlig", 1));
    } else if input.contains(L::NO_HISTORICAL_LIGATURES) {
        out.push(feature(b"hlig", 0));
    }
    if input.contains(L::CONTEXTUAL) {
        out.push(feature(b"calt", 1));
    } else if input.contains(L::NO_CONTEXTUAL) {
        out.push(feature(b"calt", 0));
    }
}

/// Map the `font-variant-caps` property to OpenType features.
pub(crate) fn font_variant_caps(input: stylo::FontVariantCaps, out: &mut Vec<parley::FontFeature>) {
    // OpenType feature mapping per css-fonts-3/#font-variant-caps-prop's
    // "Corresponding OpenType Feature(s)" column: the same table gecko
    // implements natively; small-caps already used it here.
    match input {
        stylo::FontVariantCaps::Normal => {}
        stylo::FontVariantCaps::SmallCaps => out.push(feature(b"smcp", 1)),
        stylo::FontVariantCaps::AllSmallCaps => {
            out.push(feature(b"smcp", 1));
            out.push(feature(b"c2sc", 1));
        }
        stylo::FontVariantCaps::PetiteCaps => out.push(feature(b"pcap", 1)),
        stylo::FontVariantCaps::AllPetiteCaps => {
            out.push(feature(b"pcap", 1));
            out.push(feature(b"c2pc", 1));
        }
        stylo::FontVariantCaps::Unicase => out.push(feature(b"unic", 1)),
        stylo::FontVariantCaps::TitlingCaps => out.push(feature(b"titl", 1)),
    }
}

/// Map the `font-variant-position` property to OpenType features.
pub(crate) fn font_variant_position(
    input: stylo::FontVariantPosition,
    out: &mut Vec<parley::FontFeature>,
) {
    match input {
        stylo::FontVariantPosition::Normal => {}
        stylo::FontVariantPosition::Sub => out.push(feature(b"subs", 1)),
        stylo::FontVariantPosition::Super => out.push(feature(b"sups", 1)),
    }
}

/// Map the `font-variant-numeric` property to OpenType features.
pub(crate) fn font_variant_numeric(
    input: stylo::FontVariantNumeric,
    out: &mut Vec<parley::FontFeature>,
) {
    use stylo::FontVariantNumeric as N;

    if input.contains(N::LINING_NUMS) {
        out.push(feature(b"lnum", 1));
    }
    if input.contains(N::OLDSTYLE_NUMS) {
        out.push(feature(b"onum", 1));
    }
    if input.contains(N::PROPORTIONAL_NUMS) {
        out.push(feature(b"pnum", 1));
    }
    if input.contains(N::TABULAR_NUMS) {
        out.push(feature(b"tnum", 1));
    }
    if input.contains(N::DIAGONAL_FRACTIONS) {
        out.push(feature(b"frac", 1));
    }
    if input.contains(N::STACKED_FRACTIONS) {
        out.push(feature(b"afrc", 1));
    }
    if input.contains(N::ORDINAL) {
        out.push(feature(b"ordn", 1));
    }
    if input.contains(N::SLASHED_ZERO) {
        out.push(feature(b"zero", 1));
    }
}

/// Map the `font-variant-east-asian` property to OpenType features.
pub(crate) fn font_variant_east_asian(
    input: stylo::FontVariantEastAsian,
    out: &mut Vec<parley::FontFeature>,
) {
    use stylo::FontVariantEastAsian as E;

    if input.contains(E::JIS78) {
        out.push(feature(b"jp78", 1));
    }
    if input.contains(E::JIS83) {
        out.push(feature(b"jp83", 1));
    }
    if input.contains(E::JIS90) {
        out.push(feature(b"jp90", 1));
    }
    if input.contains(E::JIS04) {
        out.push(feature(b"jp04", 1));
    }
    if input.contains(E::SIMPLIFIED) {
        out.push(feature(b"smpl", 1));
    }
    if input.contains(E::TRADITIONAL) {
        out.push(feature(b"trad", 1));
    }
    if input.contains(E::FULL_WIDTH) {
        out.push(feature(b"fwid", 1));
    }
    if input.contains(E::PROPORTIONAL_WIDTH) {
        out.push(feature(b"pwid", 1));
    }
    if input.contains(E::RUBY) {
        out.push(feature(b"ruby", 1));
    }
}

/// Build the full list of OpenType font features for a computed font style.
///
/// Per the CSS Fonts feature precedence rules
/// (<https://drafts.csswg.org/css-fonts/#feature-precedence>), features implied by the
/// high-level `font-variant-*` properties are applied first, and the low-level
/// `font-feature-settings` property is applied last so that it takes precedence.
///
/// Parley stably sorts the feature list by tag and HarfBuzz applies the last of any
/// duplicate tags, so appending `font-feature-settings` after the `font-variant-*`
/// features gives it the higher precedence required by the spec.
pub(crate) fn font_features(font_styles: &stylo::Font) -> Vec<parley::FontFeature> {
    let mut features = Vec::new();

    self::font_variant_ligatures(font_styles.font_variant_ligatures, &mut features);
    self::font_variant_caps(font_styles.font_variant_caps, &mut features);
    self::font_variant_numeric(font_styles.font_variant_numeric, &mut features);
    self::font_variant_east_asian(font_styles.font_variant_east_asian, &mut features);
    self::font_variant_position(font_styles.font_variant_position, &mut features);

    self::font_feature_settings(&font_styles.font_feature_settings, &mut features);

    dedup_features_keep_last(features)
}

/// Collapse duplicate OpenType tags, keeping the last occurrence of each:
/// not the first. `font_features` appends `font-feature-settings` after the
/// `font-variant-*`-derived entries specifically so an explicit author
/// feature wins per css-fonts/#feature-precedence (e.g.
/// `font-variant-caps: all-small-caps; font-feature-settings: "smcp" 0`
/// must turn `smcp` back off, even though all-small-caps also emits it).
/// `Vec::dedup_by_key` on a plain ascending sort keeps the first of a
/// run, which silently discarded that override: the existing
/// `feature_settings_override_variant_features` test below only checked
/// the two sub-functions' raw output, never this merge step, so it never
/// caught it. Scan from the end so the first tag we see per key is the
/// author's (or the highest-precedence variant-derived one), then
/// restore ascending tag order.
fn dedup_features_keep_last(features: Vec<parley::FontFeature>) -> Vec<parley::FontFeature> {
    let mut seen = std::collections::HashSet::new();
    let mut kept: Vec<_> = features
        .into_iter()
        .rev()
        .filter(|feature| seen.insert(feature.tag))
        .collect();
    kept.sort_by_key(|feature| feature.tag);
    kept
}

pub(crate) fn base_direction(
    direction: stylo::Direction,
    unicode_bidi: stylo::UnicodeBidi,
) -> parley::BaseDirection {
    if unicode_bidi == stylo::UnicodeBidi::Plaintext {
        return parley::BaseDirection::Auto;
    }
    match direction {
        stylo::Direction::Ltr => parley::BaseDirection::Ltr,
        stylo::Direction::Rtl => parley::BaseDirection::Rtl,
    }
}

pub(crate) fn text_align(input: stylo::TextAlignKeyword) -> parley::Alignment {
    match input {
        stylo::TextAlignKeyword::Start => parley::Alignment::Start,
        stylo::TextAlignKeyword::Left => parley::Alignment::Left,
        stylo::TextAlignKeyword::Right => parley::Alignment::Right,
        stylo::TextAlignKeyword::Center => parley::Alignment::Center,
        stylo::TextAlignKeyword::Justify => parley::Alignment::Justify,
        stylo::TextAlignKeyword::End => parley::Alignment::End,
        stylo::TextAlignKeyword::MozCenter => parley::Alignment::Center,
        stylo::TextAlignKeyword::MozLeft => parley::Alignment::Left,
        stylo::TextAlignKeyword::MozRight => parley::Alignment::Right,
    }
}

/// Returns `None` for `text-align-last: auto`
pub(crate) fn text_align_last(input: stylo::TextAlignLast) -> Option<parley::Alignment> {
    match input {
        stylo::TextAlignLast::Auto => None,
        stylo::TextAlignLast::Start => Some(parley::Alignment::Start),
        stylo::TextAlignLast::End => Some(parley::Alignment::End),
        stylo::TextAlignLast::Left => Some(parley::Alignment::Left),
        stylo::TextAlignLast::Right => Some(parley::Alignment::Right),
        stylo::TextAlignLast::Center => Some(parley::Alignment::Center),
        stylo::TextAlignLast::Justify => Some(parley::Alignment::Justify),
    }
}

pub(crate) fn text_wrap_mode(input: stylo::TextWrapMode) -> parley::TextWrapMode {
    match input {
        stylo::TextWrapMode::Wrap => parley::TextWrapMode::Wrap,
        stylo::TextWrapMode::Nowrap => parley::TextWrapMode::NoWrap,
    }
}

pub(crate) fn white_space_collapse(input: stylo::WhiteSpaceCollapse) -> parley::WhiteSpaceCollapse {
    match input {
        stylo::WhiteSpaceCollapse::Collapse => parley::WhiteSpaceCollapse::Collapse,
        stylo::WhiteSpaceCollapse::Preserve => parley::WhiteSpaceCollapse::Preserve,

        stylo::WhiteSpaceCollapse::PreserveBreaks => parley::WhiteSpaceCollapse::PreserveBreaks,
        stylo::WhiteSpaceCollapse::BreakSpaces => parley::WhiteSpaceCollapse::BreakSpaces,
    }
}

/// Map the css-inline-3 `alignment-baseline` and `baseline-shift` longhands (which Stylo
/// stores in place of the `vertical-align` shorthand) to Parley's `VerticalAlign`.
///
/// Percentages are resolved against the element's own `line-height`.
pub(crate) fn vertical_align(style: &stylo::ComputedValues) -> parley::VerticalAlign {
    let box_styles = style.get_box();
    let alignment = match box_styles.clone_alignment_baseline() {
        stylo::AlignmentBaseline::Baseline => parley::AlignmentBaseline::Baseline,
        stylo::AlignmentBaseline::TextTop => parley::AlignmentBaseline::TextTop,
        stylo::AlignmentBaseline::TextBottom => parley::AlignmentBaseline::TextBottom,
        stylo::AlignmentBaseline::Middle => parley::AlignmentBaseline::Middle,
    };
    let shift = match box_styles.clone_baseline_shift() {
        stylo::BaselineShift::Keyword(stylo::BaselineShiftKeyword::Sub) => {
            parley::BaselineShift::Sub
        }
        stylo::BaselineShift::Keyword(stylo::BaselineShiftKeyword::Super) => {
            parley::BaselineShift::Super
        }
        stylo::BaselineShift::Keyword(stylo::BaselineShiftKeyword::Top) => {
            parley::BaselineShift::Top
        }
        stylo::BaselineShift::Keyword(stylo::BaselineShiftKeyword::Bottom) => {
            parley::BaselineShift::Bottom
        }
        // TODO: `center` (align the aligned subtree's centre with the line box's centre) is not
        // representable in Parley yet. Approximate it with `middle`.
        stylo::BaselineShift::Keyword(stylo::BaselineShiftKeyword::Center) => {
            return parley::VerticalAlign::MIDDLE;
        }
        stylo::BaselineShift::Length(lp) => {
            let shift = if lp.has_percentage() {
                let font_styles = style.get_font();
                let font_size = font_styles.font_size.used_size.0.px();
                let line_height = match font_styles.line_height {
                    // TODO: `normal` resolves against the first available font's metrics, which
                    // aren't available here; 1.2 is the usual approximation.
                    stylo::LineHeight::Normal => font_size * 1.2,
                    stylo::LineHeight::Number(num) => font_size * num.0,
                    stylo::LineHeight::Length(value) => value.0.px(),
                };
                lp.resolve(Length::new(line_height)).px()
            } else {
                lp.resolve(Length::new(0.0)).px()
            };
            parley::BaselineShift::Length(shift)
        }
    };
    parley::VerticalAlign::new(alignment, shift)
}

/// Extract CSS box decorations (padding, border, background,
/// border-radius) from a stylo `ComputedValues` into a
/// `BoxDecorations` ready to attach to a parley `TextStyle`.
///
/// Conventions:
/// - The block-/inline-axis mapping assumes horizontal-tb LTR
///   (the only writing mode parley currently lays out): block-
///   start = top, block-end = bottom, inline-start = left,
///   inline-end = right.
/// - Padding values that resolve to a percentage at computed-
///   value time would normally need the inline fragment's
///   container width to be resolved. We don't have that at
///   style-extraction time, so percentage-padding falls back to
///   `0.0`. Pure-length padding (the common case) works.
/// - Border widths are always pure lengths (CSS spec) so they
///   convert cleanly via `to_f64_px`.
/// - Border-radius percentages also need fragment dimensions;
///   they fall back to `0.0`.
/// - The background brush carries the node id only when a
///   background color needs a fragment; painting resolves the actual
///   color from that styled node. Border brushes remain unset.
fn extract_box_decorations(
    span_id: NodeId,
    style: &stylo::ComputedValues,
) -> parley::BoxDecorations<TextBrush> {
    use ::style::values::computed::Au;
    use ::style::values::specified::BorderStyle;
    use ::style::values::specified::box_::{DisplayInside, DisplayOutside};

    // Only `display: inline` proper takes inline-span decorations. Atomic inline boxes already
    // paint their own border, and block-level containers already have padding and border applied
    // by taffy, so extracting either here would apply them twice. The computed display already
    // reflects blockification.
    let display = style.clone_display();
    if !matches!(display.outside(), DisplayOutside::Inline) {
        return parley::BoxDecorations::default();
    }
    if !matches!(display.inside(), DisplayInside::Flow) {
        return parley::BoxDecorations::default();
    }

    let padding = style.get_padding();
    let border = style.get_border();

    let resolve_px = |lp: &::style::values::computed::LengthPercentage| -> f32 {
        // `maybe_to_used_value(None)` returns `Some(Au)` when the
        // length-percentage is purely a length (no percentage
        // component); falls back to 0.0 for any percentage we
        // can't resolve without a containing-block size.
        lp.maybe_to_used_value(None)
            .map(Au::to_f32_px)
            .unwrap_or(0.0)
    };

    // Per-edge border style → "is this border actually visible".
    // CSS spec: when border-style is `none` or `hidden`, the
    // computed border-width is 0 and nothing is painted. Stylo
    // preserves the specified width even when style is None
    // (e.g. UA default `medium` on every element); without this
    // filter we'd charge ~3px of advance and paint a stripe for
    // every inline element on the page. Match the existing
    // blitz-paint behavior in render.rs:913 (which checks
    // `border_*_style != BorderStyle::Hidden` before drawing).
    let visible = |s: BorderStyle, w: ::style::values::computed::Au| -> f32 {
        match s {
            BorderStyle::None | BorderStyle::Hidden => 0.0,
            _ => w.to_f32_px(),
        }
    };
    let border_block_start_width =
        visible(border.border_top_style, border.border_top_width.0);
    let border_block_end_width =
        visible(border.border_bottom_style, border.border_bottom_width.0);
    let border_inline_start_width =
        visible(border.border_left_style, border.border_left_width.0);
    let border_inline_end_width =
        visible(border.border_right_style, border.border_right_width.0);

    // Padding.
    let padding_block_start = resolve_px(&padding.padding_top.0);
    let padding_block_end = resolve_px(&padding.padding_bottom.0);
    let padding_inline_start = resolve_px(&padding.padding_left.0);
    let padding_inline_end = resolve_px(&padding.padding_right.0);

    // Border radius (per-corner X/Y). Percentages resolve to 0.0.
    let resolve_radius = |corner: &::style::values::computed::BorderCornerRadius| -> (f32, f32) {
        (resolve_px(&corner.0.width.0), resolve_px(&corner.0.height.0))
    };
    let (top_left_x, top_left_y) = resolve_radius(&border.border_top_left_radius);
    let (top_right_x, top_right_y) = resolve_radius(&border.border_top_right_radius);
    let (bottom_right_x, bottom_right_y) =
        resolve_radius(&border.border_bottom_right_radius);
    let (bottom_left_x, bottom_left_y) = resolve_radius(&border.border_bottom_left_radius);
    parley::BoxDecorations {
        padding_inline_start,
        padding_inline_end,
        padding_block_start,
        padding_block_end,
        border_inline_start_width,
        border_inline_end_width,
        border_block_start_width,
        border_block_end_width,
        border_inline_start_brush: None,
        border_inline_end_brush: None,
        border_block_start_brush: None,
        border_block_end_brush: None,
        background_brush: has_inline_background(style).then(|| TextBrush::from_id(span_id)),
        border_radius: parley::BorderRadius {
            top_left_x,
            top_left_y,
            top_right_x,
            top_right_y,
            bottom_right_x,
            bottom_right_y,
            bottom_left_x,
            bottom_left_y,
        },
    }
}

pub(crate) fn has_inline_background(style: &stylo::ComputedValues) -> bool {
    use ::style::values::specified::box_::{DisplayInside, DisplayOutside};

    let display = style.clone_display();
    display.outside() == DisplayOutside::Inline
        && display.inside() == DisplayInside::Flow
        && style
            .get_background()
            .background_color
            .resolve_to_absolute(&style.clone_color())
            .to_color_space(::style::color::ColorSpace::Srgb)
            .raw_components()[3]
            > 0.0
}

pub(crate) fn style(
    span_id: NodeId,
    style: &stylo::ComputedValues,
) -> parley::TextStyle<'static, 'static, TextBrush> {
    let font_styles = style.get_font();
    let itext_styles = style.get_inherited_text();

    // Convert font size and line height
    let font_size = font_styles.font_size.used_size.0.px();
    let line_height = match font_styles.line_height {
        stylo::LineHeight::Normal => parley::LineHeight::NORMAL,
        stylo::LineHeight::Number(num) => parley::LineHeight::FontSizeRelative(num.0),
        stylo::LineHeight::Length(value) => parley::LineHeight::Absolute(value.0.px()),
    };
    let vertical_align = self::vertical_align(style);

    let letter_spacing = itext_styles
        .letter_spacing
        .0
        .resolve(Length::new(font_size))
        .px();

    let word_spacing = itext_styles
        .word_spacing
        .resolve(Length::new(font_size))
        .px();

    // Convert Bold/Italic
    let font_weight = self::font_weight(font_styles.font_weight);
    let font_style = self::font_style(font_styles.font_style);
    let font_width = self::font_width(font_styles.font_width);
    let font_variations = self::font_variations(&font_styles.font_variation_settings);
    let font_features = self::font_features(font_styles);

    // Convert font family
    let families: Vec<_> = font_styles
        .font_family
        .families
        .list
        .iter()
        .filter_map(|family| Some(match family {
            stylo::SingleFontFamily::FamilyName(name) => {
                'ret: {
                    let name = name.name.as_str();

                    // Skip a family still waiting on a face for this style:
                    // matching would otherwise substitute a loaded sibling
                    // of the wrong style. Dropping it here falls through to
                    // the next family, and registration re-shapes the text.
                    if crate::net::face_pending_for_style(&name.to_lowercase(), font_style) {
                        return None;
                    }

                    // Legacy web compatibility
                    #[cfg(target_vendor = "apple")]
                    if name == "-apple-system" {
                        break 'ret parley::FontFamilyName::Generic(
                            parley::GenericFamily::SystemUi,
                        );
                    }
                    #[cfg(target_os = "macos")]
                    if name == "BlinkMacSystemFont" {
                        break 'ret parley::FontFamilyName::Generic(
                            parley::GenericFamily::SystemUi,
                        );
                    }

                    break 'ret parley::FontFamilyName::Named(Cow::Owned(name.to_string()));
                }
            }
            stylo::SingleFontFamily::Generic(generic) => {
                parley::FontFamilyName::Generic(self::generic_font_family(*generic))
            }
        }))
        .collect();

    // Wrapping and breaking
    // `word-break: break-word` is a legacy compat value (css-text-3):
    // it behaves as `word-break: normal` together with
    // `overflow-wrap: anywhere`, regardless of the actual overflow-wrap
    // value. Fold that here rather than threading a third bit through
    // parley's own WordBreak, which has no break-word variant of its own.
    let word_break = match itext_styles.word_break {
        stylo::WordBreak::Normal | stylo::WordBreak::BreakWord => parley::WordBreak::Normal,
        stylo::WordBreak::BreakAll => parley::WordBreak::BreakAll,
        stylo::WordBreak::KeepAll => parley::WordBreak::KeepAll,
    };
    let line_break = match itext_styles.line_break {
        stylo::LineBreak::Loose => parley::LineBreak::Loose,
        stylo::LineBreak::Normal => parley::LineBreak::Normal,
        stylo::LineBreak::Auto | stylo::LineBreak::Strict => parley::LineBreak::Strict,
        stylo::LineBreak::Anywhere => parley::LineBreak::Anywhere,
    };
    let overflow_wrap = if itext_styles.word_break == stylo::WordBreak::BreakWord {
        parley::OverflowWrap::Anywhere
    } else {
        match itext_styles.overflow_wrap {
            stylo::OverflowWrap::Normal => parley::OverflowWrap::Normal,
            stylo::OverflowWrap::BreakWord => parley::OverflowWrap::BreakWord,
            stylo::OverflowWrap::Anywhere => parley::OverflowWrap::Anywhere,
        }
    };
    let text_wrap_mode = text_wrap_mode(itext_styles.text_wrap_mode);

    // CSS `tab-size`: a <number> is a count of space-advances per tab
    // (the CSS initial, 8); a <length> is an absolute advance.
    let tab_size = match itext_styles.tab_size {
        style::values::computed::NonNegativeLengthOrNumber::Number(n) => {
            parley::TabSize::Chars(n.0)
        }
        style::values::computed::NonNegativeLengthOrNumber::Length(l) => {
            parley::TabSize::Px(l.px())
        }
    };

    parley::TextStyle {
        // font_family: parley::FontFamily::Single(FontFamilyName::Generic(GenericFamily::SystemUi)),
        font_family: parley::FontFamily::List(Cow::Owned(families)),
        font_size,
        font_width,
        font_style,
        font_weight,
        font_variations: parley::FontVariations::List(Cow::Owned(font_variations)),
        font_features: parley::FontFeatures::List(Cow::Owned(font_features)),
        locale: parley::Language::parse(&font_styles._x_lang.0).ok(),
        line_height,
        vertical_align,
        word_spacing,
        letter_spacing,
        text_wrap_mode,
        white_space_collapse: white_space_collapse(itext_styles.white_space_collapse),
        overflow_wrap,
        word_break,
        line_break,
        tab_size,

        // Contains NodeId
        brush: TextBrush::from_id(span_id),

        // We avoid sending these styles through Parley because they don't affect layout
        // and handling them separately allows us to update them without rebuilding the Parley layout.
        //
        // Instead of setting them here we pass the NodeId in the `brush` field and use that to read these
        // styles lazily when rendering.
        has_underline: Default::default(),
        underline_offset: Default::default(),
        underline_size: Default::default(),
        underline_brush: Default::default(),
        has_strikethrough: Default::default(),
        strikethrough_offset: Default::default(),
        strikethrough_size: Default::default(),
        strikethrough_brush: Default::default(),

        // CSS box decorations (padding, border, background,
        // border-radius) extracted from stylo's computed values
        // for this span's element. The painter (blitz-paint's
        // text_box_decorations module) consumes this data via
        // parley's per-line span_fragments() to draw filled
        // backgrounds, edge borders, and rounded corners on
        // styled inline elements like <a class="pill">.
        box_decorations: extract_box_decorations(span_id, style),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use style::values::generics::font::{FeatureTagValue, FontSettings, FontTag};

    /// Collect the (tag, value) pairs from a feature list for easy assertions.
    fn pairs(features: &[parley::FontFeature]) -> Vec<([u8; 4], u16)> {
        features
            .iter()
            .map(|f| (f.tag.to_bytes(), f.value))
            .collect()
    }

    fn feature_settings(tags: &[(&[u8; 4], i32)]) -> stylo::FontFeatureSettings {
        FontSettings(
            tags.iter()
                .map(|(tag, value)| FeatureTagValue {
                    tag: FontTag(u32::from_be_bytes(**tag)),
                    value: *value,
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        )
    }

    #[test]
    fn ligatures_none_disables_all() {
        let mut out = Vec::new();
        font_variant_ligatures(stylo::FontVariantLigatures::NONE, &mut out);
        assert_eq!(
            pairs(&out),
            vec![
                (*b"liga", 0),
                (*b"clig", 0),
                (*b"dlig", 0),
                (*b"hlig", 0),
                (*b"calt", 0),
            ]
        );
    }

    #[test]
    fn ligatures_normal_emits_nothing() {
        let mut out = Vec::new();
        font_variant_ligatures(stylo::FontVariantLigatures::NORMAL, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn ligatures_mixed_toggles() {
        let mut out = Vec::new();
        let input = stylo::FontVariantLigatures::COMMON_LIGATURES
            | stylo::FontVariantLigatures::NO_DISCRETIONARY_LIGATURES
            | stylo::FontVariantLigatures::HISTORICAL_LIGATURES;
        font_variant_ligatures(input, &mut out);
        assert_eq!(
            pairs(&out),
            vec![(*b"liga", 1), (*b"clig", 1), (*b"dlig", 0), (*b"hlig", 1)]
        );
    }

    #[test]
    fn caps_small_caps() {
        let mut out = Vec::new();
        font_variant_caps(stylo::FontVariantCaps::SmallCaps, &mut out);
        assert_eq!(pairs(&out), vec![(*b"smcp", 1)]);

        let mut out = Vec::new();
        font_variant_caps(stylo::FontVariantCaps::Normal, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn position_sub_and_super() {
        let mut out = Vec::new();
        font_variant_position(stylo::FontVariantPosition::Sub, &mut out);
        assert_eq!(pairs(&out), vec![(*b"subs", 1)]);

        let mut out = Vec::new();
        font_variant_position(stylo::FontVariantPosition::Super, &mut out);
        assert_eq!(pairs(&out), vec![(*b"sups", 1)]);
    }

    #[test]
    fn numeric_features() {
        let mut out = Vec::new();
        let input = stylo::FontVariantNumeric::OLDSTYLE_NUMS
            | stylo::FontVariantNumeric::TABULAR_NUMS
            | stylo::FontVariantNumeric::SLASHED_ZERO;
        font_variant_numeric(input, &mut out);
        assert_eq!(
            pairs(&out),
            vec![(*b"onum", 1), (*b"tnum", 1), (*b"zero", 1)]
        );
    }

    #[test]
    fn east_asian_features() {
        let mut out = Vec::new();
        let input =
            stylo::FontVariantEastAsian::JIS04 | stylo::FontVariantEastAsian::PROPORTIONAL_WIDTH;
        font_variant_east_asian(input, &mut out);
        assert_eq!(pairs(&out), vec![(*b"jp04", 1), (*b"pwid", 1)]);
    }

    #[test]
    fn feature_settings_map_directly() {
        let mut out = Vec::new();
        font_feature_settings(&feature_settings(&[(b"smcp", 1), (b"tnum", 0)]), &mut out);
        assert_eq!(pairs(&out), vec![(*b"smcp", 1), (*b"tnum", 0)]);
    }

    /// `font-feature-settings` has a higher precedence than the `font-variant-*`
    /// properties, so its entries must be appended *after* the variant-derived ones.
    /// Parley stably sorts by tag and HarfBuzz applies the last of any duplicate tags,
    /// so the trailing `font-feature-settings` value wins.
    #[test]
    fn feature_settings_override_variant_features() {
        let mut out = Vec::new();
        // font-variant-numeric: tabular-nums => tnum=1
        font_variant_numeric(stylo::FontVariantNumeric::TABULAR_NUMS, &mut out);
        // font-feature-settings: "tnum" 0 (should override the variant)
        font_feature_settings(&feature_settings(&[(b"tnum", 0)]), &mut out);

        let flat = pairs(&out);
        // Both are present, and the feature-settings value comes last.
        assert_eq!(flat, vec![(*b"tnum", 1), (*b"tnum", 0)]);
        let last_tnum = flat.iter().rev().find(|(tag, _)| tag == b"tnum").unwrap();
        assert_eq!(last_tnum.1, 0);
    }

    /// Regression for the actual merge step (`font_features`'s dedup), not
    /// just the raw sub-function output above: an author's explicit
    /// `font-feature-settings` must survive the dedup and win over a
    /// `font-variant-caps`-derived feature with the same OpenType tag, even
    /// when the variant-derived feature emits multiple tags (all-small-caps
    /// emits both smcp and c2sc) so the override only targets one of them.
    #[test]
    fn dedup_keeps_author_override_over_variant_caps() {
        let mut out = Vec::new();
        // font-variant-caps: all-small-caps => smcp=1, c2sc=1
        font_variant_caps(stylo::FontVariantCaps::AllSmallCaps, &mut out);
        // font-feature-settings: "smcp" 0 (author turns smcp back off;
        // c2sc is untouched and must survive)
        font_feature_settings(&feature_settings(&[(b"smcp", 0)]), &mut out);

        let merged = dedup_features_keep_last(out);
        assert_eq!(pairs(&merged), vec![(*b"c2sc", 1), (*b"smcp", 0)]);
    }
}
