use blitz_traits::node_id::NodeId;
use core::str;
use std::sync::Arc;

use markup5ever::{QualName, local_name, ns};
use parley::{
    BaseDirection, FontContext, InlineBox, InlineBoxKind, LayoutContext, StyleProperty,
    TreeBuilder, WhiteSpaceCollapse,
};
use style::{
    computed_values::position::T as PositionProperty,
    data::ElementData as StyloElementData,
    shared_lock::StylesheetGuards,
    values::{
        computed::{Content, ContentItem, Display, Float},
        specified::box_::{DisplayInside, DisplayOutside},
    },
};
use thin_vec::ThinVec;

use crate::{
    BaseDocument, ElementData, Node, NodeData,
    layout::damage::{
        CONSTRUCT_BOX, CONSTRUCT_DESCENDENT, CONSTRUCT_FC, sort_layout_children_by_order,
    },
    node::{
        ListItemLayout, ListItemLayoutPosition, Marker, NodeFlags, NodeKind, SpecialElementData,
        TextBrush, TextInputData, TextLayout,
    },
    qual_name, stylo_to_parley,
    traversal::{iter_flat_children, iter_flat_children_and_pseudos},
};

use super::{
    damage::ALL_DAMAGE,
    list::{BULLET_FONT_FAMILY, collect_list_item_children},
    replaced::is_inline_box_element,
    table::build_table_context,
    text_transform::{CaseTransform, TextTransformer},
};

const DUMMY_NAME: QualName = qual_name!("div", html);

// Colors stamped onto serialized SVG must be sRGB-legacy rgb()/rgba():
// usvg's svgtypes parser rejects modern forms like oklch() / color().
#[cfg(feature = "svg")]
fn srgb_color_attr(c: &style::color::AbsoluteColor) -> String {
    use crate::util::ToColorColor as _;
    let [r, g, b, a] = c.as_color_color().components;
    let (r, g, b) = (
        (r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (b.clamp(0.0, 1.0) * 255.0).round() as u8,
    );
    // Clamp alpha too: an out-of-range (or NaN) value formats into an
    // rgba() svgtypes rejects, which silently reverts the paint to black.
    let a = if a.is_nan() { 1.0 } else { a.clamp(0.0, 1.0) };
    if a >= 1.0 {
        format!("rgb({r},{g},{b})")
    } else {
        format!("rgba({r},{g},{b},{a})")
    }
}

// Presentation attributes usvg reads as paint. Their values go through the
// CSS parser here, so `var()` and modern colour syntax resolve.
#[cfg(feature = "svg")]
const SVG_COLOR_ATTRS: [&str; 5] = [
    "fill",
    "stroke",
    "stop-color",
    "flood-color",
    "lighting-color",
];

/// Does this attribute value need the CSS engine, i.e. is it something
/// svgtypes will reject or mis-parse on its own?
///
/// Property-aware: paints care about CSS-only colour syntax, lengths care
/// about CSS-only units. `font-size="0.6875rem"` carries no function call,
/// and svgtypes reads it as the bare number 0.6875 (the very case this
/// path exists for), so the length arm keys off the unit instead.
#[cfg(feature = "svg")]
fn needs_css_resolution(property: &str, value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return false;
    }
    if v.contains("var(") {
        return true;
    }
    if property == "font-size" {
        // Strip the number off the front; whatever trails is the unit.
        let unit = v.trim_start_matches(|c: char| {
            c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c.is_whitespace()
        });
        return matches!(
            unit.trim().to_ascii_lowercase().as_str(),
            "rem" | "em" | "ex" | "ch" | "vw" | "vh" | "vmin" | "vmax" | "cap" | "ic" | "lh" | "rlh"
        ) || v.contains("calc(")
            || v.contains("clamp(")
            || v.contains("min(")
            || v.contains("max(");
    }
    // Paint keywords and references are already what usvg wants.
    if v == "none" || v == "currentColor" || v.starts_with("url(") {
        return false;
    }
    v.contains("color-mix(")
        || v.contains("light-dark(")
        || v.contains("oklch(")
        || v.contains("oklab(")
        || v.contains("lch(")
        || v.contains("lab(")
        || v.contains("color(")
}

/// Compute one declaration against the `<svg>` element's own style and
/// hand back a value usvg accepts.
///
/// Inner SVG elements are never cascaded (they have no boxes), so a
/// `fill="var(--token)"` reached usvg verbatim: svgtypes rejected it and the
/// paint silently reverted to black: or, for `stroke`, vanished. Running the
/// value through the real cascade with the svg element as parent resolves
/// custom properties, color-mix(), light-dark() and rem units alike.
#[cfg(feature = "svg")]
fn resolve_css_attr_value(
    doc: &BaseDocument,
    parent_style: &style::properties::ComputedValues,
    property: &str,
    value: &str,
) -> Option<String> {
    use style::properties::{parse_style_attribute, PropertyDeclarationBlock};
    use style::shared_lock::StylesheetGuards;
    use style::stylesheets::CssRuleType;

    let block: PropertyDeclarationBlock = parse_style_attribute(
        &format!("{property}:{value}"),
        &doc.url.url_extra_data(),
        None,
        style::context::QuirksMode::NoQuirks,
        CssRuleType::Style,
    );
    // An unparseable value leaves the block empty; computing it would answer
    // with the inherited colour, which is worse than leaving the attr alone.
    if block.declarations().is_empty() {
        return None;
    }
    let locked = style::servo_arc::Arc::new(doc.guard.wrap(block));
    let read_guard = doc.guard.read();
    let guards = StylesheetGuards::same(&read_guard);
    let computed = doc
        .stylist
        .compute_for_declarations::<&Node>(&guards, parent_style, locked);
    match property {
        "font-size" => Some(format!("{}", computed.clone_font_size().used_size().px())),
        // Paints go through the paint grammar, not `color`: a var() that
        // expands to `none` or `url(#grad)` is invalid for `color` and would
        // compute to the inherited colour, stamping a solid fill where the
        // author asked for no fill or a gradient.

        // `stroke` is a gecko-only longhand in this build, so it borrows
        // `fill`'s grammar: both are <paint>, so the parse is identical.
        "fill" => paint_attr(&computed.clone_fill().kind, &computed),
        _ => Some(srgb_color_attr(&computed.clone_color())),
    }
}

/// Serialize a computed SVG paint back into an attribute usvg accepts.
#[cfg(feature = "svg")]
fn paint_attr(
    kind: &style::values::generics::svg::SVGPaintKind<
        style::values::computed::Color,
        style::values::computed::url::ComputedUrl,
    >,
    computed: &style::properties::ComputedValues,
) -> Option<String> {
    use style::values::generics::color::GenericColor;
    use style::values::generics::svg::SVGPaintKind;
    use style_traits::ToCss as _;
    match kind {
        SVGPaintKind::None => Some("none".to_string()),
        SVGPaintKind::ContextFill => None,
        SVGPaintKind::ContextStroke => None,
        // A paint-server ref keeps its url(): usvg resolves it against the
        // ids in this same serialized subtree, so hand back the fragment:
        // the computed url is absolute and svgtypes wants `url(#id)`.
        SVGPaintKind::PaintServer(url) => {
            let css = url.to_css_string();
            let inner = css
                .trim_start_matches("url(")
                .trim_end_matches(')')
                .trim_matches('"')
                .trim_matches('\'');
            inner
                .rfind('#')
                .map(|at| format!("url({})", &inner[at..]))
        }
        SVGPaintKind::Color(GenericColor::CurrentColor) => {
            Some(srgb_color_attr(&computed.clone_color()))
        }
        SVGPaintKind::Color(GenericColor::Absolute(abs)) => Some(srgb_color_attr(abs)),
        SVGPaintKind::Color(_) => None,
    }
}

/// Rewrite `attr="<css value>"` occurrences in the serialized subtree.
#[cfg(feature = "svg")]
fn resolve_attr_occurrences(
    doc: &BaseDocument,
    parent_style: &style::properties::ComputedValues,
    html: &mut String,
    attr: &str,
    property: &str,
) {
    let needle = format!(" {attr}=\"");
    let mut at = 0usize;
    while let Some(pos) = html[at..].find(&needle) {
        let start = at + pos + needle.len();
        let Some(len) = html[start..].find('"') else { break };
        let value = html[start..start + len].to_string();
        if needs_css_resolution(property, &value) {
            if let Some(resolved) = resolve_css_attr_value(doc, parent_style, property, &value) {
                html.replace_range(start..start + len, &resolved);
                at = start + resolved.len();
                continue;
            }
        }
        at = start + len;
    }
}

/// Resolve CSS-era values in a serialized SVG subtree before usvg sees it.
#[cfg(feature = "svg")]
fn resolve_css_values_in_svg(doc: &BaseDocument, svg_id: NodeId, html: &mut String) {
    if !html.contains("var(")
        && !html.contains("color-mix(")
        && !html.contains("light-dark(")
        && !html.contains("font-size=")
        && !html.contains("oklch(")
        && !html.contains("oklab(")
        && !html.contains("lch(")
        && !html.contains("lab(")
        && !html.contains("color(")
    {
        return;
    }
    let Some(parent_style) = doc.get_node(svg_id).and_then(|n| n.primary_styles()) else {
        return;
    };
    for attr in SVG_COLOR_ATTRS {
        // fill/stroke resolve through the paint grammar (both parse as
        // <paint>); the rest are plain colours, so `color` fits them.
        let property = if attr == "fill" || attr == "stroke" { "fill" } else { "color" };
        resolve_attr_occurrences(doc, &parent_style, html, attr, property);
    }
    // font-size carries CSS units svgtypes doesn't know (rem above all), so
    // it rides the same path and lands as a plain px number.
    resolve_attr_occurrences(doc, &parent_style, html, "font-size", "font-size");
}

