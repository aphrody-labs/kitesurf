use blitz_traits::{
    events::{BlitzImeEvent, BlitzKeyEvent},
    node_id::NodeId,
    shell::ShellProvider,
};
use keyboard_types::{Key, Modifiers, NamedKey};
use parley::{Alignment, AlignmentOptions, ContentWidths, FontContext, IndentOptions, LayoutContext};
use std::cell::{Cell, RefCell};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::util::ACTION_MOD;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
/// Parley Brush type for Blitz which contains the Blitz node id
pub struct TextBrush {
    /// The node id for the span
    pub id: NodeId,
}

impl TextBrush {
    pub(crate) fn from_id(id: NodeId) -> Self {
        Self { id }
    }
}

#[derive(Clone)]
pub struct TextLayout {
    /// Shaped text and lines, with the text and source ranges they were
    /// built from. Empty while evicted or not yet shaped, and then `evicted`
    /// holds what rebuilds it: read it through [`Self::layout`] or
    /// [`Self::layout_mut`], which rebuild first.
    built: OnceLock<Box<Built>>,
    text_kind: TextKindCell,
    /// The content widths inline layout last measured, and the indent it
    /// measured them with (the fixed part of `text-indent`). Kept only for a
    /// layout without inline boxes, whose widths depend on nothing else, so
    /// that an evicted root is measured without rebuilding it.
    pub content_widths: Option<(f32, ContentWidths)>,
    evicted: Option<Box<EvictedLayout>>,
    /// Whether the last line break can be replayed from the layout alone:
    /// false when floats shaped the lines, which needs the gone block context.
    pub replayable_break: bool,
    /// Set by every read; the eviction scan spares a far layout that was read
    /// since its last visit once, so a layout queried every frame stays live.
    recently_read: ReadFlag,
    /// The low 31 bits: sequence number of this root's latest entry in the layout pass's
    /// eviction window, so older entries for it are skipped. The top bit: the document's
    /// `inline_roots` index holds this root, which it does while the node holds this layout.
    seq_and_indexed: u32,
    /// Block-axis offset (in CSS px) of the line boxes from the top of the container's content
    /// box, as applied by `align-content`.
    pub block_offset: f32,
    /// The out-of-flow positions for which some inline span with an inline box below it is a
    /// containing block (as `SPAN_*_CB` flags). The inline root claims those boxes in its place.
    /// Set by each `RunMode::PerformLayout` run of inline layout.
    pub span_cb_flags: stylo_taffy::StyleFlags,
    /// Content width `layout` currently holds lines for, or
    /// NaN when it holds none: a rebuilt tree has no lines until
    /// something breaks it. resolve compares this against the committed
    /// box and re-breaks on either mismatch. See restore_clobbered_inline_breaks.
    pub last_break_width: f32,
}

// Every inline root keeps one, evicted or not: 44 bytes fill a 48-byte
// wasm32 allocator chunk.
#[cfg(target_pointer_width = "32")]
const _: () = assert!(size_of::<TextLayout>() <= 44);

/// A shaped layout and the text it was built from, which are always built
/// and evicted together.
#[derive(Clone, Default)]
struct Built {
    layout: parley::layout::Layout<TextBrush>,
    content: TextContent,
}

#[derive(Clone, Default)]
struct TextContent {
    text: String,
    // (text node id, byte range in the layout text) per
    // pushed text node: approximate under whitespace collapse
    // (measured committed+pending around each push), consumed by
    // the DOM-selection paint bridge with clamping.
    node_ranges: Vec<(NodeId, core::ops::Range<usize>)>,
}

#[derive(Clone, Copy)]
pub(crate) struct TextKind {
    pub(crate) empty: bool,
    pub(crate) only_bidi_controls: bool,
}

impl TextKind {
    fn of(text: &str) -> Self {
        Self {
            empty: text.is_empty(),
            only_bidi_controls: text
                .chars()
                .all(|c| matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')),
        }
    }
}

/// A [`TextKind`] once known, packed into one byte.
#[derive(Default)]
struct TextKindCell(AtomicU8);

impl TextKindCell {
    const KNOWN: u8 = 1;
    const EMPTY: u8 = 2;
    const ONLY_BIDI_CONTROLS: u8 = 4;

    fn get_or_init(&self, init: impl FnOnce() -> TextKind) -> TextKind {
        let bits = self.0.load(Ordering::Relaxed);
        if bits & Self::KNOWN != 0 {
            return TextKind {
                empty: bits & Self::EMPTY != 0,
                only_bidi_controls: bits & Self::ONLY_BIDI_CONTROLS != 0,
            };
        }
        let kind = init();
        let bits = Self::KNOWN
            | if kind.empty { Self::EMPTY } else { 0 }
            | if kind.only_bidi_controls { Self::ONLY_BIDI_CONTROLS } else { 0 };
        self.0.store(bits, Ordering::Relaxed);
        kind
    }
}

impl Clone for TextKindCell {
    fn clone(&self) -> Self {
        Self(AtomicU8::new(self.0.load(Ordering::Relaxed)))
    }
}

impl Default for TextLayout {
    fn default() -> Self {
        let mut layout = Self::unfilled();
        layout.built = OnceLock::from(Box::default());
        layout
    }
}

impl TextLayout {
    /// The top bit of `seq_and_indexed`; pass sequence numbers stay below it.
    pub(crate) const ROOT_INDEXED: u32 = 1 << 31;

    pub(crate) fn in_pass_seq(&self) -> u32 {
        self.seq_and_indexed & !Self::ROOT_INDEXED
    }

    pub(crate) fn set_in_pass_seq(&mut self, seq: u32) {
        debug_assert_eq!(seq & Self::ROOT_INDEXED, 0);
        self.seq_and_indexed = (self.seq_and_indexed & Self::ROOT_INDEXED) | seq;
    }

