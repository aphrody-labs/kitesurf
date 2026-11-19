use cssparser::Parser;
use kurbo::{Affine, Rect as KurboRect};
use linebender_resource_handle::Blob;
use markup5ever::{ExpandedName, LocalName, Namespace, Prefix, QualName, local_name, ns};
use selectors::matching::{ElementSelectorFlags, QuirksMode};
use std::cell::OnceCell;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use style::Atom;
use style::parser::ParserContext;
use style::properties::{Importance, PropertyDeclaration, PropertyId, SourcePropertyDeclaration};
use style::stylesheets::{DocumentStyleSheet, Origin, UrlExtraData};
use style::values::computed::Display as StyloDisplay;
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use style::{
    properties::{PropertyDeclarationBlock, parse_style_attribute},
    servo_arc::Arc as ServoArc,
    shared_lock::{Locked, SharedRwLock},
    stylesheets::CssRuleType,
};
use style_dom::ElementState;
use style_traits::ParsingMode;
use taffy::{Cache, prelude::Layout};
use thin_vec::ThinVec;
use url::Url;

use super::stylo_data::{NodeStateFlags, StyloCell, StyloData, StyloDataMut};
#[cfg(feature = "svg")]
use super::svg::SvgImageData;
use super::{Attribute, Attributes, OptNodeId};
use blitz_traits::node_id::NodeId;
use crate::Document;
use crate::layout::table::TableContext;
use crate::node::{TextInputData, TextLayout};

#[cfg(feature = "custom-widget")]
use super::custom_widget::CustomWidgetData;

macro_rules! local_names {
    ($($name:tt),+) => {
        [$(local_name!($name),)+]
    };
}

unsafe impl crate::pool::Pooled for ElementData {
    fn pool() -> &'static std::sync::Mutex<crate::pool::Pool> {
        static POOL: std::sync::Mutex<crate::pool::Pool> =
            std::sync::Mutex::new(crate::pool::Pool::new());
        &POOL
    }
}

pub struct ElementData {
    /// The element's local name; see [`Self::namespace`] and [`Self::prefix`] for the rest.
    pub name: ElementName,

    /// The elements id attribute parsed as an atom (if it has one)
    pub id: Option<Atom>,

    /// The element's class attribute pre-split and interned. Selector
    /// matching hits this per candidate, so it must not re-parse per call.
    /// Thin, as most elements have no class.
    pub classes: ThinVec<Atom>,

    /// The element's attributes
    pub attrs: Attributes,

    /// Links, the style attribute, image layers and special data, which few elements have.
    rare: Option<Box<ElementRare>>,
    // /// Whether the node is a [HTML integration point] (https://html.spec.whatwg.org/multipage/#html-integration-point)
    // pub mathml_annotation_xml_integration_point: bool,

    // ---------------------------------------------------------------------
    // Fields moved from `Node`. These live on the element data so that the
    // `Node` struct itself only carries tree-structure information.
    // ---------------------------------------------------------------------
    /// Style data from stylo; read through [`Self::stylo_data`].
    pub(crate) stylo_element_data: StyloCell,
    pub element_state: ElementStateBits,
    /// Whether `stylo_element_data` is initialized, the traversal flags, the selector flags,
    /// the [`ElementBits`] and `display_constructed_as`, packed into one word.
    pub(crate) state_flags: NodeStateFlags,
    /// Layout output state (`None` until layout first writes to this node).
    /// Allocated by the first layout write or derived child list.
    pub layout_data: OnceCell<crate::pool::PoolBox<LayoutData>>,
}

// Pooled per element: 56 bytes on wasm32 release, where the stylo wrapper is 16.
#[cfg(target_pointer_width = "32")]
const _: () = assert!(
    std::mem::size_of::<ElementData>()
        <= (40 + std::mem::size_of::<style::data::ElementDataWrapper>()).next_multiple_of(8)
);

/// Taffy layout output state (cache, layouts, scroll offset and overflow).
///
/// Lazily boxed on [`ElementData`] / [`DocumentData`]: inline-level elements
/// are positioned by the inline (parley) layout rather than by taffy, so most
/// nodes on text-heavy pages never allocate one.
unsafe impl crate::pool::Pooled for LayoutData {
    fn pool() -> &'static std::sync::Mutex<crate::pool::Pool> {
        static POOL: std::sync::Mutex<crate::pool::Pool> =
            std::sync::Mutex::new(crate::pool::Pool::new());
        &POOL
    }
}

/// The fields of a taffy [`Layout`] kept inline in [`LayoutData`]: border, padding and
/// scrollbar size are zero on three in four boxes, so they live in [`BoxEdges`] instead.
#[derive(Debug, Clone, Copy)]
struct InlineLayout {
    order: u32,
    location: taffy::Point<f32>,
    size: taffy::Size<f32>,
    scrollable_overflow_rect: taffy::Rect<f32>,
    margin: taffy::Rect<f32>,
}

impl InlineLayout {
    const ZERO: Self = Self::of(&Layout::new());

    const fn of(layout: &Layout) -> Self {
        Self {
            order: layout.order,
            location: layout.location,
            size: layout.size,
            scrollable_overflow_rect: layout.scrollable_overflow_rect,
            margin: layout.margin,
        }
    }

    fn with(&self, edges: &Edges) -> Layout {
        Layout {
            order: self.order,
            location: self.location,
            size: self.size,
            scrollable_overflow_rect: self.scrollable_overflow_rect,
            scrollbar_size: edges.scrollbar_size,
            border: edges.border,
            padding: edges.padding,
            margin: self.margin,
        }
    }
}

/// The out-of-line part of one taffy [`Layout`].
#[derive(Debug, Clone, Copy, PartialEq)]
struct Edges {
    border: taffy::Rect<f32>,
    padding: taffy::Rect<f32>,
    scrollbar_size: taffy::Size<f32>,
}

impl Edges {
    const ZERO: Self = Self::of(&Layout::new());

    const fn of(layout: &Layout) -> Self {
        Self {
            border: layout.border,
            padding: layout.padding,
            scrollbar_size: layout.scrollbar_size,
        }
    }

    /// All bits zero, so that leaving these edges out (and reading `ZERO` back) is exact.
    fn is_zero(&self) -> bool {
        let r = |r: &taffy::Rect<f32>| [r.left, r.right, r.top, r.bottom];
        let s = &self.scrollbar_size;
        r(&self.border)
            .into_iter()
            .chain(r(&self.padding))
            .chain([s.width, s.height])
            .all(|v| v.to_bits() == 0)
    }
}

/// Both layouts' edges, allocated only while one of them is non-zero.
#[derive(Debug, Clone, Copy)]
struct BoxEdges {
    unrounded: Edges,
    rounded: Edges,
}

unsafe impl crate::pool::Pooled for BoxEdges {
    fn pool() -> &'static std::sync::Mutex<crate::pool::Pool> {
        static POOL: std::sync::Mutex<crate::pool::Pool> =
            std::sync::Mutex::new(crate::pool::Pool::new());
        &POOL
    }
}

impl Default for BoxEdges {
    fn default() -> Self {
        Self {
            unrounded: Edges::ZERO,
            rounded: Edges::ZERO,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LayoutData {
    pub cache: Cache,
    /// Taffy's unrounded and rounded layouts, split between these and `edges`; read through
    /// `unrounded_layout` and `final_layout`.
    unrounded: InlineLayout,
    rounded: InlineLayout,
    edges: Option<crate::pool::PoolBox<BoxEdges>>,
    /// Allocated by the first offset written: few boxes are scrolled or pinned.
    offsets: Option<Box<BoxOffsets>>,
    pub scrollable_overflow: KurboRect,
    /// The writing mode this node's layout algorithm ran in (the root takes it from `<body>`),
    /// recorded by the layout pass for CSSOM geometry lookups.
    #[cfg(feature = "writing-mode")]
    pub writing_mode: stylo_taffy::WritingMode,
    /// Parley text layout (elements with inline inner display mode only). Inline roots are
    /// laid out by taffy too, so it shares their box; see [`ElementData::inline_layout_data`].
    inline_layout: Option<Box<TextLayout>>,
    /// Derived layout and paint children; read through `Node::layout_children`.
    pub(crate) child_lists: super::node::LayoutChildLists,
}

impl LayoutData {
    pub const fn new() -> Self {
        Self {
            child_lists: super::node::LayoutChildLists::new(),
            cache: Cache::new(),
            unrounded: InlineLayout::ZERO,
            rounded: InlineLayout::ZERO,
            edges: None,
            offsets: None,
            scrollable_overflow: KurboRect::ZERO,
            #[cfg(feature = "writing-mode")]
            writing_mode: stylo_taffy::WritingMode::empty(),
            inline_layout: None,
        }
    }
}

impl Default for LayoutData {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy)]
struct BoxOffsets {
    /// Post-layout shift currently baked into `final_layout.location`.
    pin: taffy::Point<f32>,
    scroll: crate::Point<f64>,
}

static NO_OFFSETS: BoxOffsets = BoxOffsets {
    pin: taffy::Point { x: 0.0, y: 0.0 },
    scroll: crate::Point::ZERO,
};

impl LayoutData {
    /// Taffy's unrounded layout of this box.
    #[inline]
    pub fn unrounded_layout(&self) -> Layout {
        let edges = self.edges.as_deref().map_or(&Edges::ZERO, |edges| &edges.unrounded);
        self.unrounded.with(edges)
    }

    /// This box's rounded layout, with any sticky pin shift baked into its location.
    #[inline]
    pub fn final_layout(&self) -> Layout {
        let edges = self.edges.as_deref().map_or(&Edges::ZERO, |edges| &edges.rounded);
        self.rounded.with(edges)
    }

    pub fn set_unrounded_layout(&mut self, layout: &Layout) {
        self.unrounded = InlineLayout::of(layout);
        self.set_edges(Edges::of(layout), |edges| &mut edges.unrounded);
    }

    pub fn set_final_layout(&mut self, layout: &Layout) {
        self.rounded = InlineLayout::of(layout);
        self.set_edges(Edges::of(layout), |edges| &mut edges.rounded);
    }

    /// Stores one layout's edges, keeping the box exactly while either layout's are non-zero.
    fn set_edges(&mut self, value: Edges, slot: fn(&mut BoxEdges) -> &mut Edges) {
        match self.edges.as_deref_mut() {
            Some(edges) => {
                *slot(edges) = value;
                if edges.unrounded.is_zero() && edges.rounded.is_zero() {
                    self.edges = None;
                }
            }
            None if value.is_zero() => {}
            None => {
                let mut edges = crate::pool::PoolBox::<BoxEdges>::default();
                *slot(&mut edges) = value;
                self.edges = Some(edges);
            }
        }
    }

    /// Moves the rounded layout's location by `(dx, dy)`.
    pub(crate) fn shift_final_location(&mut self, dx: f32, dy: f32) {
        self.rounded.location.x += dx;
        self.rounded.location.y += dy;
    }

    fn offsets(&self) -> &BoxOffsets {
        self.offsets.as_deref().unwrap_or(&NO_OFFSETS)
    }

    fn offsets_mut(&mut self) -> &mut BoxOffsets {
        self.offsets.get_or_insert_with(|| Box::new(NO_OFFSETS))
    }

    pub fn pin_offset(&self) -> &taffy::Point<f32> {
        &self.offsets().pin
    }

    pub fn set_pin_offset(&mut self, pin: taffy::Point<f32>) {
        if self.offsets.is_some() || pin != NO_OFFSETS.pin {
            self.offsets_mut().pin = pin;
        }
    }

    pub fn scroll_offset(&self) -> &crate::Point<f64> {
        &self.offsets().scroll
    }

    pub fn scroll_offset_mut(&mut self) -> &mut crate::Point<f64> {
        &mut self.offsets_mut().scroll
    }
}

/// Shared read-only default returned by accessors for nodes whose
/// [`LayoutData`] has never been allocated.
pub(crate) static DEFAULT_LAYOUT_DATA: SharedLayoutData = SharedLayoutData(LayoutData::new());

pub(crate) struct SharedLayoutData(pub(crate) LayoutData);

// SAFETY: the shared default is only ever read. Its one interior-mutable part, the child
// lists, is reached only through `Node::child_lists`, which never yields the default.
unsafe impl Sync for SharedLayoutData {}

type ImageLayers = ThinVec<Option<ImageResourceData>>;

/// Store a resolved transform, reusing the existing allocation when there is one.
pub(crate) fn store_transform(slot: &mut Option<Box<Affine>>, transform: Option<Affine>) {
    match (slot.as_deref_mut(), transform) {
        (Some(existing), Some(new)) => *existing = new,
        (None, Some(new)) => *slot = Some(Box::new(new)),
        (_, None) => *slot = None,
    }
}

/// The local part of an element's name.
///
/// The namespace and prefix are kept apart, as a byte for the common namespaces, so an element
/// stores one atom of its name rather than three.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ElementName {
    pub local: LocalName,
}

/// The namespace of an element's name. `Other` covers any other namespace or a prefix, which
/// are kept in [`ElementRare::name_ext`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum NameNs {
    Html,
    Svg,
    MathMl,
    Empty,
    Other,
}

