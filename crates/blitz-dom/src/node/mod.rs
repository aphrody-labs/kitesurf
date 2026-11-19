#![allow(clippy::module_inception)]

mod attributes;
mod child_list;
#[cfg(feature = "custom-widget")]
mod custom_widget;
mod element;
mod node;
pub(crate) mod scrollbar;
mod serialize;
mod stylo_data;
#[cfg(feature = "svg")]
mod svg;
mod text;

pub use attributes::{AttrName, Attribute, Attributes};
#[cfg(feature = "custom-widget")]
pub use custom_widget::{
    ComputedStyles, CustomWidgetData, CustomWidgetStatus, ProxyRenderContext, Widget,
};
pub use element::{
    CanvasData, DocumentData, ElementData, ElementStateBits, ImageData, ImageResourceData,
    LayoutData, ListItemLayout, ListItemLayoutPosition, Marker, RasterImageData, SelectorFlagsRef,
    SpecialElementData, SpecialElementType, Status, parse_class_list,
};
pub use node::*;
pub use scrollbar::{ScrollbarColor, ScrollbarRef, ScrollbarWidth};
pub use stylo_data::ComputedStyleRef;
#[cfg(feature = "svg")]
pub use svg::{SvgImageData, SvgIntrinsicDimensions};
pub use text::{
    GeneratedTextInputEvent, InlineSummaryStats, TextBrush, TextInputData, TextLayout,
    inline_layout_rebuild_count, inline_summary_stats,
};
pub(crate) use text::{
    AlignInputs, EvictedBreak, EvictedLayout, EvictedSummary, InlineRebuild, OutOfFlow, StaticX,
    SummaryCell, count_inline_summary,
    with_rebuild_contexts,
};