    /// Marks the root as held by the `inline_roots` index; true if it already was.
    pub(crate) fn mark_root_indexed(&mut self) -> bool {
        let was = self.seq_and_indexed & Self::ROOT_INDEXED != 0;
        self.seq_and_indexed |= Self::ROOT_INDEXED;
        was
    }

    /// A layout holding neither text nor lines, for box construction to
    /// build or leave unbuilt next: an empty box would be freed unread.
    pub(crate) fn unfilled() -> Self {
        Self {
            built: OnceLock::new(),
            text_kind: TextKindCell::default(),
            content_widths: None,
            evicted: None,
            replayable_break: false,
            recently_read: ReadFlag::default(),
            seq_and_indexed: 0,
            block_offset: 0.0,
            span_cb_flags: stylo_taffy::StyleFlags::empty(),
            // NaN = no lines yet; the repair scan gives it some once
            // the node has a committed box.
            last_break_width: f32::NAN,
        }
    }
}

// Every evicted root keeps one: 20 bytes fill a 24-byte wasm32 chunk.
#[cfg(target_pointer_width = "32")]
const _: () = assert!(size_of::<EvictedLayout>() <= 20);

/// What every evicted root of a document rebuilds with.
pub(crate) struct InlineRebuild {
    pub(crate) font_ctx: Arc<Mutex<FontContext>>,
    /// The document's queue of rebuilt roots, which become resident again.
    pub(crate) rebuilt: Arc<Mutex<Vec<NodeId>>>,
}

/// Everything needed to rebuild an evicted layout with the lines it had:
/// the shaping inputs are re-read from the DOM, the line-breaking inputs are
/// the ones the dropped layout was last broken with.
#[derive(Clone)]
pub(crate) struct EvictedLayout {
    pub(crate) node_id: NodeId,
    pub(crate) scale: f32,
    /// `None` for a layout never broken (only its text was collected), which
    /// rebuilds shaped but without lines, as box construction left it.
    pub(crate) breaking: Option<Box<EvictedBreak>>,
}

/// The line-breaking inputs an evicted layout was last broken with.
#[derive(Clone)]
pub(crate) struct EvictedBreak {
    pub(crate) max_advance: f32,
    // The indent and clamp are stored apart and narrowed so that every
    // evicted root's state fits a smaller wasm32 chunk.
    indent: f32,
    indent_options: IndentOptions,
    pub(crate) alignment: Option<Alignment>,
    pub(crate) last_line_alignment: Option<Alignment>,
    clamp_lines: Option<NonZeroU32>,
    /// (width, height, baseline) per inline box, in text order.
    pub(crate) boxes: Box<[(f32, f32, Option<f32>)]>,
    /// Sizes the root at another width without rebuilding, when parley can
    /// summarize this paragraph. The rebuild drops it.
    pub(crate) summary: SummaryCell,
}

// 44 bytes fill a 48-byte wasm32 chunk.
#[cfg(target_pointer_width = "32")]
const _: () = assert!(size_of::<EvictedBreak>() <= 44);

impl EvictedBreak {
    pub(crate) fn new(
        max_advance: f32,
        indent: (f32, IndentOptions),
        alignment: Option<Alignment>,
        last_line_alignment: Option<Alignment>,
        clamp_lines: Option<usize>,
        boxes: Box<[(f32, f32, Option<f32>)]>,
        summary: SummaryCell,
    ) -> Self {
        let mut breaking = Self {
            max_advance,
            indent: indent.0,
            indent_options: indent.1,
            alignment,
            last_line_alignment,
            clamp_lines: None,
            boxes,
            summary,
        };
        breaking.set_clamp_lines(clamp_lines);
        breaking
    }

    pub(crate) fn set_indent(&mut self, (indent, options): (f32, IndentOptions)) {
        self.indent = indent;
        self.indent_options = options;
    }

    pub(crate) fn clamp_lines(&self) -> Option<usize> {
        self.clamp_lines.map(|lines| lines.get() as usize)
    }

    /// Line clamps are at least one line and come from a CSS integer.
    pub(crate) fn set_clamp_lines(&mut self, lines: Option<usize>) {
        self.clamp_lines = lines.map(|lines| {
            u32::try_from(lines)
                .ok()
                .and_then(NonZeroU32::new)
                .expect("a line clamp is a positive 32-bit count")
        });
    }
}

/// What lays out an evicted root without rebuilding it.
pub(crate) struct EvictedSummary {
    /// Breaks the lines at another width.
    pub(crate) lines: parley::BreakSummary,
    /// `None` when the root has no inline boxes. A thin box keeps every
    /// evicted root's state within its wasm32 chunk.
    pub(crate) out_of_flow: Option<Box<OutOfFlow>>,
}

impl EvictedSummary {
    /// The root's inline boxes, all absolutely positioned, in layout order.
    pub(crate) fn out_of_flow_ids(&self) -> &[NodeId] {
        self.out_of_flow.as_deref().map_or(&[], |oof| &oof.ids)
    }
}

#[derive(Clone)]
pub(crate) struct OutOfFlow {
    pub(crate) ids: Box<[NodeId]>,
    /// Kept when a box's static position depends on where along its line it
    /// sits, which only the shaped line knows.
    pub(crate) static_x: Option<StaticX>,
}

/// The inline offset of each out-of-flow box in its line, in layout units, as
/// the live layout placed it when broken and aligned with `aligned`. A pass
/// with other inputs rebuilds the root instead.
#[derive(Clone)]
pub(crate) struct StaticX {
    pub(crate) aligned: AlignInputs,
    /// Per box in [`EvictedSummary::out_of_flow_ids`] order.
    pub(crate) x: Box<[f32]>,
}

/// The inputs that place items along a broken line. Floats compare by bits,
/// so the recorded offsets are only reused for identical inputs.
#[derive(Clone, Copy)]
pub(crate) struct AlignInputs {
    pub(crate) max_advance: f32,
    pub(crate) indent: (f32, IndentOptions),
    pub(crate) alignment: Option<Alignment>,
    pub(crate) last_line_alignment: Option<Alignment>,
    pub(crate) clamp_lines: Option<usize>,
}

impl PartialEq for AlignInputs {
    fn eq(&self, other: &Self) -> bool {
        self.max_advance.to_bits() == other.max_advance.to_bits()
            && self.indent.0.to_bits() == other.indent.0.to_bits()
            && self.indent.1 == other.indent.1
            && self.alignment == other.alignment
            && self.last_line_alignment == other.last_line_alignment
            && self.clamp_lines == other.clamp_lines
    }
}

/// An [`EvictedSummary`] that a rebuild, which only borrows the layout, can
/// still drop: the live layout makes it redundant.
#[derive(Default)]
pub(crate) struct SummaryCell(Mutex<Option<EvictedSummary>>);

impl SummaryCell {
    pub(crate) fn new(summary: Option<EvictedSummary>) -> Self {
        Self(Mutex::new(summary))
    }