// Elements whose fragment ref pulls another element into the render tree.
// `<a href="#x">` is a link, not a resource ref: it must not hoist.
#[cfg(feature = "svg")]
const FRAGMENT_REF_ELEMENTS: [&str; 4] = ["<use", "<image", "<feImage", "<feimage"];

// Fragment refs (`href="#id"` / `xlink:href="#id"`) in serializer output.
// Only ever run on write_outer_html output, so `href="#` is canonical.
#[cfg(feature = "svg")]
fn collect_fragment_refs(html: &str, out: &mut Vec<String>) {
    for tag in FRAGMENT_REF_ELEMENTS {
        let mut rest = html;
        while let Some(pos) = rest.find(tag) {
            rest = &rest[pos + tag.len()..];
            // A tag-name boundary must follow, else `<uses>`/`<imagexyz>`
            // (any element sharing the prefix) would match too.
            let at_boundary = match rest.chars().next() {
                Some(c) => c.is_whitespace() || c == '>' || c == '/',
                None => false,
            };
            // Scan the opening TAG only: a ref in a later sibling belongs
            // to that element, and gets its own iteration of this loop.
            let Some(tag_end) = rest.find('>') else { break };
            if at_boundary {
                if let Some(h) = rest[..tag_end].find("href=\"#") {
                    let after = &rest[h + 7..tag_end];
                    if let Some(end) = after.find('"') {
                        if end > 0 && end <= 256 {
                            out.push(after[..end].to_string());
                        }
                    }
                }
            }
            rest = &rest[tag_end..];
        }
    }
}

// Ids the serializer has already emitted. Indexed once per chunk so the
// hoist loop tests membership in O(1) instead of rescanning the buffer.
#[cfg(feature = "svg")]
fn collect_ids(html: &str, out: &mut std::collections::HashSet<String>) {
    let mut rest = html;
    while let Some(pos) = rest.find(" id=\"") {
        rest = &rest[pos + 5..];
        let Some(end) = rest.find('"') else { break };
        out.insert(rest[..end].to_string());
        rest = &rest[end..];
    }
}

// SVG sprite support: hoist same-document `<use>` targets that live
// outside this <svg> into an injected <defs> block before usvg parse.
#[cfg(feature = "svg")]
fn inline_external_fragment_refs(doc: &BaseDocument, outer_html: &mut String) {
    let mut queue = Vec::new();
    collect_fragment_refs(outer_html, &mut queue);
    if queue.is_empty() {
        return;
    }

    // Ids usvg will already see: the subtree's own, plus each hoisted
    // chunk's (a hoisted subtree can define ids nothing queued asked for).
    let mut present = std::collections::HashSet::new();
    collect_ids(outer_html, &mut present);

    let mut seen = std::collections::HashSet::new();
    let mut defs = String::new();
    while let Some(id) = queue.pop() {
        if defs.len() > 512 * 1024 {
            break; // runaway guard: pathological ref chains stop growing
        }
        if !seen.insert(id.clone()) {
            continue;
        }
        if present.contains(&id) {
            continue;
        }
        let Some(target_id) = doc.get_element_by_id(&id) else {
            continue;
        };
        let Some(node) = doc.get_node(target_id) else {
            continue;
        };
        // Verbatim (currentColor not baked): <use> content inherits color
        // from the referencing site, not the sprite's hidden context.
        let before = defs.len();
        node.write_outer_html_verbatim(&mut defs);
        collect_fragment_refs(&defs[before..], &mut queue);
        collect_ids(&defs[before..], &mut present);
    }
    if defs.is_empty() {
        return;
    }

    // Self-closing roots have no children hence no refs, so the first
    // `>` here is always the end of the root <svg>'s opening tag.
    if let Some(open_end) = outer_html.find('>') {
        outer_html.insert_str(open_end + 1, &format!("<defs>{defs}</defs>"));
    }
}

#[derive(Clone)]
pub(crate) struct ConstructionTask {
    pub(crate) node_id: NodeId,
    pub(crate) data: ConstructionTaskData,
}

pub(crate) struct ConstructionTaskResult {
    pub(crate) node_id: NodeId,
    pub(crate) data: ConstructionTaskResultData,
}

#[derive(Clone)]
pub(crate) enum ConstructionTaskData {
    InlineLayout(Box<TextLayout>),
}

pub(crate) enum ConstructionTaskResultData {
    InlineLayout(Box<TextLayout>),
}

/// Accumulator threaded through layout-child collection.
///
/// `children` is the list of layout children being built up, and
/// `anonymous_block_id` is the currently-open anonymous block container (if
/// any) that wrapping children are being appended to.
#[derive(Default)]
pub(crate) struct LayoutChildren {
    pub(crate) children: ThinVec<NodeId>,
    pub(crate) anonymous_block_id: Option<NodeId>,
    /// All anonymous blocks created while collecting these layout children.
    ///
    /// These are recorded on the container node so they can be deallocated the
    /// next time it is reconstructed.
    pub(crate) anonymous_blocks: ThinVec<NodeId>,
    /// Open anonymous `display: table` wrapper collecting a run of
    /// misparented table-internal children (CSS 2.2 §17.2.1 fixup).
    pub(crate) anonymous_table_id: Option<NodeId>,
}

// Table-internal display values that require a table ancestor; a run of
// them under a non-table parent gets an anonymous table wrapper.
pub(crate) fn is_table_internal(display: Display) -> bool {
    matches!(
        display.inside(),
        DisplayInside::TableCell
            | DisplayInside::TableRow
            | DisplayInside::TableRowGroup
            | DisplayInside::TableHeaderGroup
            | DisplayInside::TableFooterGroup
    )
}

impl LayoutChildren {
    /// Append a single layout child.
    fn push(&mut self, child_id: NodeId, doc: &mut BaseDocument) {
        self.maybe_push_anon_block(doc);
        self.anonymous_table_id = None;
        self.children.push(child_id);
    }

    /// Append all layout children in `slice`.
    fn extend(&mut self, slice: &[NodeId], doc: &mut BaseDocument) {
        self.maybe_push_anon_block(doc);
        self.anonymous_table_id = None;
        self.children.extend_from_slice(slice);
    }

    // Route a misparented table-internal child into the open anonymous
    // table, creating the wrapper at the run's first member.
    fn push_table_wrapped(
        &mut self,
        container_node_id: NodeId,
        child_id: NodeId,
        doc: &mut BaseDocument,
    ) {
        self.maybe_push_anon_block(doc);
        if self.anonymous_table_id.is_none() {
            self.create_anonymous_table(container_node_id, doc);
        }
        doc.nodes[self.anonymous_table_id.unwrap()]
            .children
            .push(child_id);
    }

    fn create_anonymous_table(&mut self, container_node_id: NodeId, doc: &mut BaseDocument) {
        use style::selector_parser::PseudoElement;

        const NAME: QualName = QualName {
            prefix: None,
            ns: ns!(html),
            local: local_name!("table"),
        };
        let node_id = doc.create_node(NodeData::AnonymousBlock(Box::new(ElementData::new(
            NAME,
            Vec::new(),
        ))));

        let parent_style = doc.nodes[container_node_id].primary_styles().unwrap();
        let read_guard = doc.guard.read();
        let guards = StylesheetGuards::same(&read_guard);
        // display:table comes from the UA sheet's ::-servo-anonymous-table
        // rule; everything else inherits from the real parent.
        let style = doc.stylist.style_for_anonymous::<&Node>(
            &guards,
            &PseudoElement::ServoAnonymousTable,
            &parent_style,
        );
        let mut stylo_element_data = StyloElementData {
            damage: ALL_DAMAGE,
            ..Default::default()
        };
        drop(parent_style);

        stylo_element_data.styles.primary = Some(style);
        stylo_element_data.set_restyled();
        *doc.nodes[node_id]
            .stylo_element_data_mut()
            .ensure_init_mut() = stylo_element_data;

        if doc.nodes[container_node_id]
            .flags
            .contains(NodeFlags::IS_IN_DOCUMENT)
        {
            doc.nodes[node_id].flags.insert(NodeFlags::IS_IN_DOCUMENT);
        }
        doc.nodes[node_id].parent = Some(container_node_id);
        doc.nodes[node_id]
            .layout_parent
            .set(Some(container_node_id));

        self.children.push(node_id);
        self.anonymous_blocks.push(node_id);
        self.anonymous_table_id = Some(node_id);
    }

    fn maybe_push_anon_block(&mut self, doc: &mut BaseDocument) {
        fn block_is_only_whitespace(doc: &BaseDocument, node_id: NodeId) -> bool {
            for child_id in doc.nodes[node_id].children.iter().copied() {
                let child = &doc.nodes[child_id];
                if !child.is_collapsible_whitespace_node() {
                    return false;
                }
            }

            true
        }

        // If anonymous block node only contains whitespace then delete it
        if let Some(anon_id) = self.anonymous_block_id {
            if block_is_only_whitespace(doc, anon_id) {
                // Remove by identity, not pop(): hoisted display:contents
                // children may have been pushed after the anon block.
                if let Some(pos) = self.children.iter().rposition(|id| *id == anon_id) {
                    self.children.remove(pos);
                }
                self.anonymous_blocks.retain(|id| *id != anon_id);
                doc.remove_node_from_tree(anon_id);
            }
        }

        self.anonymous_block_id = None;
    }

    fn push_wrapped(
        &mut self,
        container_node_id: NodeId,
        child_id: NodeId,
        doc: &mut BaseDocument,
    ) {
        self.anonymous_table_id = None;
        if self.anonymous_block_id.is_none() {
            self.create_anonymous_block(container_node_id, doc);
        }
        doc.nodes[self.anonymous_block_id.unwrap()]
            .children
            .push(child_id);
    }