/// An element's [`NameNs`] and its plain boolean flags, packed into five bits of its
/// [`NodeStateFlags`] word.
#[derive(Clone, Copy)]
struct ElementBits(u8);

impl ElementBits {
    const NS: u8 = 0b111;
    const FOCUSSABLE: u8 = 1 << 3;
    const HAS_SNAPSHOT: u8 = 1 << 4;

    const fn new(ns: NameNs) -> Self {
        Self(ns as u8)
    }

    #[inline]
    fn name_ns(self) -> NameNs {
        match self.0 & Self::NS {
            0 => NameNs::Html,
            1 => NameNs::Svg,
            2 => NameNs::MathMl,
            3 => NameNs::Empty,
            _ => NameNs::Other,
        }
    }

    #[inline]
    fn get(self, bit: u8) -> bool {
        self.0 & bit != 0
    }

    #[inline]
    fn set(&mut self, bit: u8, value: bool) {
        if value {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }
}

const _: () = assert!(NameNs::Other as u8 <= ElementBits::NS);
const _: () = assert!(
    (ElementBits::NS | ElementBits::FOCUSSABLE | ElementBits::HAS_SNAPSHOT) as u32
        <= NodeStateFlags::ELEMENT_BITS_MASK >> NodeStateFlags::ELEMENT_BITS_SHIFT
);
const _: () = assert!(ElementSelectorFlags::all().bits() < 1 << 12);

/// Every [`DisplayInside`], indexed by discriminant.
const DISPLAY_INSIDE: [DisplayInside; 15] = {
    use DisplayInside::*;
    [
        None,
        Contents,
        Flow,
        FlowRoot,
        Flex,
        Grid,
        Table,
        TableRowGroup,
        TableColumn,
        TableColumnGroup,
        TableHeaderGroup,
        TableFooterGroup,
        TableRow,
        TableCell,
        WebkitBox,
    ]
};

/// Every [`DisplayOutside`], indexed by discriminant.
const DISPLAY_OUTSIDE: [DisplayOutside; 5] = {
    use DisplayOutside::*;
    [None, Inline, Block, TableCaption, InternalTable]
};

// The tables must list every variant (these matches are exhaustive) at its discriminant.
const _: fn(DisplayInside) = |inside| match inside {
    DisplayInside::None
    | DisplayInside::Contents
    | DisplayInside::Flow
    | DisplayInside::FlowRoot
    | DisplayInside::Flex
    | DisplayInside::Grid
    | DisplayInside::Table
    | DisplayInside::TableRowGroup
    | DisplayInside::TableColumn
    | DisplayInside::TableColumnGroup
    | DisplayInside::TableHeaderGroup
    | DisplayInside::TableFooterGroup
    | DisplayInside::TableRow
    | DisplayInside::TableCell
    | DisplayInside::WebkitBox => {}
};
const _: fn(DisplayOutside) = |outside| match outside {
    DisplayOutside::None
    | DisplayOutside::Inline
    | DisplayOutside::Block
    | DisplayOutside::TableCaption
    | DisplayOutside::InternalTable => {}
};
const _: () = {
    let mut i = 0;
    while i < DISPLAY_INSIDE.len() {
        assert!(DISPLAY_INSIDE[i] as usize == i);
        i += 1;
    }
    let mut i = 0;
    while i < DISPLAY_OUTSIDE.len() {
        assert!(DISPLAY_OUTSIDE[i] as usize == i);
        i += 1;
    }
    assert!((DisplayInside::WebkitBox as u32) < 1 << DISPLAY_OUTSIDE_SHIFT);
    let outside_bits = DISPLAY_LIST_ITEM_SHIFT - DISPLAY_OUTSIDE_SHIFT;
    assert!((DisplayOutside::InternalTable as u32) < 1 << outside_bits);
};

/// Position of the outside value and the list-item bit in a display code.
const DISPLAY_OUTSIDE_SHIFT: u32 = 5;
const DISPLAY_LIST_ITEM_SHIFT: u32 = 8;
/// The inside value in a display code: the bits below the outside value.
const DISPLAY_INSIDE_MASK: u32 = (1 << DISPLAY_OUTSIDE_SHIFT) - 1;
/// The outside value in a display code: the bits between the inside value and the list-item bit.
const DISPLAY_OUTSIDE_MASK: u32 = (1 << (DISPLAY_LIST_ITEM_SHIFT - DISPLAY_OUTSIDE_SHIFT)) - 1;

/// A [`StyloDisplay`] as a 9-bit code: inside, outside and list-item. Stylo's `Display` is
/// exactly `from3` of these three, so [`display_from_code`] restores it.
#[inline]
fn display_code(display: StyloDisplay) -> u32 {
    display.inside() as u32
        | (display.outside() as u32) << DISPLAY_OUTSIDE_SHIFT
        | u32::from(display.is_list_item()) << DISPLAY_LIST_ITEM_SHIFT
}

#[inline]
fn display_from_code(code: u32) -> StyloDisplay {
    let inside = DISPLAY_INSIDE[(code & DISPLAY_INSIDE_MASK) as usize];
    let outside = (code >> DISPLAY_OUTSIDE_SHIFT) & DISPLAY_OUTSIDE_MASK;
    StyloDisplay::from3(
        DISPLAY_OUTSIDE[outside as usize],
        inside,
        code & 1 << DISPLAY_LIST_ITEM_SHIFT != 0,
    )
}

/// The fields of the [`NodeStateFlags`] word besides its state bits (12 to 15): selector
/// flags in bits 0 to 11, [`ElementBits`] in 16 to 20 (zero on the document) and the
/// `display_constructed_as` code in 21 to 29.
impl NodeStateFlags {
    const SELECTOR_MASK: u32 = (1 << 12) - 1;
    const ELEMENT_BITS_SHIFT: u32 = 16;
    const ELEMENT_BITS_MASK: u32 = 0b1_1111 << Self::ELEMENT_BITS_SHIFT;
    const DISPLAY_SHIFT: u32 = 21;
    const DISPLAY_MASK: u32 = ((1 << (DISPLAY_LIST_ITEM_SHIFT + 1)) - 1) << Self::DISPLAY_SHIFT;

    /// Empty selector flags, the initial state bits, `bits` and a `Block` display.
    fn for_node(bits: ElementBits) -> Self {
        Self(AtomicU32::new(
            Self::INITIAL_STATE
                | u32::from(bits.0) << Self::ELEMENT_BITS_SHIFT
                | display_code(StyloDisplay::Block) << Self::DISPLAY_SHIFT,
        ))
    }

    #[inline]
    fn selector_flags(&self) -> ElementSelectorFlags {
        let bits = self.0.load(Ordering::Relaxed) & Self::SELECTOR_MASK;
        ElementSelectorFlags::from_bits_retain(bits as usize)
    }

    /// Replaces the selector flags, leaving the rest of the word unchanged.
    #[inline]
    fn set_selector_flags(&self, flags: ElementSelectorFlags) {
        let bits = flags.bits() as u32 & Self::SELECTOR_MASK;
        let _ = self.0.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |word| {
            Some(word & !Self::SELECTOR_MASK | bits)
        });
    }

    #[inline]
    fn element_bits(&self) -> ElementBits {
        let word = self.0.load(Ordering::Relaxed);
        ElementBits(((word & Self::ELEMENT_BITS_MASK) >> Self::ELEMENT_BITS_SHIFT) as u8)
    }

    #[inline]
    fn set_element_bits(&mut self, bits: ElementBits) {
        let word = self.0.get_mut();
        *word = *word & !Self::ELEMENT_BITS_MASK | u32::from(bits.0) << Self::ELEMENT_BITS_SHIFT;
    }

    #[inline]
    fn display(&self) -> StyloDisplay {
        let word = self.0.load(Ordering::Relaxed);
        display_from_code((word & Self::DISPLAY_MASK) >> Self::DISPLAY_SHIFT)
    }

    #[inline]
    fn set_display(&mut self, display: StyloDisplay) {
        let word = self.0.get_mut();
        *word = *word & !Self::DISPLAY_MASK | display_code(display) << Self::DISPLAY_SHIFT;
    }
}

