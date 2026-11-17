use crate::node::{Node, NodeData};
use color::{AlphaColor, Srgb};
use keyboard_types::Modifiers;
use std::borrow::Cow;
use style::color::AbsoluteColor;

#[cfg(target_os = "macos")]
pub(crate) const ACTION_MOD: Modifiers = Modifiers::META;
#[cfg(not(target_os = "macos"))]
pub(crate) const ACTION_MOD: Modifiers = Modifiers::CONTROL;

pub type Color = AlphaColor<Srgb>;

/// Decode raw font bytes, decompressing WOFF/WOFF2 if the `woff` feature is enabled.
/// Returns the original slice unchanged for TTF/OTF input, and also on decompression
/// failure. With the `woff` feature disabled, all input passes through unchanged.
pub fn decode_font_bytes(bytes: &[u8]) -> Cow<'_, [u8]> {
    if bytes.len() < 4 {
        return Cow::Borrowed(bytes);
    }
    match &bytes[0..4] {
        #[cfg(feature = "woff")]
        b"wOFF" => wuff::decompress_woff1(bytes)
            .map(Cow::Owned)
            .unwrap_or_else(|_| {
                #[cfg(feature = "tracing")]
                tracing::warn!("Failed to decompress woff1 font");
                Cow::Borrowed(bytes)
            }),
        #[cfg(feature = "woff")]
        b"wOF2" => wuff::decompress_woff2(bytes)
            .map(Cow::Owned)
            .unwrap_or_else(|_| {
                #[cfg(feature = "tracing")]
                tracing::warn!("Failed to decompress woff2 font");
                Cow::Borrowed(bytes)
            }),
        _ => Cow::Borrowed(bytes),
    }
}

#[cfg(feature = "svg")]
use std::sync::{Arc, LazyLock};
#[cfg(feature = "svg")]
use usvg::fontdb;
#[cfg(feature = "svg")]
pub(crate) static FONT_DB: LazyLock<std::sync::RwLock<Arc<fontdb::Database>>> =
    LazyLock::new(|| {
        let mut db = fontdb::Database::new();
        // No-op under wasm (no filesystem); the embedder pushes faces in
        // through register_svg_font instead.
        db.load_system_fonts();
        std::sync::RwLock::new(Arc::new(db))
    });

/// Parsed trees keyed by the exact source they were built from. Icon systems
/// repeat one `<svg>` per use site (136 inline svgs, 47 distinct on one
/// heavy page), and
/// box construction re-parses each from scratch.
///
/// Bounded because a mutating `<svg>` re-serializes on every write,
/// so an animated one would otherwise add an entry per frame forever.
#[cfg(feature = "svg")]
static SVG_TREE_CACHE: LazyLock<
    std::sync::RwLock<std::collections::HashMap<Box<str>, crate::node::SvgImageData>>,
> = LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));

#[cfg(feature = "svg")]
const SVG_TREE_CACHE_CAP: usize = 512;

/// Bumped by every clear, so a parse that overlapped one can tell.
#[cfg(feature = "svg")]
static SVG_TREE_CACHE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Drop every cached tree. Parsing resolves text against `FONT_DB` and the
/// default family, so a face arriving later invalidates anything already built.
#[cfg(feature = "svg")]
pub fn clear_svg_tree_cache() {
    // Bump before the clear, never after: a parse in flight checks this on the
    // way in and would otherwise repopulate the cache with a stale tree.
    SVG_TREE_CACHE_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if let Ok(mut cache) = SVG_TREE_CACHE.write() {
        cache.clear();
    }
}

/// Which generic family a registered SVG font should answer for.
#[cfg(feature = "svg")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SvgGenericSlot {
    SansSerif,
    Serif,
    Monospace,
}