    fn create_anonymous_block(&mut self, container_node_id: NodeId, doc: &mut BaseDocument) {
        use style::selector_parser::PseudoElement;

        const NAME: QualName = QualName {
            prefix: None,
            ns: ns!(html),
            local: local_name!("div"),
        };
        let node_id = doc.create_node(NodeData::AnonymousBlock(Box::new(ElementData::new(
            NAME,
            Vec::new(),
        ))));

        // Set style data
        let parent_style = doc.nodes[container_node_id].primary_styles().unwrap();
        let read_guard = doc.guard.read();
        let guards = StylesheetGuards::same(&read_guard);
        let style = doc.stylist.style_for_anonymous::<&Node>(
            &guards,
            &PseudoElement::ServoAnonymousBox,
            &parent_style,
        );
        let mut stylo_element_data = StyloElementData {
            damage: ALL_DAMAGE,
            ..Default::default()
        };
        drop(parent_style);

        stylo_element_data.styles.primary = Some(style);
        stylo_element_data.set_restyled();

        *doc.nodes[node_id]
            .stylo_element_data_mut()
            .ensure_init_mut() = stylo_element_data;

        if doc.nodes[container_node_id]
            .flags
            .contains(NodeFlags::IS_IN_DOCUMENT)
        {
            doc.nodes[node_id].flags.insert(NodeFlags::IS_IN_DOCUMENT);
        }
        doc.nodes[node_id].parent = Some(container_node_id);
        doc.nodes[node_id]
            .layout_parent
            .set(Some(container_node_id));

        self.children.push(node_id);
        self.anonymous_block_id = Some(node_id);
        self.anonymous_blocks.push(node_id);
    }
}

fn push_children_and_pseudos(layout_children: &mut ThinVec<NodeId>, node: &Node) {
    if let Some(before) = node.before() {
        layout_children.push(before);
    }
    layout_children.extend(node.flat_children().iter().copied().filter(|child_id| {
        let child_node = node.with(*child_id);
        child_node.data.kind() != NodeKind::Comment
    }));
    if let Some(after) = node.after() {
        layout_children.push(after);
    }
}

/// Wrapping policy of the nearest non-contents ancestor container. Children
/// hoisted out of display:contents nodes must be wrapped in anonymous blocks
/// exactly as if they were direct children of that container: without this,
/// a bare text node could end up as the layout child of a block/flex/grid
/// container, which cannot be laid out (text nodes carry no style).
#[derive(Copy, Clone)]
struct WrapContext {
    /// The container whose style anonymous blocks inherit from.
    container_node_id: NodeId,
    needs_wrap: fn(NodeKind, DisplayOutside) -> bool,
}

fn block_item_needs_wrap(child_node_kind: NodeKind, display_outside: DisplayOutside) -> bool {
    child_node_kind == NodeKind::Text || display_outside == DisplayOutside::Inline
}

/// Used for flex/grid containers, and for flow containers whose in-flow
/// children are all out-of-flow (where inline elements are pushed raw, but
/// bare text still cannot be laid out directly).
fn text_item_needs_wrap(child_node_kind: NodeKind, _display_outside: DisplayOutside) -> bool {
    child_node_kind == NodeKind::Text
}

/// Push a single hoisted child, recursing through display:contents nodes and
/// wrapping text/inline children per the ancestor container's `WrapContext`.
fn push_hoisted_child(
    doc: &mut BaseDocument,
    child_id: NodeId,
    out: &mut LayoutChildren,
    wrap: Option<WrapContext>,
) {
    let child = &doc.nodes[child_id];
    let child_display = child.display_style().unwrap_or(Display::inline());
    if matches!(child_display.inside(), DisplayInside::Contents) {
        collect_layout_children_with_wrap(doc, child_id, out, wrap);
        return;
    }
    if let Some(wrap_ctx) = wrap {
        let child_node_kind = child.data.kind();
        let display_outside = if child.is_or_contains_block() {
            DisplayOutside::Block
        } else {
            child_display.outside()
        };
        if (wrap_ctx.needs_wrap)(child_node_kind, display_outside) {
            out.push_wrapped(wrap_ctx.container_node_id, child_id, doc);
            return;
        }
    }
    out.push(child_id, doc);
}

/// Push the container's children (and ::before/::after pseudos) as layout
/// children, hoisting transparently through display:contents nodes and
/// filtering out comments and whitespace.
fn push_hoisted_children_and_pseudos(
    doc: &mut BaseDocument,
    container_node_id: NodeId,
    out: &mut LayoutChildren,
    wrap: Option<WrapContext>,
) {
    if let Some(before) = doc.nodes[container_node_id].before() {
        push_hoisted_child(doc, before, out, wrap);
    }
    // Take the flat child list to avoid borrow checker issues: hosts
    // hoist their shadow root's children, slots their assigned nodes.
    let flat_src = doc.nodes[container_node_id]
        .attached_shadow_root_id()
        .unwrap_or(container_node_id);
    let use_assigned = doc.nodes[flat_src]
        .assigned_nodes
        .as_ref()
        .is_some_and(|a| !a.is_empty());
    let children = if use_assigned {
        doc.nodes[flat_src].assigned_nodes.take().unwrap()
    } else {
        std::mem::take(&mut doc.nodes[flat_src].children)
    };
    for child_id in children.iter().copied() {
        let child = &doc.nodes[child_id];
        if child.data.kind() == NodeKind::Comment || child.is_collapsible_whitespace_node() {
            continue;
        }
        push_hoisted_child(doc, child_id, out, wrap);
    }
    if use_assigned {
        doc.nodes[flat_src].assigned_nodes = Some(children);
    } else {
        doc.nodes[flat_src].children = children;
    }
    if let Some(after) = doc.nodes[container_node_id].after() {
        push_hoisted_child(doc, after, out, wrap);
    }
}

fn push_non_whitespace_children_and_pseudos(layout_children: &mut ThinVec<NodeId>, node: &Node) {
    if let Some(before) = node.before() {
        layout_children.push(before);
    }
    layout_children.extend(node.flat_children().iter().copied().filter(|child_id| {
        let child_node = node.with(*child_id);
        !child_node.is_whitespace_node() && child_node.data.kind() != NodeKind::Comment
    }));
    if let Some(after) = node.after() {
        layout_children.push(after);
    }
}

/// Result of classifying the in-flow children of a flow container as
/// all-block, all-inline and/or all-out-of-flow.
struct FlowClassification {
    all_block: bool,
    all_inline: bool,
    all_out_of_flow: bool,
    has_contents: bool,
}

impl Default for FlowClassification {
    fn default() -> Self {
        Self {
            all_block: true,
            all_inline: true,
            all_out_of_flow: true,
            has_contents: false,
        }
    }
}

/// Classify a flow container's children (including its ::before/::after
/// pseudo-elements) for inline-vs-block layout, recursing transparently
/// through display:contents nodes (whose children participate in the
/// container's formatting context).
fn classify_flow_children(
    doc: &BaseDocument,
    container_node_id: NodeId,
    classification: &mut FlowClassification,
) {
    let node = &doc.nodes[container_node_id];
    // ::before/::after pseudos with display:contents are transparent for box
    // generation, so their children (e.g. generated text) participate in the
    // container's formatting context and vote in the classification. Pseudos
    // with any other display value generate their own box, which every
    // construction arm already pushes explicitly, so they cast no vote.
    let pseudo_ids = node
        .before()
        .into_iter()
        .chain(node.after())
        .filter(|pe_id| {
            let display = doc.nodes[*pe_id]
                .display_style()
                .unwrap_or(Display::inline());
            matches!(display.inside(), DisplayInside::Contents)
        });
    let child_ids = node.flat_children().iter().copied().chain(pseudo_ids);
    for child_id in child_ids {
        let child = &doc.nodes[child_id];

        // Comment nodes generate no boxes and must not affect the
        // inline-vs-block classification: an unstyled comment would
        // default to display:inline below and force an inline
        // formatting context on the container, swallowing element
        // siblings into the inline layout (zero-sizing any
        // out-of-flow ones).
        if child.data.kind() == NodeKind::Comment {
            continue;
        }

        // Unwraps on Text and SVG nodes
        let style = child.primary_styles();
        let style = style.as_ref();
        let display = child.display_style().unwrap_or(Display::inline());
        if matches!(display.inside(), DisplayInside::Contents) {
            // Transparent for box generation: the contents node casts
            // no vote itself — its children decide.
            classification.has_contents = true;
            classify_flow_children(doc, child_id, classification);
        } else if matches!(display.inside(), DisplayInside::None) {
            // display:none children generate no boxes and cast no vote.
            continue;
        } else {
            let position = style
                .map(|s| s.clone_position())
                .unwrap_or(PositionProperty::Static);
            let float = style.map(|s| s.clone_float()).unwrap_or(Float::None);

            // Ignore nodes whose whitespace is entirely collapsed away
            if child.is_collapsible_whitespace_node() {
                continue;
            }

            // display:none children generate no boxes and cast no vote
            if matches!(display.outside(), DisplayOutside::None) {
                continue;
            }

            let is_in_flow = matches!(
                position,
                PositionProperty::Static | PositionProperty::Relative | PositionProperty::Sticky
            ) && matches!(float, Float::None);

            if !is_in_flow {
                continue;
            }

            classification.all_out_of_flow = false;
            match display.outside() {
                DisplayOutside::None => {}
                DisplayOutside::Block
                | DisplayOutside::TableCaption
                | DisplayOutside::InternalTable => classification.all_inline = false,
                DisplayOutside::Inline => {
                    classification.all_block = false;

                    // We need the "complex" tree fixing when an inline contains a block
                    if child.is_or_contains_block() {
                        classification.all_inline = false;
                    }
                }
            }
        }
    }
}

