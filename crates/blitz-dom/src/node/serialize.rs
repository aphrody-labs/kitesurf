//! Serialization of nodes to HTML, and of inline `<svg>` subtrees to SVG source for usvg.

#[cfg(feature = "svg")]
use std::collections::HashSet;

use html_escape::{encode_quoted_attribute_to_string, encode_text_to_string};
use markup5ever::{QualName, ns};
use style_traits::values::ToCss;

use super::{Node, NodeData};

#[derive(Clone, Copy)]
enum OutputStyle {
    Normal,
    Pretty,
}

#[derive(Clone, Copy)]
struct SerializeOpts {
    style: OutputStyle,
    /// Replace `currentColor` in attribute values with the element's computed `color`
    resolve_current_color: bool,
    /// Declare the `xlink` prefix on the root element so that the output is well-formed XML
    declare_xlink_ns: bool,
}

struct SnapshotStamp {
    next_index: u32,
    prefix: u32,
}

impl SerializeOpts {
    const fn html(style: OutputStyle) -> Self {
        Self {
            style,
            resolve_current_color: false,
            declare_xlink_ns: false,
        }
    }
}

const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

const SVG_SNAPSHOT_ID_SEED: &str = "ks-svg-internal-";

fn svg_snapshot_id(index: u32, prefix: u32) -> String {
    format!("{SVG_SNAPSHOT_ID_SEED}{prefix}-{index}")
}

/// Writes an attribute's serialized name, per the HTML fragment serialization algorithm
fn write_attr_name(name: &QualName, writer: &mut String) {
    match name.ns {
        ns!() | ns!(html) => {}
        ns!(xml) => writer.push_str("xml:"),
        ns!(xmlns) if name.local.as_str() == "xmlns" => {}
        ns!(xmlns) => writer.push_str("xmlns:"),
        ns!(xlink) => writer.push_str("xlink:"),
        _ => {
            if let Some(prefix) = &name.prefix {
                writer.push_str(prefix);
                writer.push(':');
            }
        }
    }
    writer.push_str(&name.local);
}

fn is_xlink_ns_decl(name: &QualName) -> bool {
    (name.ns == ns!(xmlns) && name.local.as_str() == "xlink")
        || name.local.as_str() == "xmlns:xlink"
}

impl Node {
    /// Renders the HTML of this node and all its children as a `String` without extra whitespace.
    ///
    /// Example output:
    ///
    /// ```text
    /// <html><head /><body><main id="main"><div class="arbitrary-class" /></main></body></html>
    /// ```
    pub fn outer_html(&self) -> String {
        let mut output = String::new();
        self.write_outer_html(&mut output);
        output
    }

    /// Renders the HTML of this node and all its children as a `String` with whitespace for human
    /// readability.
    ///
    /// Example output:
    ///
    /// ```text
    /// <html>
    ///   <head />
    ///   <body>
    ///     <main id="main">
    ///       <div class="arbitrary-class" />
    ///     </main>
    ///   </body>
    /// </html>
    /// ```
    pub fn outer_html_pretty(&self) -> String {
        let mut output = String::new();
        self.write_outer_html_pretty(&mut output);
        output
    }

    pub fn write_outer_html(&self, writer: &mut String) {
        self.write_outer_html_in_style(
            writer,
            SerializeOpts::html(OutputStyle::Normal),
            0,
            None,
        );
    }

    pub fn write_outer_html_pretty(&self, writer: &mut String) {
        self.write_outer_html_in_style(
            writer,
            SerializeOpts::html(OutputStyle::Pretty),
            0,
            None,
        );
    }

    /// Serializes inline SVG for usvg, baking computed `currentColor` and stamping
    /// id-less descendants. It returns the source-unique prefix for geometry
    /// lookups, which the caller stores with the parsed snapshot.
    #[cfg(feature = "svg")]
    pub(crate) fn svg_source(&self) -> (String, u32) {
        let mut output = String::new();
        let opts = SerializeOpts {
            style: OutputStyle::Normal,
            resolve_current_color: true,
            declare_xlink_ns: true,
        };
        let prefix = self.choose_svg_snapshot_prefix();
        let mut stamp = SnapshotStamp {
            next_index: 0,
            prefix,
        };
        self.write_outer_html_in_style(&mut output, opts, 0, Some(&mut stamp));
        (output, prefix)
    }

