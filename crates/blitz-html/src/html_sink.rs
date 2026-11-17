//! An implementation for Html5ever's sink trait, allowing us to parse HTML into a DOM.

use html5ever::ParseOpts;
use html5ever::tokenizer::TokenizerOpts;
use html5ever::tree_builder::TreeBuilderOpts;
use std::borrow::Cow;
use std::cell::{Cell, Ref, RefCell, RefMut};

use blitz_dom::node::Attribute;
use blitz_dom::{DocumentMutator, HtmlParserProvider, NodeId};
use html5ever::{
    QualName,
    tendril::{StrTendril, TendrilSink},
    tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink},
};

/// Convert an html5ever Attribute which uses tendril for its value to a blitz Attribute
/// which uses String.
fn html5ever_to_blitz_attr(attr: html5ever::Attribute) -> Attribute {
    Attribute {
        name: attr.name,
        value: attr.value.to_string(),
    }
}

#[derive(Copy, Clone, Default, Debug)]
pub struct HtmlProvider;

impl HtmlParserProvider for HtmlProvider {
    fn parse_inner_html<'m2, 'doc2>(
        &self,
        mutr: &'m2 mut DocumentMutator<'doc2>,
        element_id: NodeId,
        html: &str,
    ) {
        DocumentHtmlParser::parse_inner_html_into_mutator(mutr, element_id, html);
    }

    fn parse_document(
        &self,
        html: &str,
        config: blitz_dom::DocumentConfig,
    ) -> Box<dyn blitz_dom::Document> {
        Box::new(crate::HtmlDocument::from_html(html, config))
    }
}

pub struct DocumentHtmlParser<'m, 'doc> {
    document_mutator: RefCell<&'m mut DocumentMutator<'doc>>,

    /// Errors that occurred during parsing.
    pub errors: RefCell<Vec<Cow<'static, str>>>,

    /// The document's quirks mode.
    pub quirks_mode: Cell<QuirksMode>,
    pub is_xml: bool,
}

impl<'m, 'doc> DocumentHtmlParser<'m, 'doc> {
    #[track_caller]
    /// Get a mutable borrow of the DocumentMutator
    fn mutr(&self) -> RefMut<'_, &'m mut DocumentMutator<'doc>> {
        self.document_mutator.borrow_mut()
    }
}

const XHTML_NAMESPACE: &[u8] = b"http://www.w3.org/1999/xhtml";

fn contains_xhtml_marker(source: &[u8]) -> bool {
    source
        .windows(5)
        .any(|window| window == b"XHTML" || window == b"xhtml")
}