    pub(crate) fn get_mut(&mut self) -> Option<&EvictedSummary> {
        self.0
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
    }

    fn take(&self) -> Option<EvictedSummary> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn heap_size(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map_or(0, |summary| {
                let oof = summary.out_of_flow.as_deref().map_or(0, |oof| {
                    size_of::<OutOfFlow>()
                        + size_of_val(&*oof.ids)
                        + oof.static_x.as_ref().map_or(0, |s| size_of_val(&*s.x))
                });
                summary.lines.heap_size() + oof
            })
    }
}

impl Clone for SummaryCell {
    fn clone(&self) -> Self {
        let summary = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::new(summary.as_ref().map(|summary| EvictedSummary {
            lines: summary.lines.clone(),
            out_of_flow: summary.out_of_flow.clone(),
        }))
    }
}

#[derive(Default)]
struct ReadFlag(AtomicBool);

impl Clone for ReadFlag {
    fn clone(&self) -> Self {
        Self(AtomicBool::new(self.0.load(Ordering::Relaxed)))
    }
}

std::thread_local! {
    static REBUILD_LAYOUT_CTX: RefCell<LayoutContext<TextBrush>> =
        RefCell::new(LayoutContext::new());
    static REBUILD_COUNT: Cell<u64> = const { Cell::new(0) };
    static SUMMARY_STATS: Cell<InlineSummaryStats> = const {
        Cell::new(InlineSummaryStats { summarized: 0, ambiguous: 0, unsummarized: 0 })
    };
}

/// Number of evicted layouts rebuilt on this thread so far.
pub fn inline_layout_rebuild_count() -> u64 {
    REBUILD_COUNT.with(Cell::get)
}

/// How inline layout broke evicted roots' lines on this thread so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InlineSummaryStats {
    /// Broken from the root's break summary, without rebuilding it.
    pub summarized: u64,
    /// Rebuilt because the summary could not decide a break exactly.
    pub ambiguous: u64,
    /// Rebuilt because the root has no summary, or floats were in the way.
    pub unsummarized: u64,
}

pub fn inline_summary_stats() -> InlineSummaryStats {
    SUMMARY_STATS.with(Cell::get)
}

pub(crate) fn count_inline_summary(update: impl FnOnce(&mut InlineSummaryStats)) {
    SUMMARY_STATS.with(|stats| {
        let mut value = stats.get();
        update(&mut value);
        stats.set(value);
    });
}

impl TextLayout {
    pub fn new() -> Self {
        Default::default()
    }

    /// The shaped, line-broken layout. Rebuilds it first if it was evicted,
    /// so callers never see the evicted state.
    pub fn layout(&self, nodes: &crate::NodeTree) -> &parley::layout::Layout<TextBrush> {
        self.recently_read.0.store(true, Ordering::Relaxed);
        &self.built(nodes).layout
    }

    fn built(&self, nodes: &crate::NodeTree) -> &Built {
        self.built.get_or_init(|| {
            let evicted = self
                .evicted
                .as_deref()
                .expect("an empty text layout always has its rebuild inputs");
            rebuild_evicted_layout(nodes, evicted)
        })
    }

    fn content(&self, nodes: &crate::NodeTree) -> &TextContent {
        &self.built(nodes).content
    }

    /// The text the layout was built from, building it first if construction
    /// left it unshaped.
    pub fn text(&self, nodes: &crate::NodeTree) -> &str {
        &self.content(nodes).text
    }

    /// (text node id, byte range in [`Self::text`]) per text node.
    pub fn node_ranges(&self, nodes: &crate::NodeTree) -> &[(NodeId, core::ops::Range<usize>)] {
        &self.content(nodes).node_ranges
    }

    /// Empty-line sizing needs only this classification, not rebuilt text.
    pub(crate) fn text_kind(&self, nodes: &crate::NodeTree) -> TextKind {
        self.text_kind
            .get_or_init(|| TextKind::of(&self.content(nodes).text))
    }

    pub(crate) fn set_content(
        &mut self,
        mut text: String,
        mut node_ranges: Vec<(NodeId, core::ops::Range<usize>)>,
    ) {
        // Both live as long as the paragraph; drop their push-growth slack.
        text.shrink_to_fit();
        node_ranges.shrink_to_fit();
        self.text_kind = TextKindCell::default();
        self.built.get_or_init(Box::default);
        let built = self.built.get_mut().expect("initialised just above");
        built.content = TextContent { text, node_ranges };
    }