    // Search all serialized text to keep literal fragment references from
    // binding to a synthetic ID. Serializing never creates the seed, so a subtree
    // whose names, values and text lack it takes the first prefix unserialized.
    #[cfg(feature = "svg")]
    fn choose_svg_snapshot_prefix(&self) -> u32 {
        const SEED: &str = SVG_SNAPSHOT_ID_SEED;
        if !self.subtree_contains(SEED) {
            return 0;
        }
        let opts = SerializeOpts {
            style: OutputStyle::Normal,
            resolve_current_color: true,
            declare_xlink_ns: true,
        };
        let mut source = String::new();
        self.write_outer_html_in_style(&mut source, opts, 0, None);

        let mut occupied = HashSet::new();
        for (start, _) in source.match_indices(SEED) {
            let suffix = &source[start + SEED.len()..];
            let digits = suffix.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 && suffix.as_bytes().get(digits) == Some(&b'-') {
                if let Ok(value) = suffix[..digits].parse::<u32>() {
                    occupied.insert(value);
                }
            }
        }
        (0u32..)
            .find(|value| !occupied.contains(value))
            .expect("finite SVG source cannot contain every snapshot prefix")
    }

    #[cfg(feature = "svg")]
    fn subtree_contains(&self, needle: &str) -> bool {
        let found = match &self.data {
            NodeData::Element(element) | NodeData::AnonymousBlock(element) => {
                element.name.local.contains(needle)
                    || element.attrs().iter().any(|attr| {
                        attr.name.local.contains(needle) || attr.value.contains(needle)
                    })
            }
            NodeData::Text(text) => text.content.contains(needle),
            NodeData::Comment { contents } => contents.contains(needle),
            _ => false,
        };
        found
            || self
                .children
                .iter()
                .any(|&child_id| self.tree()[child_id].subtree_contains(needle))
    }

    /// Return the inline snapshot ID for a descendant, or `None` when it is outside this SVG.
    #[cfg(feature = "svg")]
    pub fn svg_snapshot_id(&self, target: crate::NodeId) -> Option<String> {
        let index = self.svg_snapshot_index(target)?;
        let prefix = self.element_data()?.svg_image_data()?.snapshot_prefix?;
        Some(svg_snapshot_id(index, prefix))
    }

    /// Return the 1-based pre-order index used for `target` in `svg_source`.
    /// Return `None` when `target` is not a descendant element.
    pub fn svg_snapshot_index(&self, target: crate::NodeId) -> Option<u32> {
        fn walk(node: &Node, target: crate::NodeId, next: &mut u32) -> Option<u32> {
            for &child_id in &node.children {
                let child = &node.tree()[child_id];
                if !matches!(child.data, NodeData::Element(_)) {
                    continue;
                }
                *next += 1;
                if child_id == target {
                    return Some(*next);
                }
                if let Some(found) = walk(child, target, next) {
                    return Some(found);
                }
            }
            None
        }
        let mut next = 0u32;
        walk(self, target, &mut next)
    }

    // Serialize without baking currentColor, for subtrees that
    // get re-parented under a different color context (sprite <symbol>s).
    pub fn write_outer_html_verbatim(&self, writer: &mut String) {
        let opts = SerializeOpts {
            style: OutputStyle::Normal,
            resolve_current_color: false,
            declare_xlink_ns: false,
        };
        self.write_outer_html_in_style(writer, opts, 0, None);
    }