/// Register a font face for SVG `<text>` shaping, optionally pointing a
/// generic family (and usvg's default family) at it.
///
/// usvg converts text to paths through its own fontdb, which on a
/// system-font-less target (wasm) stays empty: every `<text>` was silently
/// dropped, so SVG charts painted their series and no labels. Embedders
/// that ship their own faces call this with the same bytes they hand the
/// text stack: `data` is shared, not copied.
#[cfg(feature = "svg")]
pub fn register_svg_font(
    data: Arc<dyn AsRef<[u8]> + Send + Sync>,
    slot: Option<SvgGenericSlot>,
) {
    let Ok(mut db_slot) = FONT_DB.write() else {
        return;
    };
    let mut db = (**db_slot).clone();
    let ids = db.load_font_source(fontdb::Source::Binary(data));
    // Generic mapping reads the family name out of the face itself, so it
    // can't drift from whatever the embedder actually loaded.
    let family = ids
        .first()
        .and_then(|id| db.face(*id))
        .and_then(|face| face.families.first().map(|(name, _)| name.clone()));
    if let (Some(slot), Some(family)) = (slot, family) {
        match slot {
            SvgGenericSlot::SansSerif => {
                db.set_sans_serif_family(family.clone());
                // usvg's font selector appends fontdb's serif generic as its
                // last resort, so an unregistered named family such as
                // `Arial` resolves to nothing and the text is dropped.

                // Point every generic at the sans face as a floor; a later
                // register_svg_font for the serif/monospace slots overwrites
                // its own entry with the real face.
                db.set_serif_family(family.clone());
                db.set_monospace_family(family.clone());
                db.set_cursive_family(family.clone());
                db.set_fantasy_family(family.clone());
                // usvg falls back to Options::font_family for text with no
                // usable family; point that at the sans face too.
                if let Ok(mut default) = SVG_DEFAULT_FAMILY.write() {
                    *default = Some(family);
                }
            }
            SvgGenericSlot::Serif => db.set_serif_family(family),
            SvgGenericSlot::Monospace => db.set_monospace_family(family),
        }
    }
    *db_slot = Arc::new(db);
    // After the swap, not before: a tree parsed in between would be built
    // against the new db and must survive, one parsed before must not.
    clear_svg_tree_cache();
}

/// Family usvg uses when the text names none we know; set by
/// `register_svg_font` for the sans-serif slot.
#[cfg(feature = "svg")]
pub(crate) static SVG_DEFAULT_FAMILY: LazyLock<std::sync::RwLock<Option<String>>> =
    LazyLock::new(|| std::sync::RwLock::new(None));

/// Which kind of CSS image layer list (`background-image` or `mask-image`) to
/// flush from style to dedicated storage on the node.
#[derive(Clone, Copy, Debug)]
pub enum ImageLayerKind {
    Background,
    Mask,
}

