// Copyright 2019 the Resvg Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use svgtypes::{Length, LengthUnit as Unit};

use super::converter;
use super::svgtree::{AId, SvgNode};
use crate::Units;

#[inline(never)]
pub(crate) fn convert_length(
    length: Length,
    node: SvgNode,
    aid: AId,
    object_units: Units,
    state: &converter::State,
) -> f32 {
    let dpi = state.opt.dpi;
    let n = length.number as f32;
    match length.unit {
        Unit::None | Unit::Px => n,
        Unit::Em => n * resolve_font_size(node, state),
        Unit::Ex => n * resolve_font_size(node, state) / 2.0,
        Unit::In => n * dpi,
        Unit::Cm => n * dpi / 2.54,
        Unit::Mm => n * dpi / 25.4,
        Unit::Pt => n * dpi / 72.0,
        Unit::Pc => n * dpi / 6.0,
        Unit::Percent => {
            if object_units == Units::ObjectBoundingBox {
                n / 100.0
            } else {
                let view_box = state.view_box;

                match aid {
                    AId::Cx
                    | AId::Dx
                    | AId::Fx
                    | AId::MarkerWidth
                    | AId::RefX
                    | AId::Rx
                    | AId::Width
                    | AId::X
                    | AId::X1
                    | AId::X2 => convert_percent(length, view_box.width()),
                    AId::Cy
                    | AId::Dy
                    | AId::Fy
                    | AId::Height
                    | AId::MarkerHeight
                    | AId::RefY
                    | AId::Ry
                    | AId::Y
                    | AId::Y1
                    | AId::Y2 => convert_percent(length, view_box.height()),
                    _ => {
                        let mut vb_len = view_box.width().powi(2) + view_box.height().powi(2);
                        vb_len = (vb_len / 2.0).sqrt();
                        convert_percent(length, vb_len)
                    }
                }
            }
        }
    }
}

pub(crate) fn convert_user_length(
    length: Length,
    node: SvgNode,
    aid: AId,
    state: &converter::State,
) -> f32 {
    convert_length(length, node, aid, Units::UserSpaceOnUse, state)
}

#[inline(never)]
pub(crate) fn convert_list(node: SvgNode, aid: AId, state: &converter::State) -> Option<Vec<f32>> {
    if let Some(text) = node.attribute::<&str>(aid) {
        let mut num_list = Vec::new();
        for length in svgtypes::LengthListParser::from(text).flatten() {
            num_list.push(convert_user_length(length, node, aid, state));
        }

        Some(num_list)
    } else {
        None
    }
}