pub(crate) fn collect_layout_children(
    doc: &mut BaseDocument,
    container_node_id: NodeId,
    out: &mut LayoutChildren,
) {
    collect_layout_children_with_wrap(doc, container_node_id, out, None)
}

fn collect_layout_children_with_wrap(
    doc: &mut BaseDocument,
    container_node_id: NodeId,
    out: &mut LayoutChildren,
    wrap: Option<WrapContext>,
) {
    // Record the `display` the box is being constructed with
    if let Some(display) = doc.nodes[container_node_id].display_style() {
        *doc.nodes[container_node_id].display_constructed_as_mut() = display;
    }

    // Reset construction flags
    // TODO: make incremental and only remove this if the element is no longer an inline root
    doc.nodes[container_node_id]
        .flags
        .reset_construction_flags();
    if let Some(element_data) = doc.nodes[container_node_id].element_data_mut() {
        element_data.take_inline_layout();
    }

    flush_pseudo_elements(doc, container_node_id);

    if let Some(el) = doc.nodes[container_node_id].data.downcast_element() {
        // Handle text inputs
        let tag_name = el.name.local.as_ref();
        if matches!(tag_name, "input" | "textarea") {
            // Type is an ASCII-ci enumerated attribute and
            // unknown/empty values fall to the Text state per HTML:
            // type="TEXT" / type="" / type="foo" must all build an
            // editor, not silently render nothing.
            let type_attr: Option<String> = doc.nodes[container_node_id]
                .data
                .downcast_element()
                .and_then(|el| el.attr(local_name!("type")))
                .map(|t| t.to_ascii_lowercase());
            let type_attr: Option<&str> = type_attr.as_deref();
            if tag_name == "textarea" {
                create_text_editor(doc, container_node_id, true);
                return;
            } else if !matches!(
                type_attr,
                Some(
                    "checkbox" | "radio" | "range" | "hidden" | "submit" | "button"
                        | "reset" | "file" | "image" | "color" | "date"
                        | "datetime-local" | "month" | "week" | "time"
                )
            ) {
                create_text_editor(doc, container_node_id, false);
                return;
            } else if matches!(type_attr, Some("checkbox" | "radio")) {
                create_checkbox_input(doc, container_node_id);
                return;
            } else if matches!(type_attr, Some("range")) {
                // Seed the slider's live value from the
                // value/min/max attributes (HTML default: midpoint
                // of the range when no value attribute). Construct
                // re-runs on every layout pass; keep an existing
                // RangeInput (it carries the live IDL value).
                let el = doc.nodes[container_node_id]
                    .data
                    .downcast_element_mut()
                    .unwrap();
                if !matches!(el.special_data, SpecialElementData::RangeInput(_)) {
                    // Rust `str::parse::<f64>()` accepts "NaN",
                    // "inf", "-inf": feeding those into
                    // f64::clamp panics (requires !NaN and min<=
                    // max). Untrusted `<input type=range min=NaN>`
                    // would take the whole layout down; filter to
                    // finite values before clamping.
                    fn parse_finite(v: &str) -> Option<f64> {
                        v.parse::<f64>().ok().filter(|n| n.is_finite())
                    }
                    let min = el
                        .attr(local_name!("min"))
                        .and_then(parse_finite)
                        .unwrap_or(0.0);
                    let max = el
                        .attr(local_name!("max"))
                        .and_then(parse_finite)
                        .unwrap_or(100.0);
                    let max = max.max(min);
                    let value = el
                        .attr(local_name!("value"))
                        .and_then(parse_finite)
                        .unwrap_or((min + max) / 2.0)
                        .clamp(min, max);
                    el.special_data = SpecialElementData::RangeInput(value);
                }
                return;
            }
        }

        #[cfg(feature = "svg")]
        if matches!(tag_name, "svg") {
            let (mut outer_html, snapshot_prefix) =
                doc.get_node(container_node_id).unwrap().svg_source();

            // Stamp the element's computed `color` on the serialized
            // root so usvg resolves any surviving `currentColor` by inheritance.
            // write_outer_html substitutes currentColor per node from its
            // primary_styles, but an unstyled child (styles not yet resolved at
            // construct time) leaves currentColor verbatim, and usvg then
            // defaults it to black. A root color attr is usvg's inheritance
            // fallback.
            let root_color = doc
                .get_node(container_node_id)
                .and_then(|n| n.primary_styles())
                .map(|s| s.clone_color())
                .map(|c| srgb_color_attr(&c));
            // Scope the "already has color" check to the opening <svg ...> tag;
            // a descendant color="..." must not suppress the root stamp.
            let root_tag_has_color = outer_html
                .split_once('>')
                .map(|(open_tag, _)| open_tag.contains(" color=\""))
                .unwrap_or(false);
            if let Some(color) = root_color {
                if !root_tag_has_color {
                    outer_html =
                        outer_html.replacen("<svg", &format!("<svg color=\"{color}\""), 1);
                }
            }

            // Icon systems color glyphs via CSS (`fill: currentcolor`
            // on the svg), which usvg never sees: it only reads attributes.

            // Stamp the computed fill on the root so paths without their own
            // fill attr inherit it, matching CSS inheritance.
            let root_fill = doc
                .get_node(container_node_id)
                .and_then(|n| n.primary_styles())
                .and_then(|s| {
                    use style::values::generics::color::GenericColor;
                    use style::values::generics::svg::SVGPaintKind;
                    match &s.clone_fill().kind {
                        SVGPaintKind::None => Some("none".to_string()),
                        // svgtypes matches "currentColor" case-sensitively;
                        // the root color stamp above gives it a value.
                        SVGPaintKind::Color(GenericColor::CurrentColor) => {
                            Some("currentColor".to_string())
                        }
                        SVGPaintKind::Color(GenericColor::Absolute(abs)) => {
                            Some(srgb_color_attr(abs))
                        }
                        _ => None,
                    }
                });
            let root_tag_has_fill = outer_html
                .split_once('>')
                .map(|(open_tag, _)| open_tag.contains(" fill=\""))
                .unwrap_or(false);
            if let Some(fill) = root_fill {
                if !root_tag_has_fill {
                    outer_html =
                        outer_html.replacen("<svg", &format!("<svg fill=\"{fill}\""), 1);
                }
            }

            // Browsers resolve `<use href="#id">` against the whole
            // document; usvg only sees this one serialized element.

            // Inline externally-defined targets (sprite-sheet <symbol>s etc.)
            // as a <defs> block so usvg can resolve them.
            inline_external_fragment_refs(doc, &mut outer_html);

            // Inner SVG elements never cascade, so CSS-era attribute values
            // (var(), color-mix(), rem) would reach usvg verbatim.
            resolve_css_values_in_svg(doc, container_node_id, &mut outer_html);

            // Remove contruction damage from subtree
            doc.iter_subtree_mut(container_node_id, |id: NodeId, doc: &mut BaseDocument| {
                doc.nodes[id].remove_damage(CONSTRUCT_BOX | CONSTRUCT_DESCENDENT | CONSTRUCT_FC);
            });

            match crate::util::parse_svg_image(outer_html.as_bytes()) {
                Ok(mut svg) => {
                    svg.snapshot_prefix = Some(snapshot_prefix);
                    let node = doc.get_node_mut(container_node_id).unwrap();
                    node.element_data_mut().unwrap().special_data =
                        SpecialElementData::Image(Box::new(crate::node::ImageData::Svg(svg)));
                }
                Err(err) => {
                    #[cfg(feature = "tracing")]
                    tracing::warn!(
                        node_id = ?container_node_id,
                        html = outer_html,
                        error = ?err,
                        "SVG parse failed",
                    );
                    #[cfg(not(feature = "tracing"))]
                    let _ = err;
                }
            };
            return;
        }

        //Only ol tags have start and reversed attributes
        let (mut index, reversed) = if tag_name == "ol" {
            (
                el.attr_parsed(local_name!("start"))
                    .map(|start: usize| start - 1)
                    .unwrap_or(0),
                el.attr_parsed(local_name!("reversed")).unwrap_or(false),
            )
        } else {
            (0, false)
        };
        collect_list_item_children(doc, &mut index, reversed, container_node_id);
    }

    // Skip further construction if the node has no children or psuedo-children
    {
        let node = &doc.nodes[container_node_id];
        if node.flat_children().is_empty() && node.before().is_none() && node.after().is_none() {
            return;
        }
    }

    let container_display = doc.nodes[container_node_id].display_style().unwrap_or(
        match doc.nodes[container_node_id].data.kind() {
            NodeKind::AnonymousBlock => Display::Block,
            _ => Display::Inline,
        },
    );

    // Display:table on a replaced element still yields a replaced box;
    // building a table here overwrote its image/canvas data and layout panicked.
    let is_replaced = doc.nodes[container_node_id]
        .data
        .downcast_element()
        .is_some_and(|el| super::replaced::is_replaced_element(&el.name.local));

    match container_display.inside() {
        DisplayInside::None => {}
        DisplayInside::Contents => {
            doc.nodes[container_node_id]
                .remove_damage(CONSTRUCT_BOX | CONSTRUCT_DESCENDENT | CONSTRUCT_FC);
            // display:contents is transparent for box generation: hoist the
            // children THEMSELVES (not their layout children) into the
            // parent, recursing only through nested contents nodes.
            push_hoisted_children_and_pseudos(doc, container_node_id, out, wrap);
        }
        DisplayInside::Flex | DisplayInside::Grid | DisplayInside::WebkitBox => {
            // ::before/::after pseudos must be checked too: a pseudo with
            // display:contents hoists its text content into the container.
            let container = &doc.nodes[container_node_id];
            let has_text_node_or_contents = container
                .flat_children()
                .iter()
                .copied()
                .chain(container.before())
                .chain(container.after())
                .map(|child_id| &doc.nodes[child_id])
                .any(|child| {
                    let display = child.display_style().unwrap_or(Display::inline());
                    let node_kind = child.data.kind();
                    display.inside() == DisplayInside::Contents || node_kind == NodeKind::Text
                });

            if !has_text_node_or_contents {
                push_non_whitespace_children_and_pseudos(
                    &mut out.children,
                    &doc.nodes[container_node_id],
                );
            } else {
                collect_complex_layout_children(
                    doc,
                    container_node_id,
                    out,
                    true,
                    text_item_needs_wrap,
                );
            }

            // Flex/grid items are laid out in order-modified document order.
            sort_layout_children_by_order(&doc.nodes, &mut out.children);
        }

        DisplayInside::Table if !is_replaced => {
            let (table_context, tlayout_children) = build_table_context(doc, container_node_id);
            #[allow(clippy::arc_with_non_send_sync)]
            let data = SpecialElementData::TableRoot(Arc::new(table_context));
            doc.nodes[container_node_id]
                .flags
                .insert(NodeFlags::IS_TABLE_ROOT);
            doc.nodes[container_node_id]
                .data
                .downcast_element_mut()
                .unwrap()
                .special_data = data;
            if let Some(before) = doc.nodes[container_node_id].before() {
                out.push(before, doc);
            }
            out.extend(&tlayout_children, doc);
            if let Some(after) = doc.nodes[container_node_id].after() {
                out.push(after, doc);
            }
        }

        // Flow, FlowRoot and TableCell, plus internal table displays (row,
        // row group, column, ...) occurring outside of a table. Blitz does
        // not yet generate anonymous table wrapper boxes for the latter, so
        // they are laid out as flow containers, which crucially ensures
        // their text children get wrapped rather than being pushed as bare
        // layout children (text nodes carry no style and cannot be laid out
        // as block/flex/grid items).
        _ => {
            let mut classification = FlowClassification::default();
            classify_flow_children(doc, container_node_id, &mut classification);

            if classification.all_out_of_flow {
                // Contents-transparent: a display:contents child may be
                // holding the out-of-flow elements (otherwise the contents
                // node itself would be pushed as a layout box).
                let wrap = Some(WrapContext {
                    container_node_id,
                    needs_wrap: text_item_needs_wrap,
                });
                return push_hoisted_children_and_pseudos(doc, container_node_id, out, wrap);
            }

            // TODO: fix display:contents
            if classification.all_inline {
                let existing_layout = doc.nodes[container_node_id]
                    .element_data_mut()
                    .and_then(|el| el.inline_layout_data.take());
                let layout = existing_layout.unwrap_or_else(|| Box::new(TextLayout::new()));

                // Queue node for inline layout construction. Deferring construction of inline layouts to a
                // dedicated phase allows us to multithread the expensive text shaping step.
                doc.deferred_construction_nodes.push(ConstructionTask {
                    node_id: container_node_id,
                    data: ConstructionTaskData::InlineLayout(layout),
                });
                doc.nodes[container_node_id]
                    .flags
                    .insert(NodeFlags::IS_INLINE_ROOT);

                find_inline_layout_embedded_boxes(doc, container_node_id, &mut out.children);
                return;
            }

            // Misparented table-internal children force the complex path,
            // which owns the anonymous-table fixup (CSS 2.2 §17.2.1).
            let has_table_internal = doc.nodes[container_node_id]
                .flat_children()
                .iter()
                .any(|&child_id| {
                    let child = &doc.nodes[child_id];
                    child.data.kind() == NodeKind::Element
                        && child
                            .display_style()
                            .map(is_table_internal)
                            .unwrap_or(false)
                });

            // If the children are either all inline or all block then simply return the regular children
            // as the layout children
            if !has_table_internal {
                if classification.all_block & !classification.has_contents {
                    return push_non_whitespace_children_and_pseudos(
                        &mut out.children,
                        &doc.nodes[container_node_id],
                    );
                } else if classification.all_inline & !classification.has_contents {
                    return push_children_and_pseudos(
                        &mut out.children,
                        &doc.nodes[container_node_id],
                    );
                }
            }

            collect_complex_layout_children(
                doc,
                container_node_id,
                out,
                false,
                block_item_needs_wrap,
            );
        }
    }
}