// The word's fields are disjoint.
const _: () = {
    let masks = [
        NodeStateFlags::SELECTOR_MASK,
        NodeStateFlags::STATE_MASK,
        NodeStateFlags::ELEMENT_BITS_MASK,
        NodeStateFlags::DISPLAY_MASK,
    ];
    let (mut union, mut ones, mut i) = (0u32, 0, 0);
    while i < masks.len() {
        union |= masks[i];
        ones += masks[i].count_ones();
        i += 1;
    }
    assert!(union.count_ones() == ones);
};

/// [`ElementState`] stored in 32 bits. Blitz sets only states in bits 0 to 31 (focus ring is
/// bit 30); storing a higher one panics rather than dropping it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ElementStateBits(u32);

impl ElementStateBits {
    #[inline]
    pub fn get(self) -> ElementState {
        ElementState::from_bits_retain(u64::from(self.0))
    }

    #[inline]
    fn store(&mut self, state: ElementState) {
        self.0 = u32::try_from(state.bits()).expect("element states above bit 31 are not stored");
    }

    #[inline]
    pub fn contains(self, other: ElementState) -> bool {
        self.get().contains(other)
    }

    #[inline]
    pub fn intersects(self, other: ElementState) -> bool {
        self.get().intersects(other)
    }

    #[inline]
    pub fn insert(&mut self, other: ElementState) {
        self.store(self.get() | other);
    }

    #[inline]
    pub fn remove(&mut self, other: ElementState) {
        self.store(self.get() - other);
    }

    #[inline]
    pub fn set(&mut self, other: ElementState, value: bool) {
        let mut state = self.get();
        state.set(other, value);
        self.store(state);
    }
}

/// The [`ElementSelectorFlags`] of an element or document node, stored in 12 bits of its
/// state word, which hold every defined flag.
#[derive(Clone, Copy)]
pub struct SelectorFlagsRef<'a>(&'a NodeStateFlags);

impl SelectorFlagsRef<'_> {
    #[inline]
    pub fn get(self) -> ElementSelectorFlags {
        self.0.selector_flags()
    }

    /// Replaces the stored flags.
    #[inline]
    pub fn set(self, flags: ElementSelectorFlags) {
        self.0.set_selector_flags(flags);
    }
}

static NS_HTML: Namespace = ns!(html);
static NS_SVG: Namespace = ns!(svg);
static NS_MATHML: Namespace = ns!(mathml);
static NS_EMPTY: Namespace = ns!();

/// Element state kept out of line so a common element stays in a smaller allocation.
#[derive(Clone, Default)]
struct ElementRare {
    /// The prefix and namespace of an element whose name is [`NameNs::Other`].
    name_ext: Option<Box<(Option<Prefix>, Namespace)>>,
    /// The element's template contents (\<template\> elements only)
    template_contents: OptNodeId,
    /// This element's attached shadow root, when it is a shadow host. The
    /// shadow tree hangs off that node, not children.
    shadow_root: OptNodeId,
    /// Generated `::before` and `::after` pseudo element nodes.
    before: OptNodeId,
    after: OptNodeId,
    /// The element's parsed style attribute (used by stylo)
    style_attribute: Option<ServoArc<Locked<PropertyDeclarationBlock>>>,
    /// One slot per `background-image` / `mask-image` layer; empty when no layer is a URL.
    background_images: ImageLayers,
    mask_images: ImageLayers,
    /// Detailed layout information from the most recent layout (currently grid track sizing
    /// for grid containers only). Absent means `DetailedLayoutInfo::None`.
    detailed_layout_info: Option<taffy::DetailedLayoutInfo<Atom>>,
    /// Data associated with display: list-item. Note that this display mode
    /// does not exclude inline_layout_data
    list_item_data: Option<Box<ListItemLayout>>,
    /// The resolved 2D transform in device pixels, when the element has one.
    transform: Option<Box<Affine>>,
    /// Heterogeneous data that depends on the element's type, such as the image data for
    /// \<img\> or the text editor for input/textarea elements.
    special_data: SpecialElementData,
}

/// Shared read-only `None` returned by [`ElementData::special_data`] without a rare box.
static NO_SPECIAL_DATA: SharedSpecialData = SharedSpecialData(SpecialElementData::None);

struct SharedSpecialData(SpecialElementData);

// SAFETY: the static holds the payload-free `None` variant and is only ever read through a
// shared reference, so no thread can observe a non-`Sync` payload or a mutation.
unsafe impl Sync for SharedSpecialData {}

impl ElementRare {
    fn is_empty(&self) -> bool {
        self.name_ext.is_none()
            && self.template_contents.get().is_none()
            && self.shadow_root.get().is_none()
            && self.before.get().is_none()
            && self.after.get().is_none()
            && self.style_attribute.is_none()
            && self.background_images.is_empty()
            && self.mask_images.is_empty()
            && self.detailed_layout_info.is_none()
            && self.list_item_data.is_none()
            && self.transform.is_none()
            && matches!(self.special_data, SpecialElementData::None)
    }
}

impl ElementData {
    /// Whether the element is focussable.
    #[inline]
    pub fn is_focussable(&self) -> bool {
        self.state_flags.element_bits().get(ElementBits::FOCUSSABLE)
    }

    #[inline]
    pub fn has_snapshot(&self) -> bool {
        self.state_flags.element_bits().get(ElementBits::HAS_SNAPSHOT)
    }

    #[inline]
    pub fn set_has_snapshot(&mut self, value: bool) {
        self.set_element_bit(ElementBits::HAS_SNAPSHOT, value);
    }

    #[inline]
    fn set_element_bit(&mut self, bit: u8, value: bool) {
        let mut bits = self.state_flags.element_bits();
        bits.set(bit, value);
        self.state_flags.set_element_bits(bits);
    }