    /// Leave the layout unshaped and its text unknown: the first build,
    /// normally its own layout call, fills both. Box construction uses this
    /// so it does not walk the subtree once more than shaping does.
    pub(crate) fn set_unbuilt(&mut self, inputs: EvictedLayout) {
        debug_assert!(inputs.breaking.is_none(), "an unbuilt layout has no lines");
        self.built = OnceLock::new();
        self.text_kind = TextKindCell::default();
        self.evict(inputs);
    }

    /// Mutable [`Self::layout`] for layout itself, so not a read that keeps it
    /// live. The text layout must not be inside `nodes` (take it out of its
    /// element first), since the rebuild reads the tree.
    pub fn layout_mut(
        &mut self,
        nodes: &crate::NodeTree,
    ) -> &mut parley::layout::Layout<TextBrush> {
        self.built(nodes);
        &mut self.built.get_mut().expect("initialised just above").layout
    }

    /// The layout, which the caller has already rebuilt with [`Self::layout_mut`].
    pub(crate) fn live(&self) -> &parley::layout::Layout<TextBrush> {
        &self.built.get().expect("text layout used while evicted").layout
    }

    /// Mutable [`Self::live`].
    pub(crate) fn live_mut(&mut self) -> &mut parley::layout::Layout<TextBrush> {
        &mut self
            .built
            .get_mut()
            .expect("text layout used while evicted")
            .layout
    }

    /// The layout for box construction to build into from scratch. Drops any
    /// eviction state, since the built layout replaces whatever it described.
    pub(crate) fn layout_for_build(&mut self) -> &mut parley::layout::Layout<TextBrush> {
        self.evicted = None;
        self.built.get_or_init(Box::default);
        &mut self.built.get_mut().expect("initialised just above").layout
    }

    /// Whether the shaped layout is currently dropped.
    pub fn is_evicted(&self) -> bool {
        self.built.get().is_none()
    }

    /// The line-breaking inputs a rebuild replays, while evicted with lines.
    pub(crate) fn evicted_break(&self) -> Option<&EvictedBreak> {
        if self.built.get().is_some() {
            return None;
        }
        self.evicted.as_deref()?.breaking.as_deref()
    }

    /// Mutable [`Self::evicted_break`], so laying out an evicted root at a
    /// new width changes the lines its rebuild will have.
    pub(crate) fn evicted_break_mut(&mut self) -> Option<&mut EvictedBreak> {
        if self.built.get().is_some() {
            return None;
        }
        self.evicted.as_deref_mut()?.breaking.as_deref_mut()
    }

    /// What lays out this root without rebuilding it, while evicted with one.
    pub(crate) fn evicted_summary(&mut self) -> Option<&EvictedSummary> {
        self.evicted_break_mut()?.summary.get_mut()
    }

    /// Heap bytes held by the break summaries of this root's eviction state,
    /// which a rebuild drops.
    pub fn break_summary_heap_size(&self) -> usize {
        self.evicted
            .as_deref()
            .and_then(|evicted| evicted.breaking.as_deref())
            .map_or(0, |breaking| breaking.summary.heap_size())
    }

    /// The live layout, or `None` while evicted. For the eviction pass only.
    pub(crate) fn live_layout(&self) -> Option<&parley::layout::Layout<TextBrush>> {
        self.built.get().map(|built| &built.layout)
    }

    /// Mutable [`Self::live_layout`].
    pub(crate) fn live_layout_mut(&mut self) -> Option<&mut parley::layout::Layout<TextBrush>> {
        self.built.get_mut().map(|built| &mut built.layout)
    }

    /// Clear the read flag, returning whether it was set.
    pub(crate) fn take_recently_read(&self) -> bool {
        self.recently_read.0.swap(false, Ordering::Relaxed)
    }

    /// Drop the shaped layout, if any; [`Self::layout`] rebuilds it from `inputs`.
    pub(crate) fn evict(&mut self, inputs: EvictedLayout) {
        if let Some(built) = self.built.take() {
            self.text_kind
                .get_or_init(|| TextKind::of(&built.content.text));
        }
        self.evicted = Some(Box::new(inputs));
    }
}

/// Lends the contexts that rebuild text outside a layout pass, which has only the tree.
pub(crate) fn with_rebuild_contexts<R>(
    nodes: &crate::NodeTree,
    f: impl FnOnce(&mut LayoutContext<TextBrush>, &mut FontContext) -> R,
) -> R {
    REBUILD_LAYOUT_CTX.with(|layout_ctx| {
        let mut font_ctx = nodes.inline_rebuild.font_ctx.lock().unwrap();
        f(&mut layout_ctx.borrow_mut(), &mut font_ctx)
    })
}

fn rebuild_evicted_layout(nodes: &crate::NodeTree, evicted: &EvictedLayout) -> Box<Built> {
    REBUILD_COUNT.with(|count| count.set(count.get() + 1));
    let mut scratch = TextLayout::unfilled();
    with_rebuild_contexts(nodes, |layout_ctx, font_ctx| {
        crate::layout::construct::build_inline_layout_into(
            nodes,
            layout_ctx,
            font_ctx,
            &mut scratch,
            evicted.scale,
            evicted.node_id,
        );
    });
    nodes.inline_rebuild.rebuilt.lock().unwrap().push(evicted.node_id);
    let mut built = scratch.built.take().expect("built just above");
    let Some(breaking) = evicted.breaking.as_deref() else {
        return built;
    };
    let layout = &mut built.layout;
    // The live layout sizes the root until it is evicted again, which
    // summarizes it afresh.
    drop(breaking.summary.take());
    debug_assert_eq!(layout.inline_boxes().len(), breaking.boxes.len());
    for (ibox, &(width, height, baseline)) in layout.inline_boxes_mut().zip(breaking.boxes.iter()) {
        ibox.width = width;
        ibox.height = height;
        ibox.baseline = baseline;
    }
    layout.set_text_indent(breaking.indent, breaking.indent_options);
    layout.break_all_lines(Some(breaking.max_advance));
    if let Some(max_lines) = breaking.clamp_lines() {
        layout.truncate_lines(max_lines);
    }
    if let Some(alignment) = breaking.alignment {
        layout.align(
            alignment,
            AlignmentOptions {
                align_when_overflowing: false,
                last_line_alignment: breaking.last_line_alignment,
            },
        );
    }
    built
}