/// Extract the text generated by a pseudo-element's `content` property
/// (only string content items are currently supported; other item types
/// are skipped).
fn pe_content_text(style: &style::properties::ComputedValues) -> Option<String> {
    match &style.get_counters().content {
        Content::Items(item_data) => {
            let items = &item_data.items[0..item_data.alt_start];
            let mut text = String::new();
            let mut has_string = false;
            for item in items {
                // TODO: other types of content
                if let ContentItem::String(owned_str) = item {
                    text.push_str(owned_str);
                    has_string = true;
                }
            }
            has_string.then_some(text)
        }
        _ => None,
    }
}

// Pseudo content with counters/attr/multiple items resolves through the
// side table the resolve_counters pass built; plain strings stay inline.
fn pe_content_text_resolved(
    doc: &BaseDocument,
    node_id: NodeId,
    pseudo_idx: usize,
    style: &style::properties::ComputedValues,
) -> Option<String> {
    let over = if pseudo_idx == 1 {
        doc.counters.before_text.get(&node_id)
    } else {
        doc.counters.after_text.get(&node_id)
    };
    if let Some(t) = over {
        return Some(t.clone());
    }
    pe_content_text(style)
}

fn flush_pseudo_elements(doc: &mut BaseDocument, node_id: NodeId) {
    let (before_style, after_style, before_node_id, after_node_id) = {
        let node = &doc.nodes[node_id];

        let before_node_id = node.before();
        let after_node_id = node.after();

        // Note: yes these are kinda backwards
        let style_data = node.try_stylo_element_data().and_then(|s| s.get());
        let before_style = style_data
            .as_ref()
            .and_then(|d| d.styles.pseudos.as_array()[1].clone());
        let after_style = style_data
            .as_ref()
            .and_then(|d| d.styles.pseudos.as_array()[0].clone());

        (before_style, after_style, before_node_id, after_node_id)
    };

    // Sync pseudo element
    // TODO: Make incremental
    for (idx, pe_style, pe_node_id) in [
        (1, before_style, before_node_id),
        (0, after_style, after_node_id),
    ] {
        // Delete psuedo element if it exists but shouldn't
        if let (Some(pe_node_id), None) = (pe_node_id, &pe_style) {
            doc.remove_and_drop_pe(pe_node_id);
            let node = &mut doc.nodes[node_id];
            node.set_pe_by_index(idx, None);
            node.insert_damage(ALL_DAMAGE);
        }

        // Create pseudo element if it should exist but doesn't
        if let (None, Some(pe_style)) = (pe_node_id, &pe_style) {
            let new_node_id = doc.create_node(NodeData::AnonymousBlock(Box::new(
                ElementData::new(DUMMY_NAME, Vec::new()),
            )));
            doc.nodes[new_node_id].parent = Some(node_id);
            doc.nodes[new_node_id].layout_parent.set(Some(node_id));
            if doc.nodes[node_id].flags.contains(NodeFlags::IS_IN_DOCUMENT) {
                doc.nodes[new_node_id]
                    .flags
                    .insert(NodeFlags::IS_IN_DOCUMENT);
            }

            if let Some(text) = pe_content_text_resolved(doc, node_id, idx, pe_style) {
                let text_node_id = doc.create_text_node(&text);
                doc.nodes[text_node_id].parent = Some(new_node_id);
                doc.nodes[new_node_id].children.push(text_node_id);
                if doc.nodes[node_id].flags.is_in_document() {
                    doc.nodes[text_node_id]
                        .flags
                        .insert(NodeFlags::IS_IN_DOCUMENT);
                    doc.note_document_codepoints(&text);
                }
            }

            let mut element_data = StyloElementData::default();
            element_data.styles.primary = Some(pe_style.clone());
            element_data.set_restyled();
            element_data.damage = ALL_DAMAGE;
            *doc.nodes[new_node_id]
                .stylo_element_data_mut()
                .ensure_init_mut() = element_data;

            let node = &mut doc.nodes[node_id];
            node.set_pe_by_index(idx, Some(new_node_id));
            node.insert_damage(ALL_DAMAGE);

            doc.pending_style_image_nodes.push(new_node_id);
        }

        // Else: Update psuedo element
        if let (Some(pe_node_id), Some(pe_style)) = (pe_node_id, pe_style) {
            // Sync the pseudo-element's text node with its `content` style, which
            // may have changed (e.g. `details[open] summary::after { content: ... }`).
            //
            // Note: this deliberately compares the text itself rather than relying on
            // the style-pointer comparison below, as the pseudo-element's style may
            // already have been updated by `sync_pseudo_element_styles` during the
            // style traversal without the text having been updated.
            let new_text = pe_content_text_resolved(doc, node_id, idx, &pe_style);
            let existing_text_node_id = doc.nodes[pe_node_id]
                .children
                .first()
                .copied()
                .filter(|&child_id| doc.nodes[child_id].is_text_node());
            match (existing_text_node_id, new_text) {
                (Some(text_node_id), Some(new_text)) => {
                    let text_data = doc.nodes[text_node_id].text_data_mut().unwrap();
                    if text_data.content != new_text {
                        // Generated content needs glyphs too.
                        let in_document = doc.nodes[node_id].flags.is_in_document();
                        doc.note_text_codepoints(&new_text, in_document);
                        let text_data = doc.nodes[text_node_id].text_data_mut().unwrap();
                        text_data.content = new_text;
                        doc.nodes[node_id].insert_damage(ALL_DAMAGE);
                    }
                }
                (None, Some(new_text)) => {
                    let in_document = doc.nodes[node_id].flags.is_in_document();
                    let text_node_id = doc.create_text_node(&new_text);
                    doc.nodes[text_node_id].parent = Some(pe_node_id);
                    doc.nodes[pe_node_id].children.push(text_node_id);
                    if in_document {
                        doc.nodes[text_node_id]
                            .flags
                            .insert(NodeFlags::IS_IN_DOCUMENT);
                        doc.note_document_codepoints(&new_text);
                    }
                    doc.nodes[node_id].insert_damage(ALL_DAMAGE);
                }
                (Some(text_node_id), None) => {
                    doc.nodes[pe_node_id]
                        .children
                        .retain(|&child_id| child_id != text_node_id);
                    doc.remove_node_from_tree(text_node_id);
                    doc.nodes[node_id].insert_damage(ALL_DAMAGE);
                }
                (None, None) => {}
            }

            let mut node_styles = doc.nodes[pe_node_id]
                .try_stylo_element_data_mut()
                .and_then(|s| s.get_mut());
            let node_styles = &mut node_styles.as_mut().unwrap();
            node_styles.damage.insert(ALL_DAMAGE);
            let primary_styles = &mut node_styles.styles.primary;

            if !std::ptr::eq(&**primary_styles.as_ref().unwrap(), &*pe_style) {
                *primary_styles = Some(pe_style);
                node_styles.set_restyled();
                doc.pending_style_image_nodes.push(pe_node_id);
            }
        }
    }
}