    /// Selector flags deposited by stylo's `apply_selector_flags`.
    #[inline]
    pub fn selector_flags(&self) -> SelectorFlagsRef<'_> {
        SelectorFlagsRef(&self.state_flags)
    }

    /// The display this element's box was last constructed with.
    #[inline]
    pub fn display_constructed_as(&self) -> StyloDisplay {
        self.state_flags.display()
    }

    #[inline]
    pub fn set_display_constructed_as(&mut self, display: StyloDisplay) {
        self.state_flags.set_display(display);
    }

    /// The namespace of the element's name.
    #[inline]
    pub fn namespace(&self) -> &Namespace {
        match self.state_flags.element_bits().name_ns() {
            NameNs::Html => &NS_HTML,
            NameNs::Svg => &NS_SVG,
            NameNs::MathMl => &NS_MATHML,
            NameNs::Empty => &NS_EMPTY,
            NameNs::Other => &self.name_ext().1,
        }
    }

    /// The prefix of the element's name, if any.
    #[inline]
    pub fn prefix(&self) -> Option<&Prefix> {
        match self.state_flags.element_bits().name_ns() {
            NameNs::Other => self.name_ext().0.as_ref(),
            _ => None,
        }
    }

    /// The element's name as namespace and local name.
    #[inline]
    pub fn expanded_name(&self) -> ExpandedName<'_> {
        ExpandedName {
            ns: self.namespace(),
            local: &self.name.local,
        }
    }

    /// The element's full name, including its prefix.
    pub fn qual_name(&self) -> QualName {
        QualName::new(
            self.prefix().cloned(),
            self.namespace().clone(),
            self.name.local.clone(),
        )
    }

    fn name_ext(&self) -> &(Option<Prefix>, Namespace) {
        self.rare
            .as_ref()
            .and_then(|rare| rare.name_ext.as_deref())
            .expect("an `Other` element name keeps its namespace in the rare box")
    }

    /// The element's template contents (\<template\> elements only).
    #[inline]
    pub fn template_contents(&self) -> Option<NodeId> {
        self.rare.as_ref()?.template_contents.get()
    }

    pub fn set_template_contents(&mut self, id: Option<NodeId>) {
        self.update_rare(|rare| rare.template_contents.set(id));
    }

    /// Node id of this element's attached shadow root, when it is a shadow host.
    #[inline]
    pub fn shadow_root(&self) -> Option<NodeId> {
        self.rare.as_ref()?.shadow_root.get()
    }

    pub fn set_shadow_root(&mut self, id: Option<NodeId>) {
        self.update_rare(|rare| rare.shadow_root.set(id));
    }

    /// The generated `::before` pseudo element node, if any.
    #[inline]
    pub fn before(&self) -> Option<NodeId> {
        self.rare.as_ref()?.before.get()
    }

    pub fn set_before(&mut self, id: Option<NodeId>) {
        self.update_rare(|rare| rare.before.set(id));
    }

    /// The generated `::after` pseudo element node, if any.
    #[inline]
    pub fn after(&self) -> Option<NodeId> {
        self.rare.as_ref()?.after.get()
    }

    pub fn set_after(&mut self, id: Option<NodeId>) {
        self.update_rare(|rare| rare.after.set(id));
    }

    /// The element's parsed style attribute (used by stylo).
    #[inline]
    pub fn style_attribute(&self) -> Option<&ServoArc<Locked<PropertyDeclarationBlock>>> {
        self.rare.as_ref()?.style_attribute.as_ref()
    }

    pub fn style_attribute_mut(&mut self) -> Option<&mut ServoArc<Locked<PropertyDeclarationBlock>>> {
        self.rare.as_mut()?.style_attribute.as_mut()
    }

    pub fn set_style_attribute(&mut self, block: Option<ServoArc<Locked<PropertyDeclarationBlock>>>) {
        self.update_rare(|rare| rare.style_attribute = block);
    }

    /// One slot per `background-image` layer; empty when no layer is a URL.
    #[inline]
    pub fn background_images(&self) -> &[Option<ImageResourceData>] {
        self.rare.as_ref().map_or(&[], |rare| &rare.background_images)
    }

    /// One slot per `mask-image` layer; empty when no layer is a URL.
    #[inline]
    pub fn mask_images(&self) -> &[Option<ImageResourceData>] {
        self.rare.as_ref().map_or(&[], |rare| &rare.mask_images)
    }

    /// The background and mask layer slots, for updating loaded images in place.
    pub fn image_layers_mut(
        &mut self,
    ) -> (&mut [Option<ImageResourceData>], &mut [Option<ImageResourceData>]) {
        match self.rare.as_deref_mut() {
            Some(rare) => (&mut rare.background_images, &mut rare.mask_images),
            None => (&mut [], &mut []),
        }
    }

    /// The background (`mask` false) or mask layer slots, allocating the rare box. Callers
    /// leave them non-empty; `clear_image_layers` empties them.
    pub(crate) fn image_layers_storage_mut(&mut self, mask: bool) -> &mut ImageLayers {
        let rare = self.rare.get_or_insert_default();
        if mask { &mut rare.mask_images } else { &mut rare.background_images }
    }

    pub(crate) fn clear_image_layers(&mut self, mask: bool) {
        self.update_rare(|rare| {
            *(if mask { &mut rare.mask_images } else { &mut rare.background_images }) =
                ThinVec::new();
        });
    }

    /// Detailed layout information from the most recent layout (currently grid track sizing
    /// for grid containers only). Used by devtools grid inspection and out-of-flow grid-area
    /// positioning.
    #[inline]
    pub fn detailed_layout_info(&self) -> &taffy::DetailedLayoutInfo<Atom> {
        self.rare
            .as_ref()
            .and_then(|rare| rare.detailed_layout_info.as_ref())
            .unwrap_or(&taffy::DetailedLayoutInfo::None)
    }

    /// Used grid track sizes from the most recent layout of a grid container.
    pub fn grid_info(&self) -> Option<&taffy::DetailedGridInfo<Atom>> {
        match self.detailed_layout_info() {
            taffy::DetailedLayoutInfo::Grid(info) => Some(info),
            _ => None,
        }
    }

    pub fn set_grid_info(&mut self, info: taffy::DetailedGridInfo<Atom>) {
        let info = taffy::DetailedLayoutInfo::Grid(Box::new(info));
        self.update_rare(|rare| rare.detailed_layout_info = Some(info));
    }

    pub(crate) fn clear_detailed_layout_info(&mut self) {
        self.update_rare(|rare| rare.detailed_layout_info = None);
    }

    /// Data associated with display: list-item, if the element is a list item.
    #[inline]
    pub fn list_item_data(&self) -> Option<&ListItemLayout> {
        self.rare.as_ref()?.list_item_data.as_deref()
    }

    pub fn set_list_item_data(&mut self, data: Option<Box<ListItemLayout>>) {
        self.update_rare(|rare| rare.list_item_data = data);
    }

    /// The resolved 2D transform in device pixels, when the element has one.
    #[inline]
    pub fn transform(&self) -> Option<Affine> {
        self.rare.as_ref()?.transform.as_deref().copied()
    }

    pub fn set_transform(&mut self, transform: Option<Affine>) {
        self.update_rare(|rare| store_transform(&mut rare.transform, transform));
    }

    /// Heterogeneous data that depends on the element's type. For example the image data for
    /// \<img\> elements or the text editor for input/textarea elements.
    #[inline]
    pub fn special_data(&self) -> &SpecialElementData {
        self.rare.as_ref().map_or(&NO_SPECIAL_DATA.0, |rare| &rare.special_data)
    }

    /// Mutable access to the special data; `None` when it is `SpecialElementData::None`.
    /// Never allocates the rare box.
    #[inline]
    pub fn special_data_mut(&mut self) -> Option<&mut SpecialElementData> {
        match &mut self.rare.as_deref_mut()?.special_data {
            SpecialElementData::None => None,
            data => Some(data),
        }
    }

    /// Replaces the special data; storing `None` never allocates the rare box.
    pub fn set_special_data(&mut self, data: SpecialElementData) {
        self.update_rare(|rare| rare.special_data = data);
    }

    /// Takes the special data, leaving `None`.
    pub fn take_special_data(&mut self) -> SpecialElementData {
        let mut taken = SpecialElementData::None;
        self.update_rare(|rare| taken = rare.special_data.take());
        taken
    }

    // Updates an absent box on a stack default, so clearing a field never allocates.
    fn update_rare(&mut self, update: impl FnOnce(&mut ElementRare)) {
        match self.rare.as_deref_mut() {
            Some(rare) => {
                update(rare);
                if rare.is_empty() {
                    self.rare = None;
                }
            }
            None => {
                let mut rare = ElementRare::default();
                update(&mut rare);
                if !rare.is_empty() {
                    self.rare = Some(Box::new(rare));
                }
            }
        }
    }

    /// This element's layout output state, or a shared default if layout has
    /// never written to this element.
    #[inline]
    pub fn layout_data(&self) -> &LayoutData {
        self.layout_data.get().map_or(&DEFAULT_LAYOUT_DATA.0, |data| data)
    }

    /// Mutable access to this element's layout output state, allocating it if
    /// it does not yet exist.
    #[inline]
    pub fn layout_data_mut(&mut self) -> &mut LayoutData {
        layout_data_mut(&mut self.layout_data)
    }

    /// Parley text layout (elements with inline inner display mode only).
    #[inline]
    pub fn inline_layout_data(&self) -> Option<&TextLayout> {
        self.layout_data.get()?.inline_layout.as_deref()
    }

    #[inline]
    pub fn inline_layout_data_mut(&mut self) -> Option<&mut TextLayout> {
        self.layout_data.get_mut()?.inline_layout.as_deref_mut()
    }

    /// Sets the text layout; clearing it never allocates the layout box.
    pub fn set_inline_layout_data(&mut self, layout: Option<Box<TextLayout>>) {
        match layout {
            Some(layout) => self.layout_data_mut().inline_layout = Some(layout),
            None => {
                self.take_inline_layout_data();
            }
        }
    }

    pub fn take_inline_layout_data(&mut self) -> Option<Box<TextLayout>> {
        self.layout_data.get_mut()?.inline_layout.take()
    }
}

#[inline]
fn layout_data_mut(cell: &mut OnceCell<crate::pool::PoolBox<LayoutData>>) -> &mut LayoutData {
    if cell.get().is_none() {
        let _ = cell.set(crate::pool::PoolBox::default());
    }
    cell.get_mut().expect("layout data was just allocated")
}

impl DocumentData {
    /// The document node's layout output state, or a shared default if layout
    /// has never written to it.
    #[inline]
    pub fn layout_data(&self) -> &LayoutData {
        self.layout_data.get().map_or(&DEFAULT_LAYOUT_DATA.0, |data| data)
    }

    /// Mutable access to the document node's layout output state, allocating
    /// it if it does not yet exist.
    #[inline]
    pub fn layout_data_mut(&mut self) -> &mut LayoutData {
        layout_data_mut(&mut self.layout_data)
    }
}

/// Data specific to the [`Document`](super::super::Document) root node.
///
/// The document node participates in layout and styling like an element, so it
/// carries the same style/layout fields that were previously stored directly on
/// [`Node`](super::Node).
pub struct DocumentData {
    pub(crate) stylo_element_data: StyloCell,
    /// See [`ElementData`]'s field of the same name. Its selector flags are deposited by
    /// `apply_selector_flags` when a `for_parent()` flag is applied while matching the root
    /// `<html>` element, whose parent node is the document.
    pub(crate) state_flags: NodeStateFlags,
    pub element_state: ElementStateBits,
    pub has_snapshot: bool,
    /// Layout output state (`None` until layout first writes to this node).
    pub layout_data: OnceCell<crate::pool::PoolBox<LayoutData>>,
    pub transform: Option<Box<Affine>>,
}

// Hand-written like `ElementData`'s, because `ElementSelectorFlags` does not
// implement `Debug`. Every other field is still reported.
impl std::fmt::Debug for DocumentData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentData")
            .field("stylo_element_data", &self.stylo_data())
            .field("dirty_descendants", &self.state_flags.dirty_descendants())
            .field("damaged_descendants", &self.state_flags.damaged_descendants())
            .field("element_state", &self.element_state)
            .field("has_snapshot", &self.has_snapshot)
            .field("snapshot_handled", &self.state_flags.snapshot_handled())
            .field("display_constructed_as", &self.display_constructed_as())
            .field("layout_data", &self.layout_data)
            .field("transform", &self.transform)
            .finish_non_exhaustive()
    }
}

impl DocumentData {
    pub fn new() -> Self {
        Self {
            stylo_element_data: Default::default(),
            state_flags: NodeStateFlags::for_node(ElementBits(0)),
            element_state: ElementStateBits::default(),
            has_snapshot: false,
            layout_data: OnceCell::new(),
            transform: None,
        }
    }

    /// See [`ElementData::selector_flags`].
    #[inline]
    pub fn selector_flags(&self) -> SelectorFlagsRef<'_> {
        SelectorFlagsRef(&self.state_flags)
    }

    #[inline]
    pub fn display_constructed_as(&self) -> StyloDisplay {
        self.state_flags.display()
    }

    #[inline]
    pub fn set_display_constructed_as(&mut self, display: StyloDisplay) {
        self.state_flags.set_display(display);
    }

    #[inline]
    pub(crate) fn stylo_data(&self) -> StyloData<'_> {
        StyloData::new(&self.stylo_element_data, &self.state_flags)
    }

    #[inline]
    pub(crate) fn stylo_data_mut(&mut self) -> StyloDataMut<'_> {
        StyloDataMut::new(&mut self.stylo_element_data, &self.state_flags)
    }
}

impl Default for DocumentData {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for DocumentData {
    fn clone(&self) -> Self {
        // Runtime style/layout state is reset (the document node is not
        // meaningfully cloneable), matching `ElementData`'s clone semantics.
        Self::new()
    }
}

impl std::fmt::Debug for ElementData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ElementData")
            .field("name", &self.qual_name())
            .field("id", &self.id)
            .field("attrs", &self.attrs)
            .field("is_focussable", &self.is_focussable())
            .field("style_attribute", &self.style_attribute())
            .field("special_data", self.special_data())
            .field("background_images", &self.background_images())
            .field("mask_images", &self.mask_images())
            .field("inline_layout_data", &self.inline_layout_data())
            .field("list_item_data", &self.list_item_data())
            .field("template_contents", &self.template_contents())
            .field("element_state", &self.element_state)
            .field("display_constructed_as", &self.display_constructed_as())
            .finish_non_exhaustive()
    }
}

impl Clone for ElementData {
    /// Clones the *content* of the element (name, attributes, style attribute,
    /// special data, etc.). Runtime style/layout state (stylo data, taffy
    /// layout, caches, pseudo-element ids, ...) is reset to its default so that
    /// the clone behaves like a freshly-created element that has not yet been
    /// styled or laid out.
    fn clone(&self) -> Self {
        let mut bits = self.state_flags.element_bits();
        bits.set(ElementBits::HAS_SNAPSHOT, false);
        Self {
            name: self.name.clone(),
            id: self.id.clone(),
            attrs: self.attrs.clone(),
            classes: self.classes.clone(),
            // Shadow roots and pseudo element nodes are never cloned.
            rare: self.rare.as_deref().and_then(|rare| {
                let clone = ElementRare {
                    name_ext: rare.name_ext.clone(),
                    template_contents: rare.template_contents,
                    style_attribute: rare.style_attribute.clone(),
                    background_images: rare.background_images.clone(),
                    mask_images: rare.mask_images.clone(),
                    special_data: rare.special_data.clone(),
                    ..ElementRare::default()
                };
                (!clone.is_empty()).then(|| Box::new(clone))
            }),

            // Runtime state: reset to defaults.
            stylo_element_data: Default::default(),
            element_state: self.element_state,
            state_flags: NodeStateFlags::for_node(bits),
            layout_data: OnceCell::new(),
        }
    }
}