    fn write_outer_html_in_style(
        &self,
        writer: &mut String,
        opts: SerializeOpts,
        nesting: usize,
        mut stamp: Option<&mut SnapshotStamp>,
    ) {
        const INDENT: &str = "  ";
        let style = opts.style;
        let has_children = !self.children.is_empty();
        let current_color = opts
            .resolve_current_color
            .then(|| self.primary_styles())
            .flatten()
            .map(|style| style.clone_color())
            .map(|color| color.to_css_string());

        match &self.data {
            NodeData::Document(_) => {}
            NodeData::Comment { .. } => {}
            NodeData::AnonymousBlock(_) => {}
            NodeData::ShadowRoot(_) => {}
            // NodeData::Doctype { name, .. } => write!(s, "DOCTYPE {name}"),
            NodeData::Text(data) => {
                if matches!(style, OutputStyle::Pretty) {
                    for _ in 0..nesting {
                        writer.push_str(INDENT);
                    }
                }
                let in_raw_text_element = self
                    .parent
                    .and_then(|id| self.tree()[id].data.downcast_element())
                    .is_some_and(|el| {
                        // Documents are parsed with scripting disabled, so `<noscript>` is not raw text
                        el.name.ns == ns!(html)
                            && matches!(
                                el.name.local.as_ref(),
                                "style"
                                    | "script"
                                    | "xmp"
                                    | "iframe"
                                    | "noembed"
                                    | "noframes"
                                    | "plaintext"
                            )
                    });
                if in_raw_text_element {
                    writer.push_str(data.content.as_str());
                } else {
                    encode_text_to_string(data.content.as_str(), writer);
                }
                if matches!(style, OutputStyle::Pretty) {
                    writer.push('\n');
                }
            }
            NodeData::Element(data) => {
                if matches!(style, OutputStyle::Pretty) {
                    for _ in 0..nesting {
                        writer.push_str(INDENT);
                    }
                }
                writer.push('<');
                writer.push_str(&data.name.local);

                if opts.declare_xlink_ns
                    && nesting == 0
                    && !data.attrs().iter().any(|attr| is_xlink_ns_decl(&attr.name))
                {
                    writer.push_str(" xmlns:xlink=\"");
                    writer.push_str(XLINK_NS);
                    writer.push('"');
                }

                // Svg text reaches usvg as a serialized snapshot, so
                // CSS-driven font styling must be baked as presentation attrs.
                // Computed values already include font attrs via the pres-hint
                // path, so stamping supersedes the originals losslessly.
                // font-size only: it is the one font property whose CSS
                // value changes svg text geometry, and usvg resolves the
                // rest acceptably on its own. Stamping family/weight/style
                // too fed usvg a CSS-serialized family list it resolved
                // differently from parley (Ahem became no glyphs at all,
                // blanking a clipPath built from <text>).
                let mut stamps: Vec<(&str, String)> = Vec::new();
                // usvg reads mix-blend-mode only from a `style` attribute.
                let mut style_stamp: Option<String> = None;
                let styles = opts.resolve_current_color.then(|| self.primary_styles()).flatten();
                if let Some(style) = &styles {
                    if matches!(&*data.name.local, "text" | "tspan" | "textPath") {
                        stamps.push((
                            "font-size",
                            format!("{}", style.clone_font_size().used_size().px()),
                        ));
                    }
                    // usvg reads only the snapshot, so an author rule's opacity or
                    // blend mode never reached it. Stamp them when they differ from
                    // the initial value, which leaves a lone attribute as it is.
                    // The root's own opacity and blend are painted with its CSS box.
                    if nesting > 0 {
                        let opacity = style.clone_opacity();
                        if opacity != 1.0 {
                            stamps.push(("opacity", format!("{opacity}")));
                        }
                        let blend = style.clone_mix_blend_mode();
                        if blend != style::computed_values::mix_blend_mode::T::Normal {
                            style_stamp = Some(format!("mix-blend-mode:{}", blend.to_css_string()));
                        }
                    }
                }
                drop(styles);

                // The snapshot root keeps its own open tag (the
                // caller pattern-matches on it); every descendant element
                // takes the next pre-order index, and the ones without a
                // usable id get it stamped. An empty `id=""` counts as
                // absent (usvg ignores it) and is dropped rather than
                // emitted next to the stamp, which would be a duplicate
                // attribute and fail the whole XML parse.
                let stamp_id = match stamp.as_deref_mut() {
                    Some(stamp) if nesting > 0 => {
                        stamp.next_index += 1;
                        let has_id = data
                            .attrs()
                            .iter()
                            .any(|a| &*a.name.local == "id" && !a.value.is_empty());
                        (!has_id).then_some((stamp.next_index, stamp.prefix))
                    }
                    _ => None,
                };
                for attr in data.attrs() {
                    if stamps.iter().any(|(name, _)| *name == &*attr.name.local) {
                        continue;
                    }
                    if stamp_id.is_some() && &*attr.name.local == "id" {
                        continue;
                    }
                    writer.push(' ');
                    write_attr_name(&attr.name, writer);
                    writer.push_str("=\"");
                    let mut value = std::borrow::Cow::Borrowed(attr.value.as_str());
                    if let Some(color) = &current_color {
                        if value.contains("currentColor") {
                            value = std::borrow::Cow::Owned(value.replace("currentColor", color));
                        }
                    }
                    if attr.name.ns == ns!() && &*attr.name.local == "style" {
                        if let Some(stamp) = style_stamp.take() {
                            value = std::borrow::Cow::Owned(format!("{value};{stamp}"));
                        }
                    }
                    encode_quoted_attribute_to_string(&value, writer);
                    writer.push('"');
                }
                if let Some(stamp) = style_stamp.take() {
                    stamps.push(("style", stamp));
                }
                for (name, value) in &stamps {
                    writer.push(' ');
                    writer.push_str(name);
                    writer.push_str("=\"");
                    encode_quoted_attribute_to_string(value, writer);
                    writer.push('"');
                }
                if let Some((index, prefix)) = stamp_id {
                    writer.push_str(" id=\"");
                    writer.push_str(&svg_snapshot_id(index, prefix));
                    writer.push('"');
                }
                if !has_children {
                    writer.push_str(" /");
                }
                writer.push('>');
                if matches!(style, OutputStyle::Pretty) {
                    writer.push('\n');
                }

                if has_children {
                    for &child_id in &self.children {
                        self.tree()[child_id].write_outer_html_in_style(
                            writer,
                            opts,
                            nesting + 1,
                            stamp.as_deref_mut(),
                        );
                    }

                    if matches!(style, OutputStyle::Pretty) {
                        for _ in 0..nesting {
                            writer.push_str(INDENT);
                        }
                    }
                    writer.push_str("</");
                    writer.push_str(&data.name.local);
                    writer.push('>');
                    if matches!(style, OutputStyle::Pretty) {
                        writer.push('\n');
                    }
                }
            }
        }
    }
}