fn doctype_info(source: &[u8]) -> Option<(usize, bool)> {
    const DOCTYPE: &[u8] = b"<!DOCTYPE";
    if !source.starts_with(DOCTYPE) {
        return None;
    }
    let mut i = DOCTYPE.len();
    let mut segment_start = i;
    let mut subset_depth = 0usize;
    let mut quote = None;
    let mut mentions_xhtml = false;

    while i < source.len() {
        if let Some(quote_char) = quote {
            if source[i] == quote_char {
                quote = None;
            }
            i += 1;
            continue;
        }
        // Comment text is not evidence that the doctype identifies XHTML.
        if source[i..].starts_with(b"<!--") {
            mentions_xhtml |= contains_xhtml_marker(&source[segment_start..i]);
            i = skip_past(source, i + 4, b"-->")?;
            segment_start = i;
            continue;
        }

        match source[i] {
            b'"' | b'\'' => quote = Some(source[i]),
            // `>` inside an internal subset does not end the doctype.
            b'[' => subset_depth += 1,
            b']' => subset_depth = subset_depth.saturating_sub(1),
            b'>' if subset_depth == 0 => {
                mentions_xhtml |= contains_xhtml_marker(&source[segment_start..i]);
                return Some((i + 1, mentions_xhtml));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn is_xml_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

fn skip_xml_whitespace(bytes: &[u8], mut i: usize) -> usize {
    while bytes.get(i).is_some_and(|byte| is_xml_whitespace(*byte)) {
        i += 1;
    }
    i
}

/// The index just past the first `terminator` at or after `from`.
fn skip_past(bytes: &[u8], from: usize, terminator: &[u8]) -> Option<usize> {
    let end = bytes
        .get(from..)?
        .windows(terminator.len())
        .position(|window| window == terminator)?;
    Some(from + end + terminator.len())
}

fn find_html_root_tag(source: &str, start: usize) -> Option<&str> {
    let bytes = source.as_bytes();
    let tag_start = skip_prolog_misc(source, start)?;
    if bytes.get(tag_start) != Some(&b'<') {
        return None;
    }

    let name_start = tag_start + 1;
    let mut name_end = name_start;
    while bytes
        .get(name_end)
        .is_some_and(|byte| !is_xml_whitespace(*byte) && !matches!(byte, b'/' | b'>'))
    {
        name_end += 1;
    }
    if !bytes[name_start..name_end].eq_ignore_ascii_case(b"html") {
        return None;
    }

    let mut quote = None;
    for (i, &byte) in bytes.iter().enumerate().skip(name_end) {
        if let Some(quote_char) = quote {
            if byte == quote_char {
                quote = None;
            }
        } else {
            match byte {
                b'"' | b'\'' => quote = Some(byte),
                b'>' => return Some(&source[tag_start..i + 1]),
                _ => {}
            }
        }
    }
    None
}

fn html_root_tag_has_xhtml_namespace(tag: &str) -> bool {
    let bytes = tag.as_bytes();
    let mut i = 5;

    while i < bytes.len() {
        i = skip_xml_whitespace(bytes, i);
        if matches!(bytes.get(i), Some(b'>' | b'/')) {
            i += 1;
            continue;
        }

        let name_start = i;
        while bytes
            .get(i)
            .is_some_and(|byte| !is_xml_whitespace(*byte) && !matches!(byte, b'=' | b'/' | b'>'))
        {
            i += 1;
        }
        if name_start == i {
            return false;
        }
        let name = &bytes[name_start..i];

        i = skip_xml_whitespace(bytes, i);
        let mut value = None;
        if bytes.get(i) == Some(&b'=') {
            i += 1;
            i = skip_xml_whitespace(bytes, i);
            let Some(&quote @ (b'"' | b'\'')) = bytes.get(i) else {
                return false;
            };
            i += 1;
            let value_start = i;
            while bytes.get(i).is_some_and(|byte| *byte != quote) {
                i += 1;
            }
            if bytes.get(i) != Some(&quote) {
                return false;
            }
            value = Some(&bytes[value_start..i]);
            i += 1;
        }

        if name == b"xmlns" && value == Some(XHTML_NAMESPACE) {
            return true;
        }
    }
    false
}

fn root_element_has_xhtml_namespace(source: &str, start: usize) -> bool {
    find_html_root_tag(source, start).is_some_and(html_root_tag_has_xhtml_namespace)
}

fn skip_prolog_misc(source: &str, mut i: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    loop {
        i = skip_xml_whitespace(bytes, i);
        if source.get(i..)?.starts_with("<!--") {
            i = skip_past(bytes, i + 4, b"-->")?;
        } else if source.get(i..)?.starts_with("<?") {
            i = skip_past(bytes, i + 2, b"?>")?;
        } else {
            return Some(i);
        }
    }
}

/// Returns whether the source should use the XML parser based on its XML declaration,
/// doctype text, or the default namespace on its root `<html>` tag.
pub fn is_xhtml_document(html: &str) -> bool {
    let html = html.strip_prefix('\u{feff}').unwrap_or(html);
    let is_xml_declaration = html
        .strip_prefix("<?xml")
        .and_then(|rest| rest.as_bytes().first())
        .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'?');
    if is_xml_declaration {
        return true;
    }

    let Some(prolog_start) = skip_prolog_misc(html, 0) else {
        return false;
    };
    if html[prolog_start..].starts_with("<!DOCTYPE") {
        let Some((doctype_end, mentions_xhtml)) = doctype_info(&html.as_bytes()[prolog_start..])
        else {
            return false;
        };
        // Text/html pages often put `<html xmlns=...>` on the line after
        // `<!DOCTYPE html>`; only a root on the doctype's line routes to XML.
        let root_search_start = prolog_start + doctype_end;
        let root_on_doctype_line = skip_prolog_misc(html, root_search_start)
            .is_some_and(|root_start| !html[root_search_start..root_start].contains('\n'));
        return mentions_xhtml
            || (root_on_doctype_line
                && root_element_has_xhtml_namespace(html, root_search_start));
    }

    root_element_has_xhtml_namespace(html, prolog_start)
}

impl<'m, 'doc> DocumentHtmlParser<'m, 'doc> {
    pub fn new(mutr: &'m mut DocumentMutator<'doc>) -> DocumentHtmlParser<'m, 'doc> {
        DocumentHtmlParser {
            document_mutator: RefCell::new(mutr),
            errors: RefCell::new(Vec::new()),
            quirks_mode: Cell::new(QuirksMode::NoQuirks),
            is_xml: false,
        }
    }

    pub fn parse_into_mutator<'a, 'd>(mutr: &'a mut DocumentMutator<'d>, html: &str) {
        if is_xhtml_document(html) {
            Self::parse_xml_into_mutator(mutr, html);
        } else {
            // Parse as HTML
            let mut sink = DocumentHtmlParser::new(mutr);
            sink.is_xml = false;
            let opts = ParseOpts {
                tokenizer: TokenizerOpts::default(),
                tree_builder: TreeBuilderOpts {
                    exact_errors: false,
                    scripting_enabled: false, // Enables parsing of <noscript> tags
                    iframe_srcdoc: false,
                    drop_doctype: true,
                    quirks_mode: QuirksMode::NoQuirks,
                },
            };
            html5ever::parse_document(sink, opts)
                .from_utf8()
                .read_from(&mut html.as_bytes())
                .unwrap();
        }
    }

    /// Parse the input as XML (XHTML), regardless of its content.
    ///
    /// [`parse_into_mutator`](Self::parse_into_mutator) sniffs the content to decide between HTML
    /// and XML parsing, but the sniffing cannot detect all XHTML documents (e.g. ones with an
    /// `<!DOCTYPE html>` doctype). Callers which know the document is XHTML from out-of-band
    /// information (a `Content-Type` header or an `.xht`/`.xhtml` file extension) should use
    /// this method instead.
    pub fn parse_xml_into_mutator<'a, 'd>(mutr: &'a mut DocumentMutator<'d>, xml: &str) {
        let mut sink = DocumentHtmlParser::new(mutr);
        sink.is_xml = true;
        xml5ever::driver::parse_document(sink, Default::default())
            .from_utf8()
            .read_from(&mut xml.as_bytes())
            .unwrap();
    }

    pub fn parse_inner_html_into_mutator<'a, 'd>(
        mutr: &'a mut DocumentMutator<'d>,
        element_id: NodeId,
        html: &str,
    ) {
        let sink = DocumentHtmlParser::new(mutr);

        let opts = ParseOpts {
            tokenizer: TokenizerOpts::default(),
            tree_builder: TreeBuilderOpts {
                exact_errors: false,
                scripting_enabled: false, // Enables parsing of <noscript> tags
                iframe_srcdoc: false,
                drop_doctype: true,
                quirks_mode: QuirksMode::NoQuirks,
            },
        };
        html5ever::driver::parse_fragment_for_element(sink, opts, element_id, false, None)
            .from_utf8()
            .read_from(&mut html.as_bytes())
            .unwrap();

        // html5ever creates a new fragment root node under the document node and parses the nodes into that fragment root.
        // So here we move the children of the fragment root to element_id and then drop the fragment root.
        let document_id = mutr.doc.root_node().id;
        let fragment_root_id = mutr.last_child_id(document_id).unwrap();
        let child_ids = mutr.child_ids(fragment_root_id);
        mutr.append_children(element_id, &child_ids);
        mutr.remove_and_drop_node(fragment_root_id);
    }
}

impl<'m, 'doc> TreeSink for DocumentHtmlParser<'m, 'doc> {
    type Output = ();

    // we use the ID of the nodes in the tree as the handle
    type Handle = NodeId;

    type ElemName<'a>
        = Ref<'a, QualName>
    where
        Self: 'a;

    fn finish(self) -> Self::Output {
        #[cfg(feature = "tracing")]
        for error in self.errors.borrow().iter() {
            tracing::error!("{error}");
        }
    }

    fn parse_error(&self, msg: Cow<'static, str>) {
        self.errors.borrow_mut().push(msg);
    }

    fn get_document(&self) -> Self::Handle {
        self.document_mutator.borrow().doc.root_node().id
    }

    fn elem_name<'a>(&'a self, target: &'a Self::Handle) -> Self::ElemName<'a> {
        Ref::map(self.document_mutator.borrow(), |docm| {
            docm.element_name(*target)
                .expect("TreeSink::elem_name called on a node which is not an element!")
        })
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<html5ever::Attribute>,
        _flags: ElementFlags,
    ) -> Self::Handle {
        let attrs = attrs.into_iter().map(html5ever_to_blitz_attr).collect();
        self.mutr().create_element(name, attrs)
    }

    fn create_comment(&self, text: StrTendril) -> Self::Handle {
        self.mutr().create_comment_node(&text)
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> Self::Handle {
        self.mutr().create_comment_node("")
    }

    fn append(&self, parent_id: &Self::Handle, child: NodeOrText<Self::Handle>) {
        match child {
            NodeOrText::AppendNode(id) => self.mutr().append_children(*parent_id, &[id]),
            // If content to append is text, first attempt to append it to the last child of parent.
            // Else create a new text node and append it to the parent
            NodeOrText::AppendText(text) => {
                let last_child_id = self.mutr().last_child_id(*parent_id);
                let has_appended = if let Some(id) = last_child_id {
                    self.mutr().append_text_to_node(id, &text).is_ok()
                } else {
                    false
                };
                if !has_appended {
                    let new_child_id = self.mutr().create_text_node(&text);
                    self.mutr().append_children(*parent_id, &[new_child_id]);
                }
            }
        }
    }

    // Note: The tree builder promises we won't have a text node after the insertion point.
    // https://github.com/servo/html5ever/blob/main/rcdom/lib.rs#L338
    fn append_before_sibling(&self, sibling_id: &Self::Handle, new_node: NodeOrText<Self::Handle>) {
        match new_node {
            NodeOrText::AppendNode(id) => self.mutr().insert_nodes_before(*sibling_id, &[id]),
            // If content to append is text, first attempt to append it to the node before sibling_node
            // Else create a new text node and insert it before sibling_node
            NodeOrText::AppendText(text) => {
                let previous_sibling_id = self.mutr().previous_sibling_id(*sibling_id);
                let has_appended = if let Some(id) = previous_sibling_id {
                    self.mutr().append_text_to_node(id, &text).is_ok()
                } else {
                    false
                };
                if !has_appended {
                    let new_child_id = self.mutr().create_text_node(&text);
                    self.mutr()
                        .insert_nodes_before(*sibling_id, &[new_child_id]);
                }
            }
        };
    }

    fn append_based_on_parent_node(
        &self,
        element: &Self::Handle,
        prev_element: &Self::Handle,
        child: NodeOrText<Self::Handle>,
    ) {
        if self.mutr().node_has_parent(*element) {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        _name: StrTendril,
        _public_id: StrTendril,
        _system_id: StrTendril,
    ) {
        // Ignore. We don't care about the DOCTYPE for now.
    }

    fn get_template_contents(&self, target: &Self::Handle) -> Self::Handle {
        // Parse a template's children inline, under the element,
        // as blitz 0.3.0-beta.2 did. Kitesurf's DOM shim owns template
        // semantics: it moves these children into the JS `.content` fragment
        // on first access. Upstream's detached contents node is invisible to
        // the shim, and would leave every parsed template (markup, innerHTML,
        // declarative shadow roots) with an empty `.content`.
        *target
    }

    fn same_node(&self, x: &Self::Handle, y: &Self::Handle) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.quirks_mode.set(mode);
    }

    fn add_attrs_if_missing(&self, target: &Self::Handle, attrs: Vec<html5ever::Attribute>) {
        let attrs = attrs.into_iter().map(html5ever_to_blitz_attr).collect();
        self.mutr().add_attrs_if_missing(*target, attrs);
    }

    fn remove_from_parent(&self, target: &Self::Handle) {
        self.mutr().remove_node(*target);
    }

    fn reparent_children(&self, old_parent_id: &Self::Handle, new_parent_id: &Self::Handle) {
        self.mutr()
            .reparent_children(*old_parent_id, *new_parent_id);
    }
}

#[test]
fn parses_some_html() {
    use blitz_dom::{BaseDocument, DocumentConfig};

    let html = "<!DOCTYPE html><html><body><h1>hello world</h1></body></html>";
    let mut doc = BaseDocument::new(DocumentConfig::default());
    let mut mutr = doc.mutate();
    let sink = DocumentHtmlParser::new(&mut mutr);

    html5ever::parse_document(sink, Default::default())
        .from_utf8()
        .read_from(&mut html.as_bytes())
        .unwrap();

    drop(mutr);
    doc.print_tree()

    // Now our tree should have some nodes in it
}

#[test]
fn xhtml_sniff_uses_only_the_doctype_and_root_namespace() {
    const XHTML_NAMESPACE: &str = "http://www.w3.org/1999/xhtml";

    for html in [
        "<!DOCTYPE html><!-- XHTML --><html></html>",
        "<!DOCTYPE html [<!-- XHTML -->]><html></html>",
        "<!DOCTYPE html><htmlish xmlns=\"http://www.w3.org/1999/xhtml\"></htmlish>",
        "<!DOCTYPE html><html data-note=\"xhtml\"></html>",
        "<!DOCTYPE html><html>text says xhtml<head></head></html>",
        "<!-- XHTML --><html></html>",
        "<htmlish xmlns=\"http://www.w3.org/1999/xhtml\"></htmlish>",
        "<html data-note=\"xhtml\"></html>",
        "<!DOCTYPE html><html\u{000c}xmlns=\"http://www.w3.org/1999/xhtml\"></html>",
        "<!DOCTYPE html>\n<html xmlns=\"http://www.w3.org/1999/xhtml\"><p>&nbsp;</p></html>",
        "<!DOCTYPE html>\r\n<!-- root follows --><html xmlns='http://www.w3.org/1999/xhtml'></html>",
    ] {
        assert!(!is_xhtml_document(html), "misrouted as XML: {html}");
    }
    assert!(!is_xhtml_document(
        "<?xml-stylesheet href=\"theme.css\"?><html></html>"
    ));

    assert!(is_xhtml_document(
        "<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Strict//EN\"><html></html>"
    ));
    assert!(is_xhtml_document(
        "<!DOCTYPE html [<!ENTITY marker \"xhtml\">]><html></html>"
    ));
    assert!(is_xhtml_document(&format!(
        "<!DOCTYPE html><!-- XHTML --><html xmlns='{XHTML_NAMESPACE}'></html>"
    )));
    assert!(is_xhtml_document(&format!(
        "<!DOCTYPE html><!-- root follows --><html data-x=\"<b>\" xmlns=\"{XHTML_NAMESPACE}\"></html>"
    )));
    assert!(is_xhtml_document(&format!(
        "<!-- root follows --><html xmlns='{XHTML_NAMESPACE}'></html>"
    )));
    assert!(is_xhtml_document(
        "\u{feff}<?xml version=\"1.0\"?><html></html>"
    ));
    assert!(is_xhtml_document(
        "\u{feff}<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Strict//EN\" \
         \"http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd\"><html></html>"
    ));
}