/// Handles the cases where there are text nodes or inline nodes that need to be wrapped in an anonymous block node
fn collect_complex_layout_children(
    doc: &mut BaseDocument,
    container_node_id: NodeId,
    out: &mut LayoutChildren,
    hide_whitespace: bool,
    needs_wrap: fn(NodeKind, DisplayOutside) -> bool,
) {
    doc.iter_flat_children_and_pseudos_mut(container_node_id, |child_id, doc| {
        // Get node kind (text, element, comment, etc)
        let child_node_kind = doc.nodes[child_id].data.kind();

        // Get Display style. Default to inline because nodes without styles are probably text nodes
        let contains_block = doc.nodes[child_id].is_or_contains_block();
        let child_display = &doc.nodes[child_id]
            .display_style()
            .unwrap_or(Display::inline());
        let display_inside = child_display.inside();
        let display_outside = if contains_block {
            DisplayOutside::Block
        } else {
            child_display.outside()
        };

        let is_whitespace_node = doc.nodes[child_id].is_whitespace_node();

        // Skip comment nodes. Note that we do *not* skip `Display::None` nodes as they may need to be hidden.
        // Taffy knows how to deal with `Display::None` children.
        //
        // Also hide all-whitespace flexbox children as these should be ignored
        if child_node_kind == NodeKind::Comment || (hide_whitespace && is_whitespace_node) {
            // return;
        }
        // Recurse into `Display::Contents` nodes, wrapping hoisted text and
        // inline children as if they were direct children of this container.
        // Whitespace between table-internal boxes is dropped by the
        // anonymous-table fixup instead of terminating the run.
        else if is_whitespace_node && out.anonymous_table_id.is_some() {
            // return;
        }
        // Misparented table-internal children collect into an anonymous
        // `display: table` wrapper (CSS 2.2 §17.2.1).
        else if child_node_kind == NodeKind::Element && is_table_internal(*child_display) {
            out.push_table_wrapped(container_node_id, child_id, doc);
        }
        else if display_inside == DisplayInside::Contents {
            let wrap = Some(WrapContext {
                container_node_id,
                needs_wrap,
            });
            collect_layout_children_with_wrap(doc, child_id, out, wrap)
        }
        // Push nodes that need wrapping into the current "anonymous block container".
        // If there is not an open one then we create one.
        else if needs_wrap(child_node_kind, display_outside) {
            out.push_wrapped(container_node_id, child_id, doc);
        }
        // Else push the child directly (and close any open "anonymous block container")
        else {
            out.push(child_id, doc);
        }
    });

    // If anonymous block node only contains whitespace then delete it, else push it
    out.maybe_push_anon_block(doc);
    out.anonymous_table_id = None;
}

/// The `::placeholder` style of a text control that has a non-empty
/// `placeholder` attribute. It is a lazy pseudo, so nothing in the style
/// traversal computes it; resolve it against the control's own style.
pub(crate) fn placeholder_pseudo_style(
    stylist: &style::stylist::Stylist,
    guards: &StylesheetGuards,
    node: &Node,
    primary: &style::properties::ComputedValues,
) -> Option<style::servo_arc::Arc<style::properties::ComputedValues>> {
    use style::selector_parser::PseudoElement;
    use style::stylist::RuleInclusion;
    if node
        .attr(local_name!("placeholder"))
        .is_none_or(|p| p.is_empty())
    {
        return None;
    }
    stylist.lazily_compute_pseudo_element_style(
        guards,
        node,
        &PseudoElement::Placeholder,
        RuleInclusion::All,
        primary,
        false,
        None,
    )
}

/// What a shown placeholder takes from its `::placeholder` style: the colour,
/// with opacity folded into its alpha, and the font style.
pub(crate) fn placeholder_look(
    style: &style::properties::ComputedValues,
) -> (style::color::AbsoluteColor, parley::FontStyle) {
    let color = style.clone_color();
    (
        color.with_alpha(color.alpha * style.get_effects().opacity),
        stylo_to_parley::font_style(style.clone_font_style()),
    )
}

fn document_placeholder_style(
    doc: &BaseDocument,
    input_element_id: NodeId,
) -> Option<style::servo_arc::Arc<style::properties::ComputedValues>> {
    let node = doc.get_node(input_element_id)?;
    let primary = node.primary_styles()?;
    let guard = doc.guard().read();
    let guards = StylesheetGuards::same(&guard);
    placeholder_pseudo_style(&doc.stylist, &guards, node, &primary)
}

fn create_text_editor(doc: &mut BaseDocument, input_element_id: NodeId, is_multiline: bool) {
    let placeholder_style = document_placeholder_style(doc, input_element_id);
    let node = &mut doc.nodes[input_element_id];
    let parley_style = node
        .primary_styles()
        .as_ref()
        .map(|s| stylo_to_parley::style(node.id, s))
        .unwrap_or_default();
    let alignment = node
        .primary_styles()
        .map(|s| stylo_to_parley::text_align(s.clone_text_align()))
        .unwrap_or(parley::layout::Alignment::Start);
    // `unicode-bidi: plaintext` (the UA sheet's dir=auto rule for
    // text controls) means per-paragraph first-strong, not whole-text.
    let base_direction = node
        .primary_styles()
        .map(|s| match s.clone_unicode_bidi() {
            style::computed_values::unicode_bidi::T::Plaintext => {
                parley::BaseDirection::AutoPerParagraph
            }
            bidi => stylo_to_parley::base_direction(s.clone_direction(), bidi),
        })
        .unwrap_or(parley::BaseDirection::Auto);

    let initial_text = if is_multiline {
        node.text_content()
    } else {
        node.attr(local_name!("value")).unwrap_or("").to_string()
    };
    let element = &mut node.data.downcast_element_mut().unwrap();
    // Decide what text the editor should display. The HTML spec
    // says <input> shows its `value` attribute (or the empty
    // string if absent), and shows the `placeholder` attribute
    // (with placeholder styling: typically grey / lighter color)
    // only when the value is empty and the field is unfocused.
    //
    // This synthesizes the placeholder text into the editor with a
    // lighter brush so the user sees something instead of a
    // blank box. Once the user types into the field, value
    // becomes non-empty, refresh_layout runs again here, and
    // the placeholder fallback stops firing.
    //
    // This is a simplified `:placeholder-shown` polyfill; a
    // proper implementation would pseudo-element-style the
    // placeholder (color, font-style: italic, etc.) and
    // render it without putting it through the editor's
    // selection/caret machinery. This version shows
    // the user the prompt text, doesn't count as the field's
    // actual value (caret behaviour, form submission, etc.
    // all read `value` not editor text).
    // The IDL `.value` override (set by JS through
    // DocumentMutator::set_text_input_value) wins over the content
    // attribute: HTML keeps the live value and the attribute
    // (defaultValue) decoupled, and this construct pass re-runs on
    // every layout, so without the override precedence a JS value
    // write would be clobbered right back to the attribute text.
    let value_override = match &element.special_data {
        SpecialElementData::TextInput(d) => d.value_override.clone(),
        _ => None,
    };
    // (Upstream: a <textarea>'s value is its child text, an <input>'s its
    // value attribute; `initial_text` already picked the right one.)
    let value_attr = value_override
        .as_deref()
        .or(Some(initial_text.as_str()));
    let placeholder_attr = element.attr(local_name!("placeholder"));
    let (display_text, is_placeholder) = match (value_attr, placeholder_attr) {
        (Some(v), _) if !v.is_empty() => (v.to_string(), false),
        (_, Some(p)) if !p.is_empty() => (p.to_string(), true),
        _ => (" ".to_string(), false),
    };

    // The ::placeholder cascade (UA `color: #757575`, plus any author
    // rules) supplies the colour and font style while the placeholder
    // shows. Without it the text keeps the input's own style.
    let placeholder_look = placeholder_style
        .filter(|_| is_placeholder)
        .map(|s| placeholder_look(&s));
    let placeholder_color = placeholder_look.map(|(color, _)| color);
    let placeholder_font_style = placeholder_look.map(|(_, font_style)| font_style);

    if !matches!(element.special_data, SpecialElementData::TextInput(_)) {
        let mut text_input_data = TextInputData::new(is_multiline);
        let editor = &mut text_input_data.editor;
        editor.set_text(&display_text);
        text_input_data.is_showing_placeholder = is_placeholder;
        text_input_data.placeholder_color = placeholder_color;
        text_input_data.placeholder_font_style = placeholder_font_style;
        element.special_data = SpecialElementData::TextInput(text_input_data);
    } else if let SpecialElementData::TextInput(data) = &mut element.special_data {
        // Update the editor's text on subsequent layouts so an
        // empty input switches between placeholder and real
        // value as the user types. This guard keeps the
        // is_showing_placeholder flag in sync with what the
        // editor actually contains.
        let editor = &mut data.editor;
        if editor.text() != display_text.as_str() {
            editor.set_text(&display_text);
        }
        data.is_showing_placeholder = is_placeholder;
        data.placeholder_color = placeholder_color;
        data.placeholder_font_style = placeholder_font_style;
    }

    let SpecialElementData::TextInput(text_input_data) = &mut element.special_data else {
        unreachable!();
    };

    let editor = &mut text_input_data.editor;
    editor.set_scale(doc.viewport.scale_f64() as f32);
    editor.set_width(None);

    let styles = editor.edit_styles();
    styles.retain(|_| false);
    styles.insert(StyleProperty::FontFamily(parley_style.font_family));
    styles.insert(StyleProperty::FontSize(parley_style.font_size));
    styles.insert(StyleProperty::FontWidth(parley_style.font_width));
    styles.insert(StyleProperty::FontStyle(
        placeholder_font_style.unwrap_or(parley_style.font_style),
    ));
    styles.insert(StyleProperty::FontWeight(parley_style.font_weight));
    styles.insert(StyleProperty::FontVariations(parley_style.font_variations));
    styles.insert(StyleProperty::FontFeatures(parley_style.font_features));
    styles.insert(StyleProperty::LineHeight(parley_style.line_height));
    styles.insert(StyleProperty::WordSpacing(parley_style.word_spacing));
    styles.insert(StyleProperty::LetterSpacing(parley_style.letter_spacing));
    styles.insert(StyleProperty::WordBreak(parley_style.word_break));
    styles.insert(StyleProperty::OverflowWrap(parley_style.overflow_wrap));
    styles.insert(StyleProperty::TextWrapMode(parley_style.text_wrap_mode));
    styles.insert(StyleProperty::WhiteSpaceCollapse(
        parley_style.white_space_collapse,
    ));
    // The brush stays the input's node: paint swaps in
    // `placeholder_color` while the placeholder shows.
    styles.insert(StyleProperty::Brush(parley_style.brush));
    editor.set_alignment(alignment);
    editor.set_base_direction(base_direction);

    editor.refresh_layout(&mut doc.font_ctx.lock().unwrap(), &mut doc.layout_ctx);
}

