//! Legacy vendor-pseudo compat rewrites applied to CSS text
//! before stylo parses it.
//!
//! The servo stylo build can't represent `::-webkit-slider-thumb` /
//! `::-moz-range-thumb` selectors (unknown pseudo-element → the whole
//! rule is dropped), but that's how the pre-`accent-color` world,
//! Bootstrap included, themes range sliders. The UA slider paint
//! reads the computed `accent-color`, so this lifts the thumb
//! rule's background color into a synthesized `accent-color` rule on
//! the pseudo-less selector and append it to the sheet. Selector
//! matching, specificity and media scoping (minus the appended rules
//! escaping any enclosing @media; acceptable: these rules virtually
//! never sit inside one) then behave naturally.
//!
//! The scanner is deliberately crude: rule bodies in CSS are flat
//! (grouping at-rules like @media nest selectors, not declarations),
//! so walking `selector { body }` segments and treating `@…{` as a
//! grouping-open covers real-world sheets. Brace characters inside
//! `content: "…"` strings could confuse it in principle; thumb rules
//! don't carry `content`, and a mis-parse only means a skipped
//! rewrite, never a corrupted sheet (we only append).

const THUMB_PSEUDOS: [&str; 2] = ["::-webkit-slider-thumb", "::-moz-range-thumb"];

/// Returns `None` when the sheet needs no rewrite (fast path: one
/// substring scan), otherwise the original CSS with synthesized
/// `accent-color` rules appended.
pub fn rewrite_legacy_widget_pseudo_rules(css: &str) -> Option<String> {
    if !THUMB_PSEUDOS.iter().any(|p| css.contains(p)) {
        return None;
    }

    let mut extra = String::new();
    let bytes = css.as_bytes();
    let mut seg_start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                let selector = css[seg_start..i].trim();
                if selector.starts_with('@') {
                    // Grouping at-rule (@media/@supports/@layer): the
                    // brace opens a rule list, not a declaration body.
                    // Statement at-rules (@import/@charset) end with
                    // ';' and never reach here.
                    seg_start = i + 1;
                    i += 1;
                    continue;
                }
                // Declaration body: flat; scan to the closing brace.
                let body_start = i + 1;
                let Some(rel_end) = css[body_start..].find('}') else {
                    break;
                };
                let body = &css[body_start..body_start + rel_end];
                process_rule(selector, body, &mut extra);
                i = body_start + rel_end + 1;
                seg_start = i;
            }
            b'}' => {
                // Closing a grouping at-rule.
                seg_start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }

    if extra.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(css.len() + extra.len());
    out.push_str(css);
    out.push_str(&extra);
    Some(out)
}

fn process_rule(selector_list: &str, body: &str, extra: &mut String) {
    // Collect the selectors that target a thumb pseudo, stripped of
    // the pseudo itself. `.form-range:focus::-webkit-slider-thumb`
    // keeps its `:focus`; the cascade sorts that out.
    let mut stripped: Vec<String> = Vec::new();
    for sel in selector_list.split(',') {
        let sel = sel.trim();
        for pseudo in THUMB_PSEUDOS {
            if let Some(pos) = sel.find(pseudo) {
                let mut s = String::with_capacity(sel.len());
                s.push_str(&sel[..pos]);
                s.push_str(&sel[pos + pseudo.len()..]);
                let s = s.trim().to_string();
                if !s.is_empty() {
                    stripped.push(s);
                }
                break;
            }
        }
    }
    if stripped.is_empty() {
        return;
    }

    let Some(color) = extract_background_color(body) else {
        return;
    };

    extra.push_str("\n");
    extra.push_str(&stripped.join(", "));
    extra.push_str(" { accent-color: ");
    extra.push_str(color);
    extra.push_str("; }");
}

/// Pull a usable color out of the rule body: `background-color: X`
/// wins; a `background: X` shorthand is accepted only when its value
/// is a single token (no url()/gradients/position syntax).
fn extract_background_color(body: &str) -> Option<&str> {
    let mut shorthand: Option<&str> = None;
    for decl in body.split(';') {
        let Some((prop, value)) = decl.split_once(':') else {
            continue;
        };
        let prop = prop.trim();
        let value = value.trim().trim_end_matches("!important").trim();
        if value.is_empty() {
            continue;
        }
        if prop.eq_ignore_ascii_case("background-color") {
            return Some(value);
        }
        if prop.eq_ignore_ascii_case("background")
            && !value.contains("url(")
            && !value.contains("gradient")
            && !value.contains(' ')
        {
            shorthand = Some(value);
        }
    }
    shorthand
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_bootstrap_thumb_rule() {
        let css = ".form-range::-webkit-slider-thumb{width:1rem;background-color:navy;border:0}";
        let out = rewrite_legacy_widget_pseudo_rules(css).unwrap();
        assert!(out.contains(".form-range { accent-color: navy; }"));
        assert!(out.starts_with(css));
    }

    #[test]
    fn keeps_focus_variant_and_skips_no_color() {
        let css = ".form-range:focus::-webkit-slider-thumb{box-shadow:0 0 0 1px #fff}";
        assert!(rewrite_legacy_widget_pseudo_rules(css).is_none());
    }

    #[test]
    fn handles_moz_and_media_nesting() {
        let css = "@media (min-width: 600px) { .r::-moz-range-thumb { background: #123456 } }";
        let out = rewrite_legacy_widget_pseudo_rules(css).unwrap();
        assert!(out.contains(".r { accent-color: #123456; }"));
    }

    #[test]
    fn fast_path_no_pseudo() {
        assert!(rewrite_legacy_widget_pseudo_rules("body{color:red}").is_none());
    }

    #[test]
    fn important_is_stripped_from_value() {
        let css = ".r::-webkit-slider-thumb{background-color: rebeccapurple !important;}";
        let out = rewrite_legacy_widget_pseudo_rules(css).unwrap();
        assert!(out.contains(".r { accent-color: rebeccapurple; }"));
    }
}
