use std::ops::{Deref, DerefMut};

use markup5ever::{LocalName, Namespace, Prefix, QualName, ns};
use thin_vec::ThinVec;

/// The local part of an attribute's name; see [`Attribute::namespace`] and
/// [`Attribute::prefix`] for the rest.
#[derive(PartialEq, Eq, Clone, Debug, Hash)]
pub struct AttrName {
    pub local: LocalName,
}

/// A tag attribute, e.g. `class="test"` in `<div class="test" ...>`.
///
/// Nearly every attribute is unprefixed in the empty namespace, so the prefix and namespace
/// are boxed apart and an attribute is three words rather than five.
#[derive(PartialEq, Eq, Clone)]
pub struct Attribute {
    /// The name of the attribute (e.g. the `class` in `<div class="test">`)
    pub name: AttrName,
    /// The value of the attribute (e.g. the `"test"` in `<div class="test">`). Inline up to
    /// 12 bytes on wasm32.
    pub value: compact_str::CompactString,
    /// The prefix and namespace of an attribute that has either.
    ext: Option<Box<(Option<Prefix>, Namespace)>>,
}

static NS_EMPTY: Namespace = ns!();

impl Attribute {
    pub fn new(name: QualName, value: impl Into<compact_str::CompactString>) -> Self {
        let QualName { prefix, ns, local } = name;
        let ext = (prefix.is_some() || ns != ns!()).then(|| Box::new((prefix, ns)));
        Self {
            name: AttrName { local },
            value: value.into(),
            ext,
        }
    }

    /// The namespace of the attribute's name.
    #[inline]
    pub fn namespace(&self) -> &Namespace {
        self.ext.as_ref().map_or(&NS_EMPTY, |ext| &ext.1)
    }

    /// The prefix of the attribute's name, if any.
    #[inline]
    pub fn prefix(&self) -> Option<&Prefix> {
        self.ext.as_ref()?.0.as_ref()
    }

    /// The attribute's full name, including its prefix.
    pub fn qual_name(&self) -> QualName {
        QualName::new(
            self.prefix().cloned(),
            self.namespace().clone(),
            self.name.local.clone(),
        )
    }

    /// Whether the attribute's full name is `name`.
    #[inline]
    pub fn has_name(&self, name: &QualName) -> bool {
        self.name.local == name.local
            && *self.namespace() == name.ns
            && self.prefix() == name.prefix.as_ref()
    }
}

impl std::fmt::Debug for Attribute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attribute")
            .field("name", &self.qual_name())
            .field("value", &self.value)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct Attributes {
    inner: ThinVec<Attribute>,
}

impl Attributes {
    pub fn new(inner: Vec<Attribute>) -> Self {
        Self {
            inner: inner.into_iter().collect(),
        }
    }
}

impl From<Vec<Attribute>> for Attributes {
    fn from(inner: Vec<Attribute>) -> Self {
        Self::new(inner)
    }
}

/// Collects straight into the list, without a `Vec` to copy from first.
impl FromIterator<Attribute> for Attributes {
    fn from_iter<I: IntoIterator<Item = Attribute>>(iter: I) -> Self {
        Self {
            inner: iter.into_iter().collect(),
        }
    }
}

impl Attributes {

    pub fn get(&mut self, name: &QualName) -> Option<&Attribute> {
        self.inner.iter().find(|attr| attr.has_name(name))
    }

    pub fn set(&mut self, name: QualName, value: &str) {
        let existing_attr = self.inner.iter_mut().find(|a| a.has_name(&name));
        if let Some(existing_attr) = existing_attr {
            existing_attr.value = value.into();
        } else {
            self.push(Attribute::new(name, value));
        }
    }

    pub fn remove(&mut self, name: &QualName) -> Option<Attribute> {
        let idx = self.inner.iter().position(|attr| attr.has_name(name));
        idx.map(|idx| self.inner.remove(idx))
    }
}

impl Deref for Attributes {
    type Target = ThinVec<Attribute>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl DerefMut for Attributes {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::{Attribute, Attributes};
    use markup5ever::{QualName, local_name, ns};

    #[test]
    fn short_values_stay_inline_across_rewrites() {
        let name = QualName::new(None, ns!(), local_name!("class"));
        let mut attrs = Attributes::new(Vec::new());
        attrs.set(name.clone(), "a long value past twelve bytes");
        assert!(attrs.get(&name).unwrap().value.is_heap_allocated());
        attrs.set(name.clone(), "short");
        let value = &attrs.get(&name).unwrap().value;
        assert_eq!(value.as_str(), "short");
        assert!(!value.is_heap_allocated(), "a rewrite to a short value frees the heap copy");
        let text = crate::node::TextNodeData::new("\n  ");
        assert!(!text.content.is_heap_allocated());
    }

    #[test]
    fn attribute_names_round_trip_and_match_exactly() {
        let names = [
            QualName::new(None, ns!(), local_name!("href")),
            QualName::new(None, ns!(html), local_name!("checked")),
            QualName::new(Some("xlink".into()), ns!(xlink), local_name!("href")),
            QualName::new(None, ns!(xml), local_name!("lang")),
            QualName::new(None, ns!(xlink), local_name!("href")),
        ];
        let mut attrs = Attributes::new(Vec::new());
        for (i, name) in names.iter().enumerate() {
            let attr = Attribute::new(name.clone(), i.to_string());
            assert_eq!(attr.qual_name(), *name);
            assert_eq!(attr.clone(), attr);
            attrs.set(name.clone(), &i.to_string());
        }
        // Names that share a local name but differ in namespace or prefix stay distinct.
        assert_eq!(attrs.len(), names.len());
        for (i, name) in names.iter().enumerate() {
            assert_eq!(attrs.get(name).map(|a| a.value.as_str()), Some(&*i.to_string()));
        }
        assert_eq!(attrs.remove(&names[2]).map(|a| a.qual_name()), Some(names[2].clone()));
        assert!(attrs.get(&names[2]).is_none() && attrs.get(&names[0]).is_some());
    }
}