#[derive(Copy, Clone, Default)]
#[non_exhaustive]
pub enum SpecialElementType {
    Stylesheet,
    Image,
    Canvas,
    TableRoot,
    TextInput,
    CheckboxInput,
    #[cfg(feature = "file-input")]
    FileInput,
    #[default]
    None,
}

/// Heterogeneous data that depends on the element's type.
#[derive(Default)]
pub enum SpecialElementData {
    // Rare payloads wider than a pointer are boxed, keeping the enum (and every
    // element) at a tag plus one 32-bit pointer.
    /// A sub-document such an \<iframe\> or \<web-view\> element
    SubDocument(Box<Box<dyn Document>>),
    /// A custom widget
    #[cfg(feature = "custom-widget")]
    CustomWidget(CustomWidgetData),
    /// A stylesheet
    Stylesheet(DocumentStyleSheet),
    /// An \<img\> element's image data
    Image(Box<ImageData>),
    /// A \<canvas\> element's custom paint source
    Canvas(Box<CanvasData>),
    /// Pre-computed table layout data
    TableRoot(Arc<TableContext>),
    /// Parley text editor (text inputs)
    TextInput(Box<TextInputData>),
    /// Checkbox checked state
    CheckboxInput(bool),
    /// Live IDL value of an `<input type=range>` slider.
    /// The paint layer combines it with the min/max/step attributes
    /// to place the thumb; JS updates it via the setInputText wasm
    /// entry (same IDL-vs-attribute decoupling as TextInput's
    /// value_override).
    RangeInput(Box<f64>),
    /// Selected files
    #[cfg(feature = "file-input")]
    FileInput(FileData),
    /// No data (for nodes that don't need any node-specific data)
    #[default]
    None,
}

impl Clone for SpecialElementData {
    fn clone(&self) -> Self {
        match self {
            Self::SubDocument(_) => Self::None, // TODO
            #[cfg(feature = "custom-widget")]
            Self::CustomWidget(_) => Self::None, // TODO
            Self::Stylesheet(data) => Self::Stylesheet(data.clone()),
            Self::Image(data) => Self::Image(data.clone()),
            Self::Canvas(data) => Self::Canvas(data.clone()),
            Self::TableRoot(data) => Self::TableRoot(data.clone()),
            Self::TextInput(data) => Self::TextInput(data.clone()),
            Self::CheckboxInput(data) => Self::CheckboxInput(*data),
            Self::RangeInput(data) => Self::RangeInput(data.clone()),
            #[cfg(feature = "file-input")]
            Self::FileInput(data) => Self::FileInput(data.clone()),
            Self::None => Self::None,
        }
    }
}

impl SpecialElementData {
    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

/// Split a `class` attribute into interned atoms. The single place that
/// decides tokenisation, so the cached list can't drift from the attribute.
pub fn parse_class_list(value: &str) -> ThinVec<Atom> {
    value.split_ascii_whitespace().map(Atom::from).collect()
}

impl ElementData {
    pub fn new(name: QualName, attrs: impl Into<Attributes>) -> Self {
        let attrs: Attributes = attrs.into();
        let id_attr_atom = attrs
            .iter()
            .find(|attr| &attr.name.local == "id")
            .map(|attr| attr.value.as_ref())
            .map(|value: &str| Atom::from(value));

        let class_atoms = attrs
            .iter()
            .find(|attr| &attr.name.local == "class")
            .map(|attr| parse_class_list(attr.value.as_ref()))
            .unwrap_or_default();

        let QualName { prefix, ns, local } = name;
        let name_ns = match prefix {
            Some(_) => NameNs::Other,
            None if ns == ns!(html) => NameNs::Html,
            None if ns == ns!(svg) => NameNs::Svg,
            None if ns == ns!(mathml) => NameNs::MathMl,
            None if ns == ns!() => NameNs::Empty,
            None => NameNs::Other,
        };
        let mut data = ElementData {
            name: ElementName { local },
            id: id_attr_atom,
            classes: class_atoms,
            attrs,
            rare: None,

            stylo_element_data: Default::default(),
            element_state: ElementStateBits::default(),
            state_flags: NodeStateFlags::for_node(ElementBits::new(name_ns)),
            layout_data: OnceCell::new(),
        };
        if name_ns == NameNs::Other {
            data.update_rare(|rare| rare.name_ext = Some(Box::new((prefix, ns))));
        }
        data.flush_is_focussable();
        data.flush_link_state();

        // Mirror the `checked` attribute into the element state so that `:checked`
        // selectors can be matched (and invalidated) from `ElementState`.
        if data.name.local == local_name!("input")
            && matches!(
                data.attr(local_name!("type")),
                Some("checkbox") | Some("radio")
            )
            && data.has_attr(local_name!("checked"))
        {
            data.element_state.insert(ElementState::CHECKED);
        }

        // The element state needs to be modified if the element can be disabled.
        if data.can_be_disabled() {
            data.element_state
                .insert(match data.has_attr(local_name!("disabled")) {
                    true => ElementState::DISABLED,
                    false => ElementState::ENABLED,
                });
        }

        data
    }

    #[inline]
    pub(crate) fn stylo_data(&self) -> StyloData<'_> {
        StyloData::new(&self.stylo_element_data, &self.state_flags)
    }