impl std::fmt::Debug for TextLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TextLayout")
    }
}

// TODO: support keypress events
pub enum GeneratedTextInputEvent {
    Input,
    Select,
    PreEditChange,
    Submit,
}

pub struct TextInputData {
    /// A parley TextEditor instance
    pub editor: Box<parley::PlainEditor<TextBrush>>,
    /// Whether the input is a singleline or multiline input
    pub is_multiline: bool,
    /// The scroll offset of the text content within the input, in CSS (unscaled) pixels.
    ///
    /// For single-line inputs this is a horizontal offset; for multi-line inputs it is a
    /// vertical offset. It is kept up to date so that the caret remains visible within the
    /// input's content box.
    pub scroll_offset: f32,
    /// True when the editor currently holds the
    /// element's `placeholder` attribute text (because `value`
    /// was empty).
    pub is_showing_placeholder: bool,
    /// The `::placeholder` colour (opacity folded into its alpha)
    /// while the placeholder shows. Paint draws the placeholder
    /// text in it instead of the input's own `color`.
    pub placeholder_color: Option<style::color::AbsoluteColor>,
    /// The `::placeholder` font style while the placeholder shows. Kept
    /// so a restyle can tell whether the editor's style is stale.
    pub placeholder_font_style: Option<parley::FontStyle>,
    /// Authoritative IDL `.value` override. When Some,
    /// layout construction displays this instead of the `value`
    /// content attribute: HTML decouples the IDL value (live,
    /// dirty) from the content attribute (defaultValue). Set by
    /// DocumentMutator::set_text_input_value; None means the
    /// attribute still drives the display (pre-dirty state).
    pub value_override: Option<String>,
}

// FIXME: Implement Clone for PlainEditor
impl Clone for TextInputData {
    fn clone(&self) -> Self {
        TextInputData::new(self.is_multiline)
    }
}

impl TextInputData {
    pub fn new(is_multiline: bool) -> Self {
        let editor = Box::new(parley::PlainEditor::new(16.0));
        Self {
            editor,
            is_multiline,
            scroll_offset: 0.0,
            is_showing_placeholder: false,
            placeholder_color: None,
            placeholder_font_style: None,
            value_override: None,
        }
    }

    pub fn set_text(
        &mut self,
        font_ctx: &mut FontContext,
        layout_ctx: &mut LayoutContext<TextBrush>,
        text: &str,
    ) {
        if self.editor.text() != text {
            self.editor.set_text(text);
            self.editor.driver(font_ctx, layout_ctx).refresh_layout();
        }
    }

    /// Recompute [`Self::scroll_offset`] so that the caret stays visible within the input's
    /// content box.
    ///
    /// `content_box_width` and `content_box_height` are the dimensions of the input's content
    /// box in CSS (unscaled) pixels.
    pub fn clamp_scroll_offset(&mut self, content_box_width: f32, content_box_height: f32) {
        let Some(layout) = self.editor.try_layout() else {
            return;
        };
        // Parley lays out at the editor's scale, so its geometry is in scaled (device) pixels.
        // We convert into CSS (unscaled) pixels to match `scroll_offset` and the content box.
        let scale = layout.scale();

        // The caret geometry relative to the start of the text content.
        let Some(caret) = self.editor.cursor_geometry(1.5) else {
            return;
        };

        // Caret bounds and content/viewport extents along the scrolling axis (CSS pixels).
        let (caret_start, caret_end, content, viewport) = if self.is_multiline {
            (
                caret.y0 as f32 / scale,
                caret.y1 as f32 / scale,
                layout.height() / scale,
                content_box_height,
            )
        } else {
            (
                caret.x0 as f32 / scale,
                caret.x1 as f32 / scale,
                layout.full_width() / scale,
                content_box_width,
            )
        };

        let mut offset = self.scroll_offset;

        // Scroll so that both edges of the caret are within the visible region.
        if caret_end > offset + viewport {
            offset = caret_end - viewport;
        }
        if caret_start < offset {
            offset = caret_start;
        }

        // Never scroll past the content, and never scroll into negative space. The content
        // extent includes the caret so that a caret at the very end remains fully visible
        // (its rendered width extends slightly past the text).
        let max_offset = (content.max(caret_end) - viewport).max(0.0);
        self.scroll_offset = offset.clamp(0.0, max_offset);
    }

    /// The maximum valid value of [`Self::scroll_offset`] (in CSS pixels) given the input's
    /// content box, i.e. the extent by which the text content overflows the content box along
    /// the input's scroll axis.
    ///
    /// `content_box_width` and `content_box_height` are the dimensions of the input's content
    /// box in CSS (unscaled) pixels.
    pub fn max_scroll_offset(&self, content_box_width: f32, content_box_height: f32) -> f32 {
        let Some(layout) = self.editor.try_layout() else {
            return 0.0;
        };
        let scale = layout.scale();
        let (content, viewport) = if self.is_multiline {
            (layout.height() / scale, content_box_height)
        } else {
            (layout.full_width() / scale, content_box_width)
        };
        (content - viewport).max(0.0)
    }