// SVG2 geometry values accept CSS <length-percentage>, which includes
// calc(). svgtypes has no calc support, so the typed attribute parse
// fails and the value is silently dropped. This resolves the linear
// subset eagerly: +/- chains of dimension terms and products of a
// dimension with plain numbers. Each dimension resolves through
// convert_length, so %, em, ex and the per-attribute viewport-axis
// rules stay in one place. Anything non-linear (dimension * dimension,
// division by a dimension, nested parens, var()) returns None and the
// caller drops the value as before.
pub(crate) fn convert_calc(
    raw: &str,
    node: SvgNode,
    aid: AId,
    object_units: Units,
    state: &converter::State,
) -> Option<f32> {
    let t = raw.trim();
    if t.len() < 6 || !t[..5].eq_ignore_ascii_case("calc(") || !t.ends_with(')') {
        return None;
    }
    let inner = &t[5..t.len() - 1];

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Tok {
        Num(f64),
        Dim(f64, Unit),
        Add,
        Sub,
        Mul,
        Div,
    }

    // Lexer. A '-' or '+' glued to a digit is a sign; CSS requires
    // whitespace around additive operators, which disambiguates.
    let mut toks: Vec<Tok> = Vec::new();
    let b = inner.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as char;
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == '*' {
            toks.push(Tok::Mul);
            i += 1;
        } else if c == '/' {
            toks.push(Tok::Div);
            i += 1;
        } else if (c == '+' || c == '-')
            && !(i + 1 < b.len()
                && (b[i + 1] as char).is_ascii_digit()
                && !matches!(toks.last(), Some(Tok::Num(_) | Tok::Dim(..))))
        {
            toks.push(if c == '+' { Tok::Add } else { Tok::Sub });
            i += 1;
        } else if c.is_ascii_digit() || c == '.' || c == '+' || c == '-' {
            let start = i;
            i += 1; // consume sign or first digit
            while i < b.len() && ((b[i] as char).is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            let num: f64 = inner[start..i].parse().ok()?;
            let ustart = i;
            while i < b.len() && ((b[i] as char).is_ascii_alphabetic() || b[i] == b'%') {
                i += 1;
            }
            let unit = match &inner[ustart..i] {
                "" => None,
                unit if unit.eq_ignore_ascii_case("px") => Some(Unit::Px),
                unit if unit.eq_ignore_ascii_case("em") => Some(Unit::Em),
                unit if unit.eq_ignore_ascii_case("ex") => Some(Unit::Ex),
                unit if unit.eq_ignore_ascii_case("in") => Some(Unit::In),
                unit if unit.eq_ignore_ascii_case("cm") => Some(Unit::Cm),
                unit if unit.eq_ignore_ascii_case("mm") => Some(Unit::Mm),
                unit if unit.eq_ignore_ascii_case("pt") => Some(Unit::Pt),
                unit if unit.eq_ignore_ascii_case("pc") => Some(Unit::Pc),
                "%" => Some(Unit::Percent),
                _ => return None,
            };
            toks.push(match unit {
                Some(u) => Tok::Dim(num, u),
                None => Tok::Num(num),
            });
        } else {
            return None; // parens, var(), anything else: not our subset
        }
    }

    // Value domain: Scalar (bare <number>) vs Px (resolved length).
    // A length-typed attribute must come out as Px overall.
    #[derive(Clone, Copy)]
    enum Val {
        Scalar(f64),
        Px(f64),
    }
    let resolve = |n: f64, u: Unit| -> f64 {
        convert_length(Length::new(n, u), node, aid, object_units, state) as f64
    };

    let mut idx = 0;
    let atom = |idx: &mut usize, toks: &[Tok]| -> Option<Val> {
        match toks.get(*idx)? {
            Tok::Num(n) => {
                *idx += 1;
                Some(Val::Scalar(*n))
            }
            Tok::Dim(n, u) => {
                *idx += 1;
                Some(Val::Px(resolve(*n, *u)))
            }
            _ => None,
        }
    };
    let product = |idx: &mut usize, toks: &[Tok]| -> Option<Val> {
        let mut acc = atom(idx, toks)?;
        while let Some(op) = toks.get(*idx) {
            match op {
                Tok::Mul | Tok::Div => {
                    let is_div = matches!(op, Tok::Div);
                    *idx += 1;
                    let rhs = atom(idx, toks)?;
                    acc = match (acc, rhs, is_div) {
                        (Val::Scalar(a), Val::Scalar(c), false) => Val::Scalar(a * c),
                        (Val::Scalar(a), Val::Scalar(c), true) if c != 0.0 => Val::Scalar(a / c),
                        (Val::Px(a), Val::Scalar(c), false) => Val::Px(a * c),
                        (Val::Px(a), Val::Scalar(c), true) if c != 0.0 => Val::Px(a / c),
                        (Val::Scalar(a), Val::Px(c), false) => Val::Px(a * c),
                        _ => return None, // px*px, x/px, div-by-zero
                    };
                }
                _ => break,
            }
        }
        Some(acc)
    };

    let mut acc = match product(&mut idx, &toks)? {
        Val::Px(v) => v,
        Val::Scalar(_) => return None, // a bare <number> is not a length
    };
    while idx < toks.len() {
        let sub = match toks[idx] {
            Tok::Add => false,
            Tok::Sub => true,
            _ => return None,
        };
        idx += 1;
        let rhs = match product(&mut idx, &toks)? {
            Val::Px(v) => v,
            Val::Scalar(_) => return None,
        };
        acc = if sub { acc - rhs } else { acc + rhs };
    }
    let out = acc as f32;
    out.is_finite().then_some(out)
}

fn convert_percent(length: Length, base: f32) -> f32 {
    base * (length.number as f32) / 100.0
}

#[inline(never)]
pub(crate) fn resolve_font_size(node: SvgNode, state: &converter::State) -> f32 {
    let nodes: Vec<_> = node.ancestors().collect();
    let mut font_size = state.opt.font_size;
    for n in nodes.iter().rev().skip(1) {
        // skip Root
        if let Some(length) = n.try_attribute::<Length>(AId::FontSize) {
            let dpi = state.opt.dpi;
            let n = length.number as f32;
            font_size = match length.unit {
                Unit::None | Unit::Px => n,
                Unit::Em => n * font_size,
                Unit::Ex => n * font_size / 2.0,
                Unit::In => n * dpi,
                Unit::Cm => n * dpi / 2.54,
                Unit::Mm => n * dpi / 25.4,
                Unit::Pt => n * dpi / 72.0,
                Unit::Pc => n * dpi / 6.0,
                Unit::Percent => {
                    // If `font-size` has percent units that it's value
                    // is relative to the parent node `font-size`.
                    length.number as f32 * font_size * 0.01
                }
            }
        } else if let Some(name) = n.attribute(AId::FontSize) {
            font_size = convert_named_font_size(name, font_size);
        }
    }

    font_size
}

fn convert_named_font_size(name: &str, parent_font_size: f32) -> f32 {
    let factor = match name {
        "xx-small" => -3,
        "x-small" => -2,
        "small" => -1,
        "medium" => 0,
        "large" => 1,
        "x-large" => 2,
        "xx-large" => 3,
        "smaller" => -1,
        "larger" => 1,
        _ => {
            log::warn!("Invalid 'font-size' value: '{}'.", name);
            0
        }
    };

    // 'On a computer screen a scaling factor of 1.2 is suggested between adjacent indexes.'
    parent_font_size * 1.2f32.powi(factor)
}