#[cfg(all(test, feature = "svg"))]
mod tests {
    use crate::{Attribute, BaseDocument, DocumentConfig, qual_name};

    #[test]
    fn snapshot_ids_do_not_satisfy_literal_fragment_references() {
        let mut doc = BaseDocument::new(DocumentConfig::default());
        let root_id = doc.root_node().id;
        let mut mutator = doc.mutate();
        let svg = mutator.create_element(qual_name!("svg", svg), vec![]);
        let earlier_rect = mutator.create_element(qual_name!("rect", svg), vec![]);
        let reference = "ks-svg-internal-0-3";
        let use_element = mutator.create_element(
            qual_name!("use", svg),
            vec![Attribute {
                name: qual_name!("href"),
                value: format!("#{reference}"),
            }],
        );
        let target = mutator.create_element(qual_name!("rect", svg), vec![]);
        mutator.append_children(svg, &[earlier_rect, use_element, target]);
        mutator.append_children(root_id, &[svg]);
        drop(mutator);

        let host = doc.get_node(svg).expect("SVG host exists");
        let (source, prefix) = host.svg_source();
        let index = host
            .svg_snapshot_index(target)
            .expect("target is a descendant");
        let snapshot_id = super::svg_snapshot_id(index, prefix);

        assert!(source.contains(&format!("href=\"#{reference}\"")));
        assert!(!source.contains(&format!("id=\"{reference}\"")));
        assert!(source.contains(&format!("id=\"{snapshot_id}\"")));
        assert!(!snapshot_id.starts_with("ks-svg-internal-0-"));
    }

    #[test]
    fn snapshot_prefix_without_a_seed_in_the_source_is_the_first() {
        let mut doc = BaseDocument::new(DocumentConfig::default());
        let root_id = doc.root_node().id;
        let mut mutator = doc.mutate();
        let svg = mutator.create_element(qual_name!("svg", svg), vec![]);
        let rect = mutator.create_element(qual_name!("rect", svg), vec![]);
        mutator.append_children(svg, &[rect]);
        mutator.append_children(root_id, &[svg]);
        drop(mutator);

        let (_, prefix) = doc.get_node(svg).expect("SVG host exists").svg_source();
        assert_eq!(prefix, 0);
    }
}