    #[inline]
    pub(crate) fn stylo_data_mut(&mut self) -> StyloDataMut<'_> {
        StyloDataMut::new(&mut self.stylo_element_data, &self.state_flags)
    }

    pub fn attrs(&self) -> &[Attribute] {
        &self.attrs
    }

    pub fn attr(&self, name: impl PartialEq<LocalName>) -> Option<&str> {
        let attr = self.attrs.iter().find(|attr| name == attr.name.local)?;
        Some(&attr.value)
    }

    pub fn attr_parsed<T: FromStr>(&self, name: impl PartialEq<LocalName>) -> Option<T> {
        let attr = self.attrs.iter().find(|attr| name == attr.name.local)?;
        attr.value.parse::<T>().ok()
    }

    /// Detects the presence of the attribute, treating *any* value as truthy.
    pub fn has_attr(&self, name: impl PartialEq<LocalName>) -> bool {
        self.attrs.iter().any(|attr| name == attr.name.local)
    }

    pub fn can_be_disabled(&self) -> bool {
        local_names!("button", "input", "select", "textarea").contains(&self.name.local)
    }

    /// Whether this element is a link (an `<a>` or `<area>` element with an `href` attribute)
    pub fn is_link(&self) -> bool {
        (self.name.local == local_name!("a") || self.name.local == local_name!("area"))
            && self.has_attr(local_name!("href"))
    }

    /// Sync the visitedness bits of `element_state` with the element's link-ness.
    /// Blitz does not track browsing history, so all links are unvisited.
    ///
    /// Stylo's snapshot invalidation (`ElementWrapper::is_link`) determines link-ness
    /// from these state bits, so they must be kept accurate for `:link`/`:any-link`
    /// selectors to be correctly invalidated. Must be called whenever the `href`
    /// attribute is added or removed.
    pub fn flush_link_state(&mut self) {
        self.element_state
            .remove(ElementState::VISITED_OR_UNVISITED);
        if self.is_link() {
            self.element_state.insert(ElementState::UNVISITED);
        }
    }

    pub fn image_data(&self) -> Option<&ImageData> {
        match self.special_data() {
            SpecialElementData::Image(data) => Some(&**data),
            _ => None,
        }
    }

    pub fn image_data_mut(&mut self) -> Option<&mut ImageData> {
        match self.special_data_mut() {
            Some(SpecialElementData::Image(data)) => Some(&mut **data),
            _ => None,
        }
    }

    pub fn raster_image_data(&self) -> Option<&RasterImageData> {
        match self.image_data()? {
            ImageData::Raster(data) => Some(data),
            _ => None,
        }
    }

    pub fn raster_image_data_mut(&mut self) -> Option<&mut RasterImageData> {
        match self.image_data_mut()? {
            ImageData::Raster(data) => Some(data),
            _ => None,
        }
    }

    pub fn canvas_data(&self) -> Option<&CanvasData> {
        match self.special_data() {
            SpecialElementData::Canvas(data) => Some(data),
            _ => None,
        }
    }

    pub fn sub_doc_data(&self) -> Option<&dyn Document> {
        match self.special_data() {
            SpecialElementData::SubDocument(data) => Some(data.as_ref().as_ref()),
            _ => None,
        }
    }

    pub fn sub_doc_data_mut(&mut self) -> Option<&mut dyn Document> {
        match self.special_data_mut() {
            Some(SpecialElementData::SubDocument(data)) => Some(data.as_mut().as_mut()),
            _ => None,
        }
    }

    #[cfg(feature = "svg")]
    pub fn svg_data(&self) -> Option<&usvg::Tree> {
        match self.image_data()? {
            ImageData::Svg(data) => Some(&data.tree),
            _ => None,
        }
    }

    // Full svg metadata (naturals + viewBox presence), so the
    // painter can tell a real viewBox from usvg's synthesized size.
    #[cfg(feature = "svg")]
    pub fn svg_image_data(&self) -> Option<&SvgImageData> {
        match self.image_data()? {
            ImageData::Svg(data) => Some(data),
            _ => None,
        }
    }

    pub fn text_input_data(&self) -> Option<&TextInputData> {
        match self.special_data() {
            SpecialElementData::TextInput(data) => Some(data),
            _ => None,
        }
    }

    pub fn text_input_data_mut(&mut self) -> Option<&mut TextInputData> {
        match self.special_data_mut() {
            Some(SpecialElementData::TextInput(data)) => Some(data),
            _ => None,
        }
    }

    #[cfg(feature = "custom-widget")]
    pub fn custom_widget_data(&self) -> Option<&CustomWidgetData> {
        match self.special_data() {
            SpecialElementData::CustomWidget(data) => Some(data),
            _ => None,
        }
    }

    #[cfg(feature = "custom-widget")]
    pub fn custom_widget_data_mut(&mut self) -> Option<&mut CustomWidgetData> {
        match self.special_data_mut() {
            Some(SpecialElementData::CustomWidget(data)) => Some(data),
            _ => None,
        }
    }

    pub fn checkbox_input_checked(&self) -> Option<bool> {
        match *self.special_data() {
            SpecialElementData::CheckboxInput(checked) => Some(checked),
            _ => None,
        }
    }

    /// Live value of an `<input type=range>`, when the
    /// special data has been initialized (layout construct pass).
    pub fn range_input_value(&self) -> Option<f64> {
        match self.special_data() {
            SpecialElementData::RangeInput(value) => Some(**value),
            _ => None,
        }
    }

    pub fn checkbox_input_checked_mut(&mut self) -> Option<&mut bool> {
        match self.special_data_mut() {
            Some(SpecialElementData::CheckboxInput(checked)) => Some(checked),
            _ => None,
        }
    }

    /// Set the checked state of a checkbox/radio input, keeping the
    /// `ElementState::CHECKED` bit (used for `:checked` selector matching and
    /// invalidation) in sync with the special data.
    pub fn set_checkbox_input_checked(&mut self, checked: bool) {
        if let Some(is_checked) = self.checkbox_input_checked_mut() {
            *is_checked = checked;
        }
        self.element_state.set(ElementState::CHECKED, checked);
    }

    #[cfg(feature = "file-input")]
    pub fn file_data(&self) -> Option<&FileData> {
        match self.special_data() {
            SpecialElementData::FileInput(data) => Some(data),
            _ => None,
        }
    }

    #[cfg(feature = "file-input")]
    pub fn file_data_mut(&mut self) -> Option<&mut FileData> {
        match self.special_data_mut() {
            Some(SpecialElementData::FileInput(data)) => Some(data),
            _ => None,
        }
    }

    pub fn flush_is_focussable(&mut self) {
        let disabled: bool = self.attr_parsed(local_name!("disabled")).unwrap_or(false);
        let tabindex: Option<i32> = self.attr_parsed(local_name!("tabindex"));
        let contains_sub_document: bool = self.sub_doc_data().is_some();

        let is_focussable = contains_sub_document
            || (!disabled
                && match tabindex {
                    Some(index) => index >= 0,
                    None => {
                        // Some focusable HTML elements have a default tabindex value of 0 set under the hood by the user agent.
                        // These elements are:
                        //   - <a> or <area> with href attribute
                        //   - <button>, <frame>, <iframe>, <input>, <object>, <select>, <textarea>, and SVG <a> element
                        //   - <summary> element that provides summary for a <details> element.

                        if [local_name!("a"), local_name!("area")].contains(&self.name.local) {
                            self.attr(local_name!("href")).is_some()
                        } else {
                            const DEFAULT_FOCUSSABLE_ELEMENTS: [LocalName; 7] = [
                                local_name!("button"),
                                local_name!("input"),
                                local_name!("select"),
                                local_name!("textarea"),
                                local_name!("frame"),
                                local_name!("iframe"),
                                local_name!("summary"),
                            ];
                            DEFAULT_FOCUSSABLE_ELEMENTS.contains(&self.name.local)
                        }
                    }
                });
        self.set_element_bit(ElementBits::FOCUSSABLE, is_focussable);
    }

    pub fn flush_style_attribute(&mut self, guard: &SharedRwLock, url_extra_data: &UrlExtraData) {
        let block = self.attr(local_name!("style")).map(|style_str| {
            ServoArc::new(guard.wrap(parse_style_attribute(
                style_str,
                url_extra_data,
                None,
                QuirksMode::NoQuirks,
                CssRuleType::Style,
            )))
        });
        self.set_style_attribute(block);
    }

    pub fn set_style_property(
        &mut self,
        name: &str,
        value: &str,
        guard: &SharedRwLock,
        url_extra_data: UrlExtraData,
    ) -> bool {
        let context = ParserContext::new(
            Origin::Author,
            &url_extra_data,
            Some(CssRuleType::Style),
            ParsingMode::DEFAULT,
            QuirksMode::NoQuirks,
            /* namespaces = */ Default::default(),
            None,
            None,
            /* attr_taint = */ Default::default(),
        );

        let Ok(property_id) = PropertyId::parse(name, &context) else {
            #[cfg(feature = "tracing")]
            tracing::warn!(property = name, "Unsupported property");
            return false;
        };
        let mut source_property_declaration = SourcePropertyDeclaration::default();
        let mut parser = Parser::new(value);
        let Ok(_) = PropertyDeclaration::parse_into(
            &mut source_property_declaration,
            property_id,
            &context,
            &mut parser,
        ) else {
            #[cfg(feature = "tracing")]
            tracing::warn!(property = name, value, "Invalid property value");
            return false;
        };

        self.mutable_style_attribute(guard)
            .write_with(&mut guard.write())
            .extend(source_property_declaration.drain(), Importance::Normal);

        true
    }

    // Copy-on-write: once Stylo has put this block in the rule tree it may be
    // shared with other elements' styles (mirrors Gecko's
    // nsDOMCSSDeclaration::EnsureBlockMutable).
    fn mutable_style_attribute(
        &mut self,
        guard: &SharedRwLock,
    ) -> &ServoArc<Locked<PropertyDeclarationBlock>> {
        let cloned = match self.style_attribute() {
            None => Some(PropertyDeclarationBlock::new()),
            Some(arc) => {
                let read = guard.read();
                let block = arc.read_with(&read);
                block
                    .immutable
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .then(|| block.clone())
            }
        };
        if let Some(block) = cloned {
            self.set_style_attribute(Some(ServoArc::new(guard.wrap(block))));
        }
        self.style_attribute().expect("style attribute was just set")
    }

    pub fn remove_style_property(
        &mut self,
        name: &str,
        guard: &SharedRwLock,
        url_extra_data: UrlExtraData,
    ) -> bool {
        let context = ParserContext::new(
            Origin::Author,
            &url_extra_data,
            Some(CssRuleType::Style),
            ParsingMode::DEFAULT,
            QuirksMode::NoQuirks,
            /* namespaces = */ Default::default(),
            None,
            None,
            /* attr_taint = */ Default::default(),
        );
        let Ok(property_id) = PropertyId::parse(name, &context) else {
            #[cfg(feature = "tracing")]
            tracing::warn!(property = name, "Unsupported property");
            return false;
        };

        if self.style_attribute().is_none() {
            return false;
        }
        let style = self.mutable_style_attribute(guard);
        let mut guard = guard.write();
        let style = style.write_with(&mut guard);
        if let Some(index) = style.first_declaration_to_remove(&property_id) {
            style.remove_property(&property_id, index);
            return true;
        }

        false
    }

    pub fn set_sub_document(&mut self, sub_document: Box<dyn Document>) {
        self.set_special_data(SpecialElementData::SubDocument(Box::new(sub_document)));
    }

    pub fn remove_sub_document(&mut self) {
        self.set_special_data(SpecialElementData::None);
    }

    #[cfg(feature = "custom-widget")]
    pub fn set_custom_widget(&mut self, widget: Box<dyn crate::Widget>) {
        use crate::node::custom_widget::CustomWidgetData;
        self.set_special_data(SpecialElementData::CustomWidget(CustomWidgetData::new(widget)));
    }

    #[cfg(feature = "custom-widget")]
    pub fn remove_custom_widget(&mut self) -> Vec<anyrender::ResourceId> {
        let resource_ids = self
            .custom_widget_data_mut()
            .map(|widget_data| widget_data.take_resource_ids())
            .unwrap_or_default();
        self.set_special_data(SpecialElementData::None);
        resource_ids
    }

    pub fn take_inline_layout(&mut self) -> Option<Box<TextLayout>> {
        self.take_inline_layout_data()
    }

    pub(crate) fn is_button_input(&self) -> bool {
        self.name.local == local_name!("input")
            && matches!(
                self.attr(local_name!("type")),
                Some("button" | "submit" | "reset")
            )
    }

    pub fn is_submit_button(&self) -> bool {
        if self.name.local != local_name!("button") {
            return false;
        }
        let type_attr = self.attr(local_name!("type"));
        let is_submit = type_attr == Some("submit");
        let is_auto_submit = type_attr.is_none()
            && self.attr(LocalName::from("command")).is_none()
            && self.attr(LocalName::from("commandfor")).is_none();
        is_submit || is_auto_submit
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RasterImageData {
    /// The width of the image
    pub width: u32,
    /// The height of the image
    pub height: u32,
    /// The raw image data in RGBA8 format
    pub data: Blob<u8>,
    /// When true, `data` holds the original encoded
    /// container bytes (png/jpeg/gif/webp), not RGBA8: dims-only
    /// renderer mode. Layout reads width/height only; paint-side
    /// consumers must check this before touching pixels.
    pub encoded: bool,
}
impl RasterImageData {
    pub fn new(width: u32, height: u32, data: Arc<Vec<u8>>) -> Self {
        Self {
            width,
            height,
            data: Blob::new(data),
            encoded: false,
        }
    }

    /// Dims-only entry: see the `encoded` field doc.
    ///
    /// `key` is the embedder-supplied resource id. Non-zero means the
    /// embedder owns identity for these bytes, so it rides in the Blob
    /// id rather than being derived from a content hash: the point
    /// being that a consumer holding no bytes can still name the
    /// resource. Zero keeps `Blob::new`'s counter id.
    pub fn new_encoded(width: u32, height: u32, data: Arc<Vec<u8>>, key: u64) -> Self {
        Self {
            width,
            height,
            data: if key != 0 {
                Blob::from_raw_parts(data, key)
            } else {
                Blob::new(data)
            },
            encoded: true,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ImageData {
    Raster(RasterImageData),
    #[cfg(feature = "svg")]
    Svg(SvgImageData),
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Ok,
    Error,
    Loading,
}

#[derive(Debug, Clone)]
pub struct ImageResourceData {
    /// The url of the background image
    pub url: ServoArc<Url>,
    /// The loading status of the background image
    pub status: Status,
    /// The image data
    pub image: ImageData,
}

impl ImageResourceData {
    pub fn new(url: ServoArc<Url>) -> Self {
        Self {
            url,
            status: Status::Loading,
            image: ImageData::None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CanvasData {
    pub custom_paint_source_id: u64,
}

impl std::fmt::Debug for SpecialElementData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpecialElementData::SubDocument(_) => f.write_str("NodeSpecificData::SubDocument"),
            #[cfg(feature = "custom-widget")]
            SpecialElementData::CustomWidget(_) => f.write_str("NodeSpecificData::CustomWidget"),
            SpecialElementData::Stylesheet(_) => f.write_str("NodeSpecificData::Stylesheet"),
            SpecialElementData::Image(data) => match **data {
                ImageData::Raster(_) => f.write_str("NodeSpecificData::Image(Raster)"),
                #[cfg(feature = "svg")]
                ImageData::Svg(_) => f.write_str("NodeSpecificData::Image(Svg)"),
                ImageData::None => f.write_str("NodeSpecificData::Image(None)"),
            },
            SpecialElementData::Canvas(_) => f.write_str("NodeSpecificData::Canvas"),
            SpecialElementData::TableRoot(_) => f.write_str("NodeSpecificData::TableRoot"),
            SpecialElementData::TextInput(_) => f.write_str("NodeSpecificData::TextInput"),
            SpecialElementData::CheckboxInput(_) => f.write_str("NodeSpecificData::CheckboxInput"),
            SpecialElementData::RangeInput(_) => f.write_str("NodeSpecificData::RangeInput"),
            #[cfg(feature = "file-input")]
            SpecialElementData::FileInput(_) => f.write_str("NodeSpecificData::FileInput"),
            SpecialElementData::None => f.write_str("NodeSpecificData::None"),
        }
    }
}

#[derive(Clone)]
pub struct ListItemLayout {
    pub marker: Marker,
    pub position: ListItemLayoutPosition,
}

//We seperate chars from strings in order to optimise rendering - ie not needing to
//construct a whole parley layout for simple char markers
#[derive(Debug, PartialEq, Clone)]
pub enum Marker {
    Char(char),
    String(String),
}

//Value depends on list-style-position, determining whether a seperate layout is created for it
#[derive(Clone)]
pub enum ListItemLayoutPosition {
    Inside,
    /// A separate box shaped at `scale` when painted, by
    /// [`ListItemLayout::outside_marker_layout`]: a page holds many list items, and a
    /// kept layout costs about a kilobyte each.
    Outside { scale: f32 },
}

impl std::fmt::Debug for ListItemLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ListItemLayout - marker {:?}", self.marker)
    }
}

#[cfg(feature = "file-input")]
mod file_data {
    use std::ops::{Deref, DerefMut};
    use std::path::PathBuf;

    #[derive(Clone, Debug)]
    pub struct FileData(pub Vec<PathBuf>);
    impl Deref for FileData {
        type Target = Vec<PathBuf>;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
    impl DerefMut for FileData {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }
    impl From<Vec<PathBuf>> for FileData {
        fn from(files: Vec<PathBuf>) -> Self {
            Self(files)
        }
    }
}
#[cfg(feature = "file-input")]
pub use file_data::FileData;

#[cfg(test)]
mod tests {
    use super::TextInputData;
    use parley::{FontContext, LayoutContext};

    #[test]
    fn cloning_an_element_discards_derived_text_layouts() {
        use crate::node::{
            ElementData, ListItemLayout, ListItemLayoutPosition, Marker, TextLayout,
        };

        let mut element = ElementData::new(crate::qual_name!("li", html), Vec::new());
        element.set_inline_layout_data(Some(Box::default()));
        element.set_list_item_data(Some(Box::new(ListItemLayout {
            marker: Marker::Char('•'),
            position: ListItemLayoutPosition::Inside,
        })));

        let cloned = element.clone();
        assert!(cloned.inline_layout_data().is_none());
        assert!(cloned.list_item_data().is_none());
        assert!(element.inline_layout_data().is_some());
        assert!(element.list_item_data().is_some());
    }

    /// Build a [`TextInputData`] with the given text laid out at scale 1.0.
    fn make_input(is_multiline: bool, text: &str) -> TextInputData {
        let mut font_ctx = FontContext::new();
        let mut layout_ctx = LayoutContext::new();
        let mut data = TextInputData::new(is_multiline);
        data.editor.set_scale(1.0);
        data.editor.set_text(text);
        data.editor
            .driver(&mut font_ctx, &mut layout_ctx)
            .refresh_layout();
        data
    }

    #[test]
    fn short_text_does_not_scroll() {
        let mut data = make_input(false, "hi");
        // A wide content box that comfortably fits the text.
        data.clamp_scroll_offset(1000.0, 100.0);
        assert_eq!(data.scroll_offset, 0.0);
    }

    #[test]
    fn single_line_scrolls_to_follow_caret() {
        let text = "the quick brown fox jumps over the lazy dog repeatedly and at length";
        let mut data = make_input(false, text);
        let content_box_width = 40.0;
        let content_box_height = 20.0;

        // Caret at the end of a string that overflows a narrow input should scroll right.
        data.editor
            .driver(&mut FontContext::new(), &mut LayoutContext::new())
            .move_to_text_end();
        data.clamp_scroll_offset(content_box_width, content_box_height);

        let layout_width = data.editor.try_layout().unwrap().full_width();
        if layout_width > content_box_width {
            assert!(
                data.scroll_offset > 0.0,
                "expected horizontal scroll for overflowing single-line input"
            );
            // The caret must be within the visible region after scrolling.
            let caret = data.editor.cursor_geometry(1.5).unwrap();
            assert!(caret.x1 as f32 <= data.scroll_offset + content_box_width + 0.5);
            assert!(caret.x0 as f32 >= data.scroll_offset - 0.5);
        }

        // Moving the caret back to the start should reset the scroll offset.
        data.editor
            .driver(&mut FontContext::new(), &mut LayoutContext::new())
            .move_to_text_start();
        data.clamp_scroll_offset(content_box_width, content_box_height);
        assert_eq!(data.scroll_offset, 0.0);
    }

    #[test]
    fn multiline_scrolls_vertically_not_horizontally() {
        let text = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut data = make_input(true, &text);
        // Constrain the width so wrapping is well-defined.
        data.editor.set_width(Some(200.0));
        data.editor
            .driver(&mut FontContext::new(), &mut LayoutContext::new())
            .refresh_layout();

        let content_box_width = 200.0;
        let content_box_height = 30.0;

        data.editor
            .driver(&mut FontContext::new(), &mut LayoutContext::new())
            .move_to_text_end();
        data.clamp_scroll_offset(content_box_width, content_box_height);

        let layout_height = data.editor.try_layout().unwrap().height();
        if layout_height > content_box_height {
            assert!(
                data.scroll_offset > 0.0,
                "expected vertical scroll for overflowing multi-line input"
            );
        }
    }

    #[test]
    fn scroll_by_clamps_and_bubbles() {
        let text = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut data = make_input(true, &text);
        data.editor.set_width(Some(200.0));
        data.editor
            .driver(&mut FontContext::new(), &mut LayoutContext::new())
            .refresh_layout();

        let content_box_width = 200.0;
        let content_box_height = 30.0;
        let max = data.max_scroll_offset(content_box_width, content_box_height);
        assert!(max > 0.0, "test text should overflow the content box");

        // Scrolling up (positive delta decreases offset) while already at the top is a no-op and
        // the whole delta bubbles.
        assert_eq!(data.scroll_offset, 0.0);
        let bubbled = data.scroll_by(15.0, content_box_width, content_box_height);
        assert_eq!(data.scroll_offset, 0.0);
        assert_eq!(bubbled, 15.0);

        // Scrolling down moves the offset and consumes the delta.
        let bubbled = data.scroll_by(-10.0, content_box_width, content_box_height);
        assert_eq!(data.scroll_offset, 10.0);
        assert_eq!(bubbled, 0.0);

        // Scrolling past the end clamps to the maximum and bubbles the remainder. Starting at
        // offset 10 with max headroom of `max - 10`, a delta of `-(max + 100)` consumes
        // `max - 10` and bubbles the rest (`-110`).
        let bubbled = data.scroll_by(-(max + 100.0), content_box_width, content_box_height);
        assert_eq!(data.scroll_offset, max);
        assert!((bubbled - (-110.0)).abs() < 1e-3);
    }

    #[test]
    fn single_line_does_not_scroll_when_text_fits() {
        let mut data = make_input(false, "hi");
        // Wide content box; nothing to scroll, so all delta bubbles.
        let bubbled = data.scroll_by(-50.0, 1000.0, 100.0);
        assert_eq!(data.scroll_offset, 0.0);
        assert_eq!(bubbled, -50.0);
    }
}

#[cfg(test)]
mod rare_tests {
    use super::ElementData;
    use blitz_traits::node_id::NodeId;
    use markup5ever::{QualName, local_name, ns};

    #[test]
    fn layouts_round_trip_with_edges_kept_only_while_non_zero() {
        use taffy::{Layout, Point, Rect, Size};
        let mut data = super::LayoutData::new();
        let mut plain = Layout::new();
        plain.order = 3;
        plain.location = Point { x: 1.5, y: -2.0 };
        plain.size = Size { width: 10.0, height: 20.0 };
        plain.margin = Rect { left: 1.0, right: 2.0, top: 3.0, bottom: 4.0 };
        plain.scrollable_overflow_rect = Rect { left: -1.0, right: 9.0, top: 0.0, bottom: 7.0 };
        data.set_unrounded_layout(&plain);
        data.set_final_layout(&plain);
        assert!(data.edges.is_none());
        assert_eq!(data.unrounded_layout(), plain);

        let mut edged = plain;
        edged.border = Rect { left: 1.0, right: 1.0, top: 0.0, bottom: 0.0 };
        edged.padding.top = -0.0;
        edged.scrollbar_size = Size { width: 15.0, height: 0.0 };
        data.set_final_layout(&edged);
        assert!(data.edges.is_some());
        let rounded = data.final_layout();
        assert_eq!(rounded, edged);
        assert!(rounded.padding.top.is_sign_negative(), "-0.0 survives");
        assert_eq!(data.unrounded_layout(), plain);

        data.shift_final_location(2.0, 3.0);
        assert_eq!(data.final_layout().location, Point { x: 3.5, y: 1.0 });
        data.set_final_layout(&plain);
        assert!(data.edges.is_none(), "box dropped once both layouts' edges are zero");
    }

    #[test]
    fn rare_links_round_trip_and_clone_drops_the_shadow_root() {
        let name = QualName::new(None, ns!(html), local_name!("template"));
        let mut el = ElementData::new(name, Vec::new());
        let contents = NodeId::from_u64(1 << 32 | 5);
        let shadow = NodeId::from_u64(3 << 32 | 9);
        el.set_template_contents(None);
        assert!(el.rare.is_none());
        el.set_template_contents(Some(contents));
        el.set_shadow_root(Some(shadow));
        assert_eq!((el.template_contents(), el.shadow_root()), (Some(contents), Some(shadow)));
        let clone = el.clone();
        assert_eq!((clone.template_contents(), clone.shadow_root()), (Some(contents), None));
        el.set_template_contents(None);
        el.set_shadow_root(None);
        assert!(el.rare.is_none());
    }

    #[test]
    fn special_data_lives_in_rare_and_survives_clone() {
        use super::SpecialElementData;
        let name = QualName::new(None, ns!(html), local_name!("input"));
        let mut el = ElementData::new(name, Vec::new());
        assert!(matches!(el.special_data(), SpecialElementData::None));
        el.set_special_data(SpecialElementData::None);
        assert!(el.special_data_mut().is_none());
        assert!(el.rare.is_none());
        el.set_special_data(SpecialElementData::CheckboxInput(true));
        assert!(matches!(el.special_data(), SpecialElementData::CheckboxInput(true)));
        let clone = el.clone();
        assert!(matches!(clone.special_data(), SpecialElementData::CheckboxInput(true)));
        assert!(matches!(el.take_special_data(), SpecialElementData::CheckboxInput(true)));
        assert!(el.rare.is_none());
    }

    #[test]
    fn element_names_round_trip() {
        let names = [
            QualName::new(None, ns!(html), local_name!("div")),
            QualName::new(None, ns!(svg), local_name!("svg")),
            QualName::new(None, ns!(mathml), local_name!("math")),
            QualName::new(None, ns!(), local_name!("div")),
            QualName::new(None, "urn:x".into(), "item".into()),
            QualName::new(Some("x".into()), ns!(svg), local_name!("rect")),
        ];
        for name in names {
            let el = ElementData::new(name.clone(), Vec::new());
            assert_eq!(el.qual_name(), name);
            assert_eq!(el.clone().qual_name(), name);
            assert_eq!(el.expanded_name(), name.expanded());
            // Only an uncommon namespace or a prefix needs the rare box.
            let common = name.prefix.is_none() && !name.ns.starts_with("urn:");
            assert_eq!(el.rare.is_none(), common, "{name:?}");
            let mut el = el;
            el.set_before(Some(NodeId::from_u64(2 << 32 | 7)));
            el.set_before(None);
            assert_eq!(el.qual_name(), name, "clearing a rare field keeps the name");
        }
    }

    #[test]
    fn packed_flags_and_states_round_trip() {
        use selectors::matching::ElementSelectorFlags;
        use style_dom::ElementState;

        let href = || {
            vec![super::Attribute::new(
                QualName::new(None, ns!(), local_name!("href")),
                "#",
            )]
        };
        let html = QualName::new(None, ns!(html), local_name!("a"));
        let html_el = ElementData::new(html.clone(), href());
        assert!(html_el.is_focussable());
        assert_eq!(html_el.qual_name(), html);

        let name = QualName::new(Some("x".into()), ns!(svg), local_name!("a"));
        let mut el = ElementData::new(name.clone(), href());
        assert!(el.is_focussable());
        el.set_has_snapshot(true);
        assert!(el.has_snapshot() && el.is_focussable());
        assert_eq!(el.qual_name(), name);
        let clone = el.clone();
        assert!(!clone.has_snapshot() && clone.is_focussable());
        assert_eq!(clone.qual_name(), name);

        let state = ElementState::FOCUS | ElementState::FOCUSRING | ElementState::UNVISITED;
        el.element_state.insert(state);
        el.element_state.set(ElementState::HOVER, true);
        el.element_state.remove(ElementState::FOCUS);
        assert_eq!(
            el.element_state.get(),
            ElementState::FOCUSRING | ElementState::UNVISITED | ElementState::HOVER
        );
        let flags = ElementSelectorFlags::all();
        el.selector_flags().set(flags);
        assert_eq!(el.selector_flags().get().bits(), flags.bits());
    }

    #[test]
    #[should_panic(expected = "element states above bit 31")]
    fn storing_a_high_element_state_panics() {
        let name = QualName::new(None, ns!(html), local_name!("div"));
        let mut el = ElementData::new(name, Vec::new());
        el.element_state.insert(style_dom::ElementState::PICTURE_IN_PICTURE);
    }

    #[test]
    fn pseudo_links_round_trip_and_are_not_cloned() {
        let name = QualName::new(None, ns!(html), local_name!("div"));
        let mut el = ElementData::new(name, Vec::new());
        let before = NodeId::from_u64(2 << 32 | 7);
        let after = NodeId::from_u64(4 << 32 | 8);
        el.set_before(Some(before));
        el.set_after(Some(after));
        assert_eq!((el.before(), el.after()), (Some(before), Some(after)));
        let clone = el.clone();
        assert_eq!((clone.before(), clone.after()), (None, None));
        assert!(clone.rare.is_none());
        el.set_before(None);
        assert_eq!((el.before(), el.after()), (None, Some(after)));
        el.set_after(None);
        assert!(el.rare.is_none());
    }

    #[test]
    fn image_layers_live_in_the_rare_box_until_cleared() {
        let name = QualName::new(None, ns!(html), local_name!("div"));
        let mut el = ElementData::new(name, Vec::new());
        el.clear_image_layers(false);
        assert!(el.rare.is_none());
        el.image_layers_storage_mut(true).resize(2, None);
        assert_eq!((el.background_images().len(), el.mask_images().len()), (0, 2));
        assert_eq!(el.clone().mask_images().len(), 2);
        el.clear_image_layers(true);
        assert!(el.rare.is_none());
        assert!(el.grid_info().is_none());
    }

    #[test]
    fn display_codes_round_trip_every_display() {
        use super::{DISPLAY_INSIDE, DISPLAY_OUTSIDE, display_code, display_from_code};
        use style::values::computed::Display;
        let name = QualName::new(None, ns!(html), local_name!("div"));
        let mut el = ElementData::new(name, Vec::new());
        let mut checked = 0;
        for outside in DISPLAY_OUTSIDE {
            for inside in DISPLAY_INSIDE {
                for list_item in [false, true] {
                    let display = Display::from3(outside, inside, list_item);
                    let decoded = display_from_code(display_code(display));
                    assert_eq!(decoded, display);
                    assert_eq!(decoded.to_u16(), display.to_u16());
                    el.set_display_constructed_as(display);
                    assert_eq!(el.display_constructed_as().to_u16(), display.to_u16());
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 5 * 15 * 2);
    }

    /// Every field packed into an element's state word.
    #[derive(Clone, Debug, PartialEq)]
    struct WordFields {
        selector: usize,
        stylo_data: bool,
        dirty: bool,
        damaged: bool,
        snapshot_handled: bool,
        ns: super::NameNs,
        focussable: bool,
        has_snapshot: bool,
        display: u16,
    }

    fn word_fields(el: &ElementData) -> WordFields {
        let flags = &el.state_flags;
        WordFields {
            selector: el.selector_flags().get().bits(),
            stylo_data: el.stylo_data().has_data(),
            dirty: flags.dirty_descendants(),
            damaged: flags.damaged_descendants(),
            snapshot_handled: flags.snapshot_handled(),
            ns: flags.element_bits().name_ns(),
            focussable: el.is_focussable(),
            has_snapshot: el.has_snapshot(),
            display: el.display_constructed_as().to_u16(),
        }
    }

    #[test]
    fn fresh_element_word_has_initial_values() {
        use style::values::computed::Display;
        let name = QualName::new(None, ns!(html), local_name!("div"));
        let fresh = WordFields {
            selector: 0,
            stylo_data: false,
            dirty: true,
            damaged: true,
            snapshot_handled: false,
            ns: super::NameNs::Html,
            focussable: false,
            has_snapshot: false,
            display: Display::Block.to_u16(),
        };
        assert_eq!(word_fields(&ElementData::new(name, Vec::new())), fresh);
        let svg = QualName::new(None, ns!(svg), local_name!("svg"));
        let svg_fields = word_fields(&ElementData::new(svg, Vec::new()));
        assert_eq!(svg_fields, WordFields { ns: super::NameNs::Svg, ..fresh });
        let doc = super::DocumentData::new();
        assert!(doc.selector_flags().get().is_empty());
        assert_eq!(doc.display_constructed_as(), Display::Block);
    }

    #[test]
    fn word_fields_are_independent() {
        use super::{ElementBits, NameNs};
        use selectors::matching::ElementSelectorFlags;
        use style::values::computed::Display;
        use style::values::specified::box_::{DisplayInside, DisplayOutside};

        type Step = (fn(&mut ElementData, bool), fn(&mut WordFields, bool));
        fn selector(v: bool) -> ElementSelectorFlags {
            match v {
                true => ElementSelectorFlags::all(),
                false => ElementSelectorFlags::empty(),
            }
        }
        fn display(v: bool) -> Display {
            let (outside, inside) = (DisplayOutside::InternalTable, DisplayInside::WebkitBox);
            match v {
                true => Display::from3(outside, inside, true),
                false => Display::Block,
            }
        }
        fn ns(v: bool) -> NameNs {
            if v { NameNs::Other } else { NameNs::Html }
        }
        let steps: [Step; 9] = [
            (
                |el, v| el.selector_flags().set(selector(v)),
                |f, v| f.selector = selector(v).bits(),
            ),
            (
                |el, v| match v {
                    true => drop(el.stylo_data_mut().ensure_init_mut()),
                    // SAFETY: no borrows of `el`'s stylo data are outstanding.
                    false => unsafe { el.stylo_data().clear() },
                },
                |f, v| f.stylo_data = v,
            ),
            (|el, v| el.state_flags.set_dirty_descendants(v), |f, v| f.dirty = v),
            (|el, v| el.state_flags.set_damaged_descendants(v), |f, v| f.damaged = v),
            (|el, v| el.state_flags.set_snapshot_handled(v), |f, v| f.snapshot_handled = v),
            (
                |el, v| {
                    let old = el.state_flags.element_bits().0;
                    let bits = ElementBits(old & !ElementBits::NS | ns(v) as u8);
                    el.state_flags.set_element_bits(bits);
                },
                |f, v| f.ns = ns(v),
            ),
            (
                |el, v| el.set_element_bit(ElementBits::FOCUSSABLE, v),
                |f, v| f.focussable = v,
            ),
            (|el, v| el.set_has_snapshot(v), |f, v| f.has_snapshot = v),
            (
                |el, v| el.set_display_constructed_as(display(v)),
                |f, v| f.display = display(v).to_u16(),
            ),
        ];

        let name = QualName::new(None, ns!(html), local_name!("div"));
        let mut el = ElementData::new(name, Vec::new());
        // The first pass starts from a fresh element; the second from every field set.
        for _pass in 0..2 {
            for (mutate, expect) in steps {
                for value in [true, false, true] {
                    let mut expected = word_fields(&el);
                    expect(&mut expected, value);
                    mutate(&mut el, value);
                    assert_eq!(word_fields(&el), expected);
                }
            }
        }
    }
}