fn create_checkbox_input(doc: &mut BaseDocument, input_element_id: NodeId) {
    let node = &mut doc.nodes[input_element_id];

    let element = &mut node.data.downcast_element_mut().unwrap();
    if !matches!(element.special_data, SpecialElementData::CheckboxInput(_)) {
        let checked = element.has_attr(local_name!("checked"));
        element.special_data = SpecialElementData::CheckboxInput(checked);
        element.set_checkbox_input_checked(checked);
    }
}

/// Find and return the "layout_children" (inline boxes) for an inline layout
/// without actually constructing the layout. This allows us to defer the expensive
/// construction of the Parley layout (which invokes text shaping) to a paralell phase.
pub(crate) fn find_inline_layout_embedded_boxes(
    doc: &mut BaseDocument,
    inline_context_root_node_id: NodeId,
    layout_children: &mut ThinVec<NodeId>,
) {
    flush_inline_pseudos_recursive(doc, inline_context_root_node_id);

    iter_flat_children_and_pseudos!(doc.nodes, inline_context_root_node_id, |child_id| {
        find_inline_layout_embedded_boxes_recursive(
            &mut doc.nodes,
            inline_context_root_node_id,
            child_id,
            layout_children,
        );
    });

    fn flush_inline_pseudos_recursive(doc: &mut BaseDocument, node_id: NodeId) {
        doc.iter_flat_children_mut(node_id, |child_id, doc| {
            flush_pseudo_elements(doc, child_id);
            // Recurse based on the child's display, not the
            // parent's. Reading doc.nodes[node_id] (the node being
            // iterated) would make do_recurse false for a block
            // inline-context root (p, div), so ::before/::after on
            // inline elements nested deeper than one level
            // (p > span > i.icon:before) would never be synthesized.
            // The recursive walk below (build_inline_layout_recursive)
            // handles nested pseudos once they exist.
            let display = doc.nodes[child_id]
                .display_style()
                .unwrap_or(Display::inline());
            let do_recurse = match (display.outside(), display.inside()) {
                (DisplayOutside::None, DisplayInside::Contents) => true,
                (DisplayOutside::Inline, DisplayInside::Flow) => true,
                (_, _) => false,
            };
            if do_recurse {
                flush_inline_pseudos_recursive(doc, child_id);
            }
        });
    }

    fn find_inline_layout_embedded_boxes_recursive(
        nodes: &mut crate::NodeTree,
        parent_id: NodeId,
        node_id: NodeId,
        layout_children: &mut ThinVec<NodeId>,
    ) {
        let node = &mut nodes[node_id];

        // Set layout_parent for node.
        node.layout_parent.set(Some(parent_id));

        match &node.data {
            NodeData::Element(element_data) | NodeData::AnonymousBlock(element_data) => {
                // if the input type is hidden, hide it
                if *element_data.name.local == *"input" {
                    if element_data.attr(local_name!("type")).is_some_and(|t| t.eq_ignore_ascii_case("hidden")) {
                        return;
                    }
                }

                let display = node.display_style().unwrap_or(Display::inline());

                match (display.outside(), display.inside()) {
                    (DisplayOutside::None, DisplayInside::None) => {
                        node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                    }
                    (DisplayOutside::None, DisplayInside::Contents) => {
                        node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                        iter_flat_children!(nodes, node_id, |child_id| {
                            find_inline_layout_embedded_boxes_recursive(
                                nodes,
                                parent_id,
                                child_id,
                                layout_children,
                            );
                        });
                    }
                    (DisplayOutside::Inline, DisplayInside::Flow) => {
                        let tag_name = &element_data.name.local;

                        if is_inline_box_element(tag_name) {
                            layout_children.push(node_id);
                        } else if *tag_name == local_name!("br") {
                            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                        } else {
                            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                            iter_flat_children_and_pseudos!(nodes, node_id, |child_id| {
                                find_inline_layout_embedded_boxes_recursive(
                                    nodes,
                                    node_id,
                                    child_id,
                                    layout_children,
                                );
                            });
                        }
                    }
                    // Inline box
                    (_, _) => {
                        layout_children.push(node_id);
                    }
                };
            }
            NodeData::Comment { .. } | NodeData::Text(_) => {
                node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            }
            // Shadow roots never sit in a child list; tolerate one
            // showing up here rather than killing the whole paint.
            NodeData::ShadowRoot(_) => {}
            NodeData::Document(_) => unreachable!(),
        }
    }
}

// UAX#9 control pair for a computed unicode-bidi value.
// Isolates close with PDI, embeds/overrides with PDF.

// The isolate direction follows computed direction: dir=auto is
// already resolved into it by the presentational hint (stylo.rs),
// with the HTML scan exclusions FSI can't express (dir-shadow-33:
// shadow text must not steer a host's isolate). Plaintext keeps
// FSI: it is first-strong-of-content by definition.
fn bidi_wrap_chars(
    ub: style::computed_values::unicode_bidi::T,
    rtl: bool,
) -> (&'static str, &'static str) {
    use style::computed_values::unicode_bidi::T as UnicodeBidi;
    match ub {
        UnicodeBidi::Isolate => (if rtl { "\u{2067}" } else { "\u{2066}" }, "\u{2069}"),
        UnicodeBidi::Plaintext => ("\u{2068}", "\u{2069}"),
        UnicodeBidi::IsolateOverride => {
            if rtl {
                ("\u{2067}\u{202E}", "\u{202C}\u{2069}")
            } else {
                ("\u{2066}\u{202D}", "\u{202C}\u{2069}")
            }
        }
        UnicodeBidi::Embed => (if rtl { "\u{202B}" } else { "\u{202A}" }, "\u{202C}"),
        UnicodeBidi::BidiOverride => (if rtl { "\u{202E}" } else { "\u{202D}" }, "\u{202C}"),
        UnicodeBidi::Normal => ("", ""),
    }
}