    /// Scroll the input's text content by `delta` CSS pixels along its scroll axis (horizontal
    /// for single-line inputs, vertical for multi-line inputs), clamping to the scrollable
    /// range.
    ///
    /// Returns the portion of `delta` that could not be consumed (because the input was already
    /// scrolled to its limit), so the caller can bubble it up to an ancestor scroller.
    pub fn scroll_by(
        &mut self,
        delta: f32,
        content_box_width: f32,
        content_box_height: f32,
    ) -> f32 {
        let max_offset = self.max_scroll_offset(content_box_width, content_box_height);
        if max_offset <= 0.0 {
            return delta;
        }

        // Match the sign convention used for block scrolling: a positive delta decreases the
        // scroll offset.
        let new_offset = (self.scroll_offset - delta).clamp(0.0, max_offset);
        let consumed = self.scroll_offset - new_offset;
        self.scroll_offset = new_offset;
        delta - consumed
    }

    pub(crate) fn apply_keypress_event(
        &mut self,
        font_ctx: &mut FontContext,
        layout_ctx: &mut LayoutContext<TextBrush>,
        shell_provider: &dyn ShellProvider,
        event: BlitzKeyEvent,
    ) -> Option<GeneratedTextInputEvent> {
        // Do nothing if it is a keyup event
        if !event.state.is_pressed() {
            return None;
        }

        let mods = event.modifiers;
        let shift = mods.contains(Modifiers::SHIFT);
        let action_mod = mods.contains(ACTION_MOD);

        let is_multiline = self.is_multiline;
        let editor = &mut self.editor;
        let mut driver = editor.driver(font_ctx, layout_ctx);
        match event.key {
            Key::Character(c) if action_mod && matches!(c.as_str(), "c" | "x" | "v") => {
                match c.to_lowercase().as_str() {
                    "c" => {
                        if let Some(text) = driver.editor.selected_text() {
                            let _ = shell_provider.set_clipboard_text(text.to_owned());
                        }
                    }
                    "x" => {
                        if let Some(text) = driver.editor.selected_text() {
                            let _ = shell_provider.set_clipboard_text(text.to_owned());
                            driver.delete_selection()
                        }
                    }
                    "v" => {
                        let text = shell_provider.get_clipboard_text().unwrap_or_default();
                        driver.insert_or_replace_selection(&text)
                    }
                    _ => unreachable!(),
                }

                return Some(GeneratedTextInputEvent::Input);
            }
            Key::Character(c) if action_mod && matches!(c.to_lowercase().as_str(), "a") => {
                if shift {
                    driver.collapse_selection()
                } else {
                    driver.select_all()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::Named(NamedKey::ArrowLeft) => {
                if action_mod {
                    if shift {
                        driver.select_word_left()
                    } else {
                        driver.move_word_left()
                    }
                } else if shift {
                    driver.select_left()
                } else {
                    driver.move_left()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::Named(NamedKey::ArrowRight) => {
                if action_mod {
                    if shift {
                        driver.select_word_right()
                    } else {
                        driver.move_word_right()
                    }
                } else if shift {
                    driver.select_right()
                } else {
                    driver.move_right()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::Named(NamedKey::ArrowUp) => {
                if shift {
                    driver.select_up()
                } else {
                    driver.move_up()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::Named(NamedKey::ArrowDown) => {
                if shift {
                    driver.select_down()
                } else {
                    driver.move_down()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::Named(NamedKey::Home) => {
                if action_mod {
                    if shift {
                        driver.select_to_text_start()
                    } else {
                        driver.move_to_text_start()
                    }
                } else if shift {
                    driver.select_to_line_start()
                } else {
                    driver.move_to_line_start()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::Named(NamedKey::End) => {
                if action_mod {
                    if shift {
                        driver.select_to_text_end()
                    } else {
                        driver.move_to_text_end()
                    }
                } else if shift {
                    driver.select_to_line_end()
                } else {
                    driver.move_to_line_end()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::Named(NamedKey::Delete) => {
                if action_mod {
                    driver.delete_word()
                } else {
                    driver.delete()
                }
                return Some(GeneratedTextInputEvent::Input);
            }

            // On macOS this is handled by the apple standard keybindings
            #[cfg(not(target_os = "macos"))]
            Key::Named(NamedKey::Backspace) => {
                if action_mod {
                    driver.backdelete_word()
                } else {
                    driver.backdelete()
                }
                return Some(GeneratedTextInputEvent::Input);
            }

            Key::Character(c) if c == "\n" => {
                if is_multiline {
                    driver.insert_or_replace_selection("\n");
                    return Some(GeneratedTextInputEvent::Input);
                } else {
                    return Some(GeneratedTextInputEvent::Submit);
                }
            }
            Key::Named(NamedKey::Enter) => {
                if is_multiline {
                    driver.insert_or_replace_selection("\n");
                    return Some(GeneratedTextInputEvent::Input);
                } else {
                    return Some(GeneratedTextInputEvent::Submit);
                }
            }
            Key::Character(s)
                if !mods.contains(Modifiers::CONTROL) && !mods.contains(Modifiers::META) =>
            {
                driver.insert_or_replace_selection(&s);
                return Some(GeneratedTextInputEvent::Input);
            }
            _ => {}
        };

        None
    }

    pub(crate) fn apply_apple_standard_keybinding(
        &mut self,
        font_ctx: &mut FontContext,
        layout_ctx: &mut LayoutContext<TextBrush>,
        shell_provider: &dyn ShellProvider,
        command: &str,
    ) -> Option<GeneratedTextInputEvent> {
        let editor = &mut self.editor;
        let mut driver = editor.driver(font_ctx, layout_ctx);
        let is_multiline = self.is_multiline;

        match command {
            // Inserting Content

            // Inserts a backtab character.
            "insertBacktab:" => {}
            // Inserts a container break, such as a new page break.
            "insertContainerBreak:" => {}
            // Inserts a double quotation mark without substituting a curly quotation mark.
            "insertDoubleQuoteIgnoringSubstitution:" => {
                driver.insert_or_replace_selection("\"");
                return Some(GeneratedTextInputEvent::Input);
            }
            // Inserts a line break character.
            "insertLineBreak:" => {
                driver.insert_or_replace_selection("\n");
                return Some(GeneratedTextInputEvent::Input);
            }
            // Inserts a newline character.
            "insertNewline:" => {
                if is_multiline {
                    driver.insert_or_replace_selection("\n");
                    return Some(GeneratedTextInputEvent::Input);
                } else {
                    return Some(GeneratedTextInputEvent::Submit);
                }
            }
            // Inserts a newline character without invoking the field editor’s normal handling to end editing.
            "insertNewlineIgnoringFieldEditor:" => {
                driver.insert_or_replace_selection("\n");
                return Some(GeneratedTextInputEvent::Input);
            }
            // Inserts a paragraph separator.
            "insertParagraphSeparator:" => {
                driver.insert_or_replace_selection("\n");
                return Some(GeneratedTextInputEvent::Input);
            }
            "insertSingleQuoteIgnoringSubstitution:" => {
                driver.insert_or_replace_selection("'");
                return Some(GeneratedTextInputEvent::Input);
            }
            // Inserts a tab character.
            "insertTab:" | "insertTabIgnoringFieldEditor:" => {
                // Ignore for now seeing as parley has poor support for laying out tabs
            }
            // Inserts the text you specify.
            "insertText:" => {}

            // Deleting Content

            // Deletes content moving backward from the current insertion point.
            // TODO: handle deleteBackwardByDecomposingPreviousCharacter separately
            "deleteBackward:" | "deleteBackwardByDecomposingPreviousCharacter:" => {
                driver.backdelete();
                return Some(GeneratedTextInputEvent::Input);
            }
            "deleteForward:" => {
                driver.delete();
                return Some(GeneratedTextInputEvent::Input);
            }
            // Deletes content from the insertion point to the beginning of the current line.
            "deleteToBeginningOfLine:" => {
                if driver.editor.raw_selection().is_collapsed() {
                    driver.select_to_line_start();
                }
                driver.delete_selection();
                return Some(GeneratedTextInputEvent::Input);
            }
            // Deletes content from the insertion point to the beginning of the current paragraph.
            "deleteToEndOfLine:" => {
                if driver.editor.raw_selection().is_collapsed() {
                    driver.select_to_line_end();
                }
                driver.delete_selection();
                return Some(GeneratedTextInputEvent::Input);
            }
            "deleteToBeginningOfParagraph:" => {
                if driver.editor.raw_selection().is_collapsed() {
                    driver.select_to_hard_line_start();
                }
                driver.delete_selection();
                return Some(GeneratedTextInputEvent::Input);
            }

            // Deletes content from the insertion point to the end of the current line.
            "deleteToEndOfParagraph:" => {
                if driver.editor.raw_selection().is_collapsed() {
                    driver.select_to_hard_line_end();
                }
                driver.delete_selection();
                return Some(GeneratedTextInputEvent::Input);
            }
            // Deletes content from the insertion point to the end of the current paragraph.
            "deleteWordBackward:" => {
                driver.backdelete_word();
                return Some(GeneratedTextInputEvent::Input);
            }
            // Deletes the word preceding the current insertion point.
            "deleteWordForward:" => {
                driver.delete_word();
                return Some(GeneratedTextInputEvent::Input);
            }
            // Deletes the current selection, placing it in a temporary buffer, such as the Clipboard.
            "yank:" => {
                if let Some(text) = driver.editor.selected_text() {
                    let _ = shell_provider.set_clipboard_text(text.to_owned());
                    driver.delete_selection();
                    return Some(GeneratedTextInputEvent::Input);
                }
            }

            // Moving the Insertion Pointer

            // Moves the insertion pointer backward in the current content.
            "moveBackward:" => {
                driver.move_left(); // TODO: Bidi-aware
                return Some(GeneratedTextInputEvent::Select);
            }

            // Moves the insertion pointer down in the current content.
            "moveDown:" => {
                driver.move_down();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Moves the insertion pointer forward in the current content.
            "moveForward:" => {
                driver.move_right();
                return Some(GeneratedTextInputEvent::Select);
            } // TODO: Bidi-aware

            // Moves the insertion pointer left in the current content.
            "moveLeft:" => {
                driver.move_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Moves the insertion pointer right in the current content.
            "moveRight:" => {
                driver.move_right();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Moves the insertion pointer up in the current content.
            "moveUp:" => {
                driver.move_up();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Modifying the Selection

            // Extends the selection to include the content before the current selection.
            "moveBackwardAndModifySelection:" => {
                driver.select_left(); // TODO: Bidi-aware
                return Some(GeneratedTextInputEvent::Select);
            }
            // Extends the selection to include the content below the current selection.
            "moveDownAndModifySelection:" => {
                driver.select_down();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Extends the selection to include the content after the current selection.
            "moveForwardAndModifySelection:" => {
                driver.select_right(); // TODO: Bidi-aware
                return Some(GeneratedTextInputEvent::Select);
            }
            // Extends the selection to include the content to the left of the current selection.
            "moveLeftAndModifySelection:" => {
                driver.select_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Extends the selection to include the content to the right of the current selection.
            "moveRightAndModifySelection:" => {
                driver.select_right();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Extends the selection to include the content above the current selection.
            "moveUpAndModifySelection:" => {
                driver.select_up();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Changing the Selection
            "selectAll:" => {
                driver.select_all();
                return Some(GeneratedTextInputEvent::Select);
            }
            "selectLine:" => {
                driver.move_to_line_start();
                driver.select_to_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "selectParagraph:" => {
                driver.move_to_hard_line_start();
                driver.select_to_hard_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "selectWord:" => {
                // TODO
            }

            // Moving the Selection in Documents
            "moveToBeginningOfDocument:" => {
                driver.move_to_text_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToBeginningOfDocumentAndModifySelection:" => {
                driver.select_to_text_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfDocument:" => {
                driver.move_to_text_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfDocumentAndModifySelection:" => {
                driver.move_to_text_end();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Moving the Selection in Paragraphs
            "moveParagraphBackwardAndModifySelection:" => {}
            "moveParagraphForwardAndModifySelection:" => {}
            "moveToBeginningOfParagraph:" => {
                driver.move_to_hard_line_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToBeginningOfParagraphAndModifySelection:" => {
                driver.select_to_hard_line_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfParagraph:" => {
                driver.move_to_hard_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfParagraphAndModifySelection:" => {
                driver.select_to_hard_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Moving the Selection in Lines of Text
            "moveToBeginningOfLine:" => {
                driver.move_to_line_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToBeginningOfLineAndModifySelection:" => {
                driver.select_to_line_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfLine:" => {
                driver.move_to_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfLineAndModifySelection:" => {
                driver.select_to_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToLeftEndOfLine:" => {
                driver.move_to_text_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToLeftEndOfLineAndModifySelection:" => {
                driver.select_to_line_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToRightEndOfLine:" => {
                driver.move_to_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToRightEndOfLineAndModifySelection:" => {
                driver.select_to_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Moving the Selection by Word Boundaries
            "moveWordBackward:" => {
                driver.move_word_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordBackwardAndModifySelection:" => {
                driver.select_word_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordForward:" => {
                driver.move_word_right();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordForwardAndModifySelection:" => {
                driver.select_word_right();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordLeft:" => {
                driver.move_word_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordLeftAndModifySelection:" => {
                driver.select_word_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordRight:" => {
                driver.move_word_right();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordRightAndModifySelection:" => {
                driver.select_word_right();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Scrolling Content

            // Scrolls the content down by a page.
            "scrollPageDown:" => {}
            // Scrolls the content up by a page.
            "scrollPageUp:" => {}
            // Scrolls the content down by a line.
            "scrollLineDown:" => {}
            // Scrolls the content up by a line.
            "scrollLineUp:" => {}
            // Scrolls the content to the beginning of the document.
            "scrollToBeginningOfDocument:" => {}
            // Scrolls the content to the end of the document.
            "scrollToEndOfDocument:" => {}
            // Moves the visible content region down by a page.
            "pageDown:" => {}
            // Moves the visible content region up by a page.
            "pageUp:" => {}
            // Moves the visible content region down by a page, and extends the current selection.
            "pageDownAndModifySelection:" => {}
            // Moves the visible content region up by a page, and extends the current selection.
            "pageUpAndModifySelection:" => {}
            // Moves the visible content region so the current selection is visually centered.
            "centerSelectionInVisibleArea:" => {}

            // Transposing Elements

            // Transposes the content around the current selection.
            "transpose:" => {}
            // Transposes the words around the current selection.
            "transposeWords:" => {}

            // Indenting Content
            // Indents the content at the current selection.
            "indent:" => {}

            // Canceling Operations
            // Cancels the current operation.
            "cancelOperation:" => {}

            // Supporting QuickLook
            // Invokes QuickLook to preview the current selection.
            "quickLookPreviewItems:" => {}

            // Supporting Writing Directions
            "makeBaseWritingDirectionLeftToRight:" => {}
            "makeBaseWritingDirectionNatural:" => {}
            "makeBaseWritingDirectionRightToLeft:" => {}
            "makeTextWritingDirectionLeftToRight:" => {}
            "makeTextWritingDirectionNatural:" => {}
            "makeTextWritingDirectionRightToLeft:" => {}

            // Changing Capitalization
            "capitalizeWord:" => {}
            "changeCaseOfLetter:" => {}
            "lowercaseWord:" => {}
            "uppercaseWord:" => {}

            // Supporting Marked Selections
            "setMark:" => {}
            "selectToMark:" => {}
            "deleteToMark:" => {}
            "swapWithMark:" => {}

            // Supporting Autocomplete
            "complete:" => {}

            // Instance Methods
            "showContextMenuForSelection:" => {}

            // Unknown command
            _ => {}
        };

        None
    }

    pub(crate) fn apply_ime_event(
        &mut self,
        font_ctx: &mut FontContext,
        layout_ctx: &mut LayoutContext<TextBrush>,
        event: BlitzImeEvent,
    ) -> Option<GeneratedTextInputEvent> {
        let editor = &mut self.editor;
        let mut driver = editor.driver(font_ctx, layout_ctx);

        match event {
            BlitzImeEvent::Enabled => {
                // Do nothing
                None
            }
            BlitzImeEvent::Disabled => {
                driver.clear_compose();
                Some(GeneratedTextInputEvent::PreEditChange)
            }
            BlitzImeEvent::Commit(text) => {
                driver.insert_or_replace_selection(&text);
                Some(GeneratedTextInputEvent::Input)
            }
            BlitzImeEvent::Preedit(text, cursor) => {
                if text.is_empty() {
                    driver.clear_compose();
                } else {
                    driver.set_compose(&text, cursor);
                }
                Some(GeneratedTextInputEvent::PreEditChange)
            }
            BlitzImeEvent::DeleteSurrounding {
                before_bytes,
                after_bytes,
            } => {
                let _ = before_bytes;
                let _ = after_bytes;
                // TODO
                None
            }
        }
    }
}