impl ImageLayerKind {
    pub fn image_type(self, idx: usize) -> ImageType {
        match self {
            Self::Background => ImageType::Background(idx),
            Self::Mask => ImageType::Mask(idx),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ImageType {
    Image,
    Background(usize),
    Mask(usize),
}

/// A point
#[derive(Clone, Debug, Copy, Eq, PartialEq)]
pub struct Point<T> {
    /// The x coordinate
    pub x: T,
    /// The y coordinate
    pub y: T,
}

impl Point<f64> {
    pub const ZERO: Self = Point { x: 0.0, y: 0.0 };
}

// Debug print an RcDom
pub fn walk_tree(indent: usize, node: &Node) {
    // Skip all-whitespace text nodes entirely
    if let NodeData::Text(data) = &node.data {
        if data.content.chars().all(|c| c.is_ascii_whitespace()) {
            return;
        }
    }

    print!("{}", " ".repeat(indent));
    let id = node.id;
    match &node.data {
        NodeData::Document(_) => println!("#Document {id}"),

        NodeData::Text(data) => {
            if data.content.chars().all(|c| c.is_ascii_whitespace()) {
                println!("{id} #text: <whitespace>");
            } else {
                let content = data.content.trim();
                if content.len() > 10 {
                    println!(
                        "#text {id}: {}...",
                        content
                            .split_at(content.char_indices().take(10).last().unwrap().0)
                            .0
                            .escape_default()
                    )
                } else {
                    println!("#text {id}: {}", data.content.trim().escape_default())
                }
            }
        }

        NodeData::Comment { .. } => println!("<!-- COMMENT {id} -->"),

        NodeData::AnonymousBlock(_) => println!("{id} AnonymousBlock"),

        NodeData::ShadowRoot(data) => println!("#shadow-root {id} ({:?})", data.mode),

        NodeData::Element(data) => {
            print!("<{} {id}", data.name.local);
            for attr in data.attrs.iter() {
                print!(" {}=\"{}\"", attr.name.local, attr.value);
            }
            if !node.children.is_empty() {
                println!(">");
            } else {
                println!("/>");
            }
        } // NodeData::Doctype {
          //     ref name,
          //     ref public_id,
          //     ref system_id,
          // } => println!("<!DOCTYPE {} \"{}\" \"{}\">", name, public_id, system_id),
          // NodeData::ProcessingInstruction { .. } => unreachable!(),
    }

    if !node.children.is_empty() {
        for child_id in node.children.iter() {
            walk_tree(indent + 2, node.with(*child_id));
        }

        if let NodeData::Element(data) = &node.data {
            println!("{}</{}>", " ".repeat(indent), data.name.local);
        }
    }
}

/// Parse an SVG image.
#[cfg(feature = "svg")]
/// SVG fragment views: `image.svg#viewname` selects a `<view>` whose
/// viewBox (and preserveAspectRatio, when present) override the root's
/// (SVG2 linking.html#ViewElement). usvg has no fragment handling, so
/// rewrite the root tag before the parse: string-level, same technique
/// as the inline-svg stamps. An unmatched or viewBox-less view leaves
/// the source untouched, which is the spec's fallback to a normal view.
pub fn apply_svg_view_fragment(source: &str, fragment: &str) -> Option<String> {
    if fragment.is_empty() {
        return None;
    }
    let id_attr_a = format!("id=\"{fragment}\"");
    let id_attr_b = format!("id='{fragment}'");
    // Find the <view ...> opening tag carrying this id.
    let mut at = 0usize;
    let view_tag = loop {
        let rel = source[at..].find("<view")?;
        let start = at + rel;
        let end = start + source[start..].find('>')?;
        let tag = &source[start..=end];
        if tag.contains(&id_attr_a) || tag.contains(&id_attr_b) {
            break tag;
        }
        at = end + 1;
    };
    let attr_of = |tag: &str, name: &str| -> Option<String> {
        let pos = tag.find(&format!("{name}="))?;
        let rest = &tag[pos + name.len() + 1..];
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let inner = &rest[1..];
        Some(inner[..inner.find(quote)?].to_string())
    };
    let vb = attr_of(view_tag, "viewBox")?;
    // An invalid view viewBox (malformed, or non-positive extents) makes
    // the whole fragment invalid; the spec falls back to the normal view,
    // so the source must stay untouched. Without this gate the negative
    // extents would be stamped and then disable rendering outright.
    {
        // Every token must parse (filter_map would let "0 0 bogus 0 100"
        // pass as four numbers) and be finite.
        let tokens: Vec<&str> = vb
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|s| !s.is_empty())
            .collect();
        if tokens.len() != 4 {
            return None;
        }
        let nums: Vec<f64> = tokens
            .iter()
            .filter_map(|s| s.parse().ok())
            .filter(|n: &f64| n.is_finite())
            .collect();
        if nums.len() != 4 || nums[2] <= 0.0 || nums[3] <= 0.0 {
            return None;
        }
    }
    let par = attr_of(view_tag, "preserveAspectRatio");

    // Rewrite the root <svg ...> tag: drop its own viewBox (and PAR when
    // the view supplies one), then inject the view's values.
    let root_start = source.find("<svg")?;
    let root_end = root_start + source[root_start..].find('>')?;
    let mut root = source[root_start..root_end].to_string();
    let strip_attr = |tag: &mut String, name: &str| {
        if let Some(pos) = tag.find(&format!(" {name}=")) {
            let rest = &tag[pos + name.len() + 2..];
            if let Some(q) = rest.chars().next() {
                if q == '"' || q == '\'' {
                    if let Some(close) = rest[1..].find(q) {
                        let end = pos + name.len() + 2 + 1 + close + 1;
                        tag.replace_range(pos..end, "");
                    }
                }
            }
        }
    };
    strip_attr(&mut root, "viewBox");
    if par.is_some() {
        strip_attr(&mut root, "preserveAspectRatio");
    }
    let mut inject = format!(" viewBox=\"{vb}\"");
    if let Some(par) = &par {
        inject.push_str(&format!(" preserveAspectRatio=\"{par}\""));
    }
    root.insert_str(4, &inject);
    let mut out = String::with_capacity(source.len() + inject.len());
    out.push_str(&source[..root_start]);
    out.push_str(&root);
    out.push_str(&source[root_end..]);
    Some(out)
}

/// SVG stacks: `sprite.svg#name` shows one icon out of many through
/// `svg>[id]{display:none} svg>[id]:target{display:inline}` in the
/// sprite's `<style>` (tldraw's toolbar icons are CSS masks of this
/// kind). usvg knows no document URL, so it cannot match `:target`,
/// and simplecss drops the rule: every icon stayed hidden. Rewrite
/// `:target` inside `<style>` blocks to the id selector it stands for.
///
/// Without a fragment nothing is targeted, which is what the dropped
/// rule already gives, so there is nothing to rewrite.
pub fn apply_svg_target_fragment(source: &str, fragment: &str) -> Option<String> {
    // The fragment lands inside a quoted attribute selector; refuse
    // anything that could close the quote or the style block early.
    if fragment.is_empty()
        || fragment
            .chars()
            .any(|c| matches!(c, '"' | '\\' | '<' | '>') || c.is_control())
        || !source.contains(":target")
    {
        return None;
    }
    let id_selector = format!("[id=\"{fragment}\"]");
    let mut out = String::with_capacity(source.len() + 64);
    let mut rest = source;
    let mut changed = false;
    while let Some(open) = rest.find("<style") {
        let Some(body_start) = rest[open..].find('>').map(|i| open + i + 1) else {
            break;
        };
        let Some(close) = rest[body_start..].find("</style").map(|i| body_start + i) else {
            break;
        };
        out.push_str(&rest[..body_start]);
        let body = &rest[body_start..close];
        if body.contains(":target") {
            out.push_str(&body.replace(":target", &id_selector));
            changed = true;
        } else {
            out.push_str(body);
        }
        rest = &rest[close..];
    }
    out.push_str(rest);
    changed.then_some(out)
}

pub fn parse_svg_image(source: &[u8]) -> Result<crate::node::SvgImageData, usvg::Error> {
    // Cheap to hand back: SvgImageData is an Arc<Tree> plus three scalars.
    let cache_key: Option<&str> = std::str::from_utf8(source).ok();
    if let Some(key) = cache_key {
        if let Ok(cache) = SVG_TREE_CACHE.read() {
            if let Some(hit) = cache.get(key) {
                return Ok(hit.clone());
            }
        }
    }

    // Read before the fonts are: a registration landing mid-parse invalidates
    // what comes out of it, and wasm's single thread is not the only embedder.
    let generation = SVG_TREE_CACHE_GEN.load(std::sync::atomic::Ordering::SeqCst);
    let parsed = parse_svg_image_uncached(source)?;

    if let Some(key) = cache_key {
        if let Ok(mut cache) = SVG_TREE_CACHE.write() {
            if generation == SVG_TREE_CACHE_GEN.load(std::sync::atomic::Ordering::SeqCst) {
                if cache.len() >= SVG_TREE_CACHE_CAP {
                    cache.clear();
                }
                cache.insert(key.into(), parsed.clone());
            }
        }
    }

    Ok(parsed)
}

fn parse_svg_image_uncached(source: &[u8]) -> Result<crate::node::SvgImageData, usvg::Error> {
    let mut options = usvg::Options {
        fontdb: match FONT_DB.read() {
            Ok(db) => Arc::clone(&*db),
            Err(_) => Arc::new(fontdb::Database::new()),
        },
        ..Default::default()
    };
    if let Some(family) = SVG_DEFAULT_FAMILY.read().ok().and_then(|f| f.clone()) {
        options.font_family = family;
    }

    crate::node::SvgImageData::from_data(source, &options)
}

pub trait ToColorColor {
    /// Converts a color into the `AlphaColor<Srgb>` type from the `color` crate
    fn as_color_color(&self) -> Color;
}
impl ToColorColor for AbsoluteColor {
    fn as_color_color(&self) -> Color {
        Color::new(
            *self
                .to_color_space(style::color::ColorSpace::Srgb)
                .raw_components(),
        )
    }
}

#[cfg(all(test, feature = "svg"))]
mod svg_tests {
    use super::parse_svg_image;

    #[test]
    fn svg_without_xmlns_parses() {
        let src = br#"<svg viewBox="0 0 24 24"><path d="M0 0h24v24H0z"/></svg>"#;
        let svg = parse_svg_image(src).unwrap();
        assert!(!svg.tree.root().children().is_empty());
    }

    #[test]
    fn missing_height_is_computed_from_width_and_viewbox_ratio() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" width="200"><rect width="100%" height="100%" fill="green"/></svg>"#;
        let svg = parse_svg_image(src).unwrap();
        assert_eq!(svg.intrinsic_width(), Some(200.0));
        assert_eq!(svg.intrinsic_height(), None);
        assert_eq!(svg.viewbox_aspect_ratio(), Some(1.0));
        assert_eq!(svg.tree.size().width(), 200.0);
        assert_eq!(svg.intrinsic_size(), (200.0, 200.0));
    }

    #[test]
    fn viewbox_only_has_no_intrinsic_dimensions() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 485 58"></svg>"#;
        let svg = parse_svg_image(src).unwrap();
        assert_eq!(svg.intrinsic_width(), None);
        assert_eq!(svg.intrinsic_height(), None);
        // The aspect ratio is still available from the viewBox.
        assert!((svg.aspect_ratio() - (485.0 / 58.0)).abs() < 1e-3);
    }