pub(crate) fn build_inline_layout_into(
    nodes: &crate::NodeTree,
    layout_ctx: &mut LayoutContext<TextBrush>,
    font_ctx: &mut FontContext,
    text_layout: &mut TextLayout,
    scale: f32,
    inline_context_root_node_id: NodeId,
) {
    // Get the inline context's root node's text styles
    let root_node = &nodes[inline_context_root_node_id];
    let root_node_style = root_node.primary_styles().or_else(|| {
        root_node
            .parent
            .and_then(|parent_id| nodes[parent_id].primary_styles())
    });

    let parley_style = root_node_style
        .as_ref()
        .map(|s| stylo_to_parley::style(inline_context_root_node_id, s))
        .unwrap_or_else(|| parley::TextStyle {
            white_space_collapse: WhiteSpaceCollapse::Collapse,
            ..Default::default()
        });

    // Create a parley tree builder
    let mut builder = layout_ctx.tree_builder(font_ctx, scale, true, &parley_style);

    // Stamp the block's bidi base direction; parley's
    // default whole-text autodetect ignores CSS direction entirely.

    // Computed direction is authoritative: dir=auto resolves into
    // the cascade via the presentational hint (stylo.rs).
    if let Some(s) = root_node_style.as_deref() {
        use style::computed_values::unicode_bidi::T as UnicodeBidi;
        let base_direction = match s.clone_unicode_bidi() {
            UnicodeBidi::Plaintext => BaseDirection::AutoPerParagraph,
            bidi => stylo_to_parley::base_direction(s.clone_direction(), bidi),
        };
        builder.set_base_direction(base_direction);
    }

    let text_transform = root_node_style
        .as_deref()
        .map(|s| CaseTransform::from_style(s))
        .unwrap_or(CaseTransform::NONE);

    // Render position-inside list items
    if let Some(ListItemLayout {
        marker,
        position: ListItemLayoutPosition::Inside,
    }) = root_node
        .element_data()
        .and_then(|el| el.list_item_data.as_deref())
    {
        match marker {
            // Bullet glyphs live in the bundled bullet font. The position-outside
            // path already asks for it; without the same span here a marker like
            // disclosure-closed (U+25B8) falls back to the element's own font and
            // renders as a missing glyph.
            Marker::Char(char) => {
                let mut marker_style = parley_style.clone();
                marker_style.font_family = BULLET_FONT_FAMILY.into();
                builder.push_style_span(marker_style);
                builder.push_text(&format!("{char} "));
                builder.pop_style_span();
            }
            Marker::String(str) => builder.push_text(str),
        }
    };
    // The marker is a separate box, so words in the content don't continue from it.
    let mut text_transformer = TextTransformer::default();
    text_transformer.word_break(&builder);

    // Per-text-node byte ranges for the DOM-selection
    // paint bridge; measured around each push (see TextLayout).
    let node_ranges: &mut Vec<(NodeId, core::ops::Range<usize>)> = &mut Vec::new();

    if let Some(before_id) = root_node.before() {
        build_inline_layout_recursive(
            &mut builder,
            &mut text_transformer,
            nodes,
            before_id,
            &text_transform,
            node_ranges,
        );
    }
    for child_id in root_node.flat_children().iter().copied() {
        build_inline_layout_recursive(
            &mut builder,
            &mut text_transformer,
            nodes,
            child_id,
            &text_transform,
            node_ranges,
        );
    }
    if let Some(after_id) = root_node.after() {
        build_inline_layout_recursive(
            &mut builder,
            &mut text_transformer,
            nodes,
            after_id,
            &text_transform,
            node_ranges,
        );
    }

    text_layout.text = builder.build_into(&mut text_layout.layout);
    text_layout.node_ranges = core::mem::take(node_ranges);
    return;

    fn build_inline_layout_recursive(
        builder: &mut TreeBuilder<TextBrush>,
        text_transformer: &mut TextTransformer,
        nodes: &crate::NodeTree,
        node_id: NodeId,
        parent_text_transform: &CaseTransform,
        node_ranges: &mut Vec<(NodeId, core::ops::Range<usize>)>,
    ) {
        let node = &nodes[node_id];

        let style = node.primary_styles();
        let style = style.as_ref();

        let text_transform = style
            .map(|s| CaseTransform::from_style(s))
            .unwrap_or(CaseTransform::NONE);

        match &node.data {
            NodeData::Element(element_data) | NodeData::AnonymousBlock(element_data) => {
                // if the input type is hidden, hide it
                if *element_data.name.local == *"input" {
                    if element_data.attr(local_name!("type")).is_some_and(|t| t.eq_ignore_ascii_case("hidden")) {
                        return;
                    }
                }

                let display = node.display_style().unwrap_or(Display::inline());
                let position = style
                    .map(|s| s.clone_position())
                    .unwrap_or(PositionProperty::Static);
                let float = style.map(|s| s.clone_float()).unwrap_or(Float::None);
                let box_kind = if position.is_absolutely_positioned() {
                    InlineBoxKind::OutOfFlow
                } else if float.is_floating() {
                    InlineBoxKind::CustomOutOfFlow
                } else {
                    InlineBoxKind::InFlow
                };

                match (display.outside(), display.inside()) {
                    (DisplayOutside::None, DisplayInside::None) => {
                        // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                    }
                    (DisplayOutside::None, DisplayInside::Contents) => {
                        let whitespace = style.map(|s| {
                            [
                                parley::StyleProperty::WhiteSpaceCollapse(
                                    stylo_to_parley::white_space_collapse(
                                        s.clone_white_space_collapse(),
                                    ),
                                ),
                                parley::StyleProperty::TextWrapMode(
                                    stylo_to_parley::text_wrap_mode(s.clone_text_wrap_mode()),
                                ),
                            ]
                        });
                        builder.push_style_modification_span(
                            whitespace
                                .as_ref()
                                .map_or(&[][..], |styles| styles.as_slice()),
                        );
                        for child_id in node.flat_children().iter().copied() {
                            // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                            build_inline_layout_recursive(
                                builder,
                                text_transformer,
                                nodes,
                                child_id,
                                &text_transform,
                                node_ranges,
                            );
                        }
                        builder.pop_style_span();
                    }
                    (DisplayOutside::Inline, DisplayInside::Flow) => {
                        let tag_name = &element_data.name.local;

                        if is_inline_box_element(tag_name) {
                            if box_kind == InlineBoxKind::InFlow {
                                text_transformer.word_break(builder);
                            }
                            builder.push_inline_box(InlineBox {
                                id: node_id.as_u64(),
                                kind: box_kind,
                                // Overridden by push_inline_box method
                                index: 0,
                                // Width and height are set during layout
                                width: 0.0,
                                height: 0.0,
                                baseline: None,
                                vertical_align: node
                                    .primary_styles()
                                    .map(|s| stylo_to_parley::vertical_align(&s))
                                    .unwrap_or_default(),
                            });
                        } else if *tag_name == local_name!("br") {
                            // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                            // TODO: update span id for br spans
                            builder.push_style_modification_span(&[
                                parley::StyleProperty::WhiteSpaceCollapse(
                                    WhiteSpaceCollapse::Preserve,
                                ),
                            ]);
                            builder.push_text("\n");
                            builder.pop_style_span();
                            text_transformer.word_break(builder);
                        } else {
                            // Inline bidi isolation: wrap the span's
                            // text in the controls its unicode-bidi implies.

                            // Cf chars shape to nothing and TextBrush spans are
                            // node-id keyed, so only the resolver sees them.
                            let bidi_wrap = style.and_then(|s| {
                                use style::computed_values::direction::T as Direction;
                                use style::computed_values::unicode_bidi::T as UnicodeBidi;
                                let ub = s.clone_unicode_bidi();
                                if matches!(ub, UnicodeBidi::Normal) {
                                    return None;
                                }
                                let rtl = s.clone_direction() == Direction::Rtl;
                                Some(bidi_wrap_chars(ub, rtl))
                            });

                            // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                            let style = node
                                .primary_styles()
                                .map(|s| stylo_to_parley::style(node.id, &s))
                                .unwrap_or_else(|| parley::TextStyle {
                                    white_space_collapse: WhiteSpaceCollapse::Collapse,
                                    ..Default::default()
                                });

                            builder.push_style_span(style);

                            if let Some((open, _)) = bidi_wrap {
                                builder.push_text(open);
                            }

                            if let Some(before_id) = node.before() {
                                build_inline_layout_recursive(
                                    builder,
                                    text_transformer,
                                    nodes,
                                    before_id,
                                    &text_transform,
                                    node_ranges,
                                );
                            }

                            for child_id in node.flat_children().iter().copied() {
                                build_inline_layout_recursive(
                                    builder,
                                    text_transformer,
                                    nodes,
                                    child_id,
                                    &text_transform,
                                    node_ranges,
                                );
                            }
                            if let Some(after_id) = node.after() {
                                build_inline_layout_recursive(
                                    builder,
                                    text_transformer,
                                    nodes,
                                    after_id,
                                    &text_transform,
                                    node_ranges,
                                );
                            }

                            if let Some((_, close)) = bidi_wrap {
                                builder.push_text(close);
                            }

                            builder.pop_style_span();
                        }
                    }
                    // Inline box
                    (_, _) => {
                        if box_kind == InlineBoxKind::InFlow {
                            text_transformer.word_break(builder);
                        }
                        builder.push_inline_box(InlineBox {
                            id: node_id.as_u64(),
                            kind: box_kind,
                            // Overridden by push_inline_box method
                            index: 0,
                            // Width and height are set during layout
                            width: 0.0,
                            height: 0.0,
                            baseline: None,
                            vertical_align: node
                                .primary_styles()
                                .map(|s| stylo_to_parley::vertical_align(&s))
                                .unwrap_or_default(),
                        });
                    }
                };
            }
            NodeData::Text(data) => {
                // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                // dbg!(&data.content);

                let range_start = builder.current_text_len();
                let text =
                    text_transformer.transform(&data.content, parent_text_transform, builder);
                builder.push_text(text);
                let range_end = builder.current_text_len();
                if range_end > range_start {
                    node_ranges.push((node_id, range_start..range_end));
                }
            }
            NodeData::Comment { .. } => {
                // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            }
            // Shadow roots never sit in a child list; tolerate one
            // showing up here rather than killing the whole paint.
            NodeData::ShadowRoot(_) => {}
            NodeData::Document(_) => unreachable!(),
        }
    }
}