    #[test]
    fn absolute_dimensions_are_intrinsic() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="16" viewBox="0 0 48 32"></svg>"#;
        let svg = parse_svg_image(src).unwrap();
        assert_eq!(svg.intrinsic_width(), Some(24.0));
        assert_eq!(svg.intrinsic_height(), Some(16.0));
    }

    #[test]
    fn root_style_dimensions_override_attributes() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10" style="width: 400px; height: 50px" viewBox="0 0 200 100"></svg>"#;
        let svg = parse_svg_image(src).unwrap();
        assert_eq!(svg.intrinsic_width(), Some(400.0));
        assert_eq!(svg.intrinsic_height(), Some(50.0));
        assert_eq!(svg.tree.size().width(), 400.0);
        assert_eq!(svg.tree.size().height(), 50.0);
    }

    #[test]
    fn percentage_dimensions_are_not_intrinsic() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" height="50%" viewBox="0 0 200 100"></svg>"#;
        let svg = parse_svg_image(src).unwrap();
        assert_eq!(svg.intrinsic_width(), None);
        assert_eq!(svg.intrinsic_height(), None);
    }

    #[test]
    fn unit_lengths_are_intrinsic() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24px" height="1.5em" viewBox="0 0 48 32"></svg>"#;
        let svg = parse_svg_image(src).unwrap();
        assert!(svg.intrinsic_width().is_some());
        assert!(svg.intrinsic_height().is_some());
    }

    #[test]
    fn non_numeric_dimensions_are_not_intrinsic() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" width="auto" height="foo" viewBox="0 0 200 100"></svg>"#;
        let svg = parse_svg_image(src).unwrap();
        assert_eq!(svg.intrinsic_width(), None);
        assert_eq!(svg.intrinsic_height(), None);
    }

    #[test]
    fn zero_dimension_survives_usvg_invalid_size() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="20"></svg>"#;
        let svg = parse_svg_image(src).unwrap();
        assert_eq!(svg.intrinsic_width(), Some(0.0));
        assert_eq!(svg.intrinsic_height(), Some(20.0));
    }

    #[test]
    fn negative_dimension_is_not_intrinsic() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" width="-5" height="20"></svg>"#;
        let svg = parse_svg_image(src).unwrap();
        assert_eq!(svg.intrinsic_width(), None);
        assert_eq!(svg.intrinsic_height(), Some(20.0));
    }
}

/// Creates an markup5ever::QualName.
/// Given a local name and an optional namespace
#[macro_export]
macro_rules! qual_name {
    ($local:tt $(, $ns:ident)?) => {
        $crate::QualName {
            prefix: None,
            ns: $crate::ns!($($ns)?),
            local: $crate::local_name!($local),
        }
    };
}
