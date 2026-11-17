//! This module implements the binding scope for various AST nodes.
//!
//! Scopes are used to track the bindings of identifiers in the AST.

use bitflags::bitflags;
use boa_string::JsString;
use std::{
    cell::{Cell, RefCell},
    fmt::Debug,
    rc::Rc,
};

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct BindingFlags: u8 {
        const MUTABLE  = 1 << 0;
        const LEX      = 1 << 1;
        const STRICT   = 1 << 2;
        const ESCAPES  = 1 << 3;
        const ACCESSED = 1 << 4;
    }
}

impl BindingFlags {
    fn is_mutable(self) -> bool {
        self.contains(BindingFlags::MUTABLE)
    }
    fn is_lex(self) -> bool {
        self.contains(BindingFlags::LEX)
    }
    fn is_strict(self) -> bool {
        self.contains(BindingFlags::STRICT)
    }
    fn escapes(self) -> bool {
        self.contains(BindingFlags::ESCAPES)
    }
    fn is_accessed(self) -> bool {
        self.contains(BindingFlags::ACCESSED)
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Binding {
    name: JsString,
    index: u32,
    flags: BindingFlags,
}

impl Binding {
    fn is_mutable(&self) -> bool {
        self.flags.is_mutable()
    }
    fn is_lex(&self) -> bool {
        self.flags.is_lex()
    }
    fn is_strict(&self) -> bool {
        self.flags.is_strict()
    }
    fn escapes(&self) -> bool {
        self.flags.escapes()
    }
    fn is_accessed(&self) -> bool {
        self.flags.is_accessed()
    }
}

/// A scope maps bound identifiers to their binding positions.
///
/// It can be either a global scope or a function scope or a declarative scope.
#[derive(Clone, PartialEq)]
pub struct Scope {
    inner: Rc<Inner>,
}

impl Debug for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scope")
            .field("outer", &self.inner.outer)
            .field("index", &self.inner.index)
            .field("bindings", &self.inner.bindings)
            .field("function", &self.inner.function)
            .finish()
    }
}

impl Default for Scope {
    fn default() -> Self {
        Self::new_global()
    }
}

#[cfg(feature = "arbitrary")]
impl<'a> arbitrary::Arbitrary<'a> for Scope {
    fn arbitrary(_u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(Self::new_global())
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct Inner {
    unique_id: u32,
    outer: Option<Scope>,
    index: Cell<u32>,
    bindings: RefCell<Vec<Binding>>,
    // Maps a name to its position in `bindings`. Minified bundles keep thousands of
    // bindings in one flat function scope, where a linear scan is quadratic.
    lookup: RefCell<rustc_hash::FxHashMap<JsString, usize>>,
    // Kitesurf: set once the compiler is done with the scope and dropped `lookup`; the next
    // query (a direct eval) rebuilds it. Compiled code resolves names by index, not here.
    lookup_dropped: Cell<bool>,
    function: bool,
    // Has the `this` been accessed/escaped outside the function environment boundary.
    this_escaped: Cell<bool>,

    context: Rc<ScopeContext>,
}

impl Inner {
    /// Position of `name` in `bindings`, or `usize::MAX` (which `Vec::get` maps to
    /// `None`). Bindings are append-only, so a position never changes once taken.
    #[inline]
    fn pos(&self, name: &JsString) -> usize {
        self.ensure_lookup();
        self.lookup
            .borrow()
            .get(name)
            .copied()
            .unwrap_or(usize::MAX)
    }

    // Kitesurf: rebuilds a dropped `lookup`. Callers that borrow `bindings` mutably must run
    // this first; `pos` runs it for the rest, beside their shared borrow.
    #[inline]
    fn ensure_lookup(&self) {
        if !self.lookup_dropped.get() {
            return;
        }
        let bindings = self.bindings.borrow();
        let mut lookup = self.lookup.borrow_mut();
        lookup.reserve(bindings.len());
        for (i, binding) in bindings.iter().enumerate() {
            lookup.insert(binding.name.clone(), i);
        }
        self.lookup_dropped.set(false);
    }
}

impl Scope {
    /// Creates a new global scope.
    #[must_use]
    pub fn new_global() -> Self {
        Self {
            inner: Rc::new(Inner {
                unique_id: 0,
                outer: None,
                index: Cell::default(),
                bindings: RefCell::default(),
                lookup: RefCell::default(),
                lookup_dropped: Cell::new(false),
                function: true,
                this_escaped: Cell::new(false),
                context: Rc::default(),
            }),
        }
    }

    /// Creates a new scope.
    #[must_use]
    pub fn new(parent: Self, function: bool) -> Self {
        Self {
            inner: Rc::new(Inner {
                unique_id: parent.inner.context.next_unique_id(),
                index: Cell::new(parent.inner.index.get() + 1),
                bindings: RefCell::default(),
                lookup: RefCell::default(),
                lookup_dropped: Cell::new(false),
                function,
                this_escaped: Cell::new(false),
                context: parent.inner.context.clone(),
                outer: Some(parent),
            }),
        }
    }

    /// Checks if the scope has only local bindings.
    #[must_use]
    pub fn all_bindings_local(&self) -> bool {
        // if self.inner.function && self.inn
        self.inner
            .bindings
            .borrow()
            .iter()
            .all(|binding| !binding.escapes())
    }

    /// Marks all bindings in this scope as escaping.
    pub fn escape_all_bindings(&self) {
        for binding in self.inner.bindings.borrow_mut().iter_mut() {
            binding.flags.insert(BindingFlags::ESCAPES);
        }
    }

    /// Has this binding escaped.
    #[must_use]
    pub fn escaped_this(&self) -> bool {
        self.inner.this_escaped.get()
    }

    /// Check if the scope has a lexical binding with the given name.
    #[must_use]
    pub fn has_lex_binding(&self, name: &JsString) -> bool {
        self.inner
            .bindings
            .borrow()
            .get(self.inner.pos(&name))
            .is_some_and(Binding::is_lex)
    }

    /// Check if the scope has a binding with the given name.
    #[must_use]
    pub fn has_binding(&self, name: &JsString) -> bool {
        self.inner.pos(&name) != usize::MAX
    }

    /// Get the binding locator for a binding with the given name.
    /// Fall back to the global scope if the binding is not found.
    #[must_use]
    pub fn get_identifier_reference(&self, name: JsString) -> IdentifierReference {
        if let Some(binding) = self.inner.bindings.borrow().get(self.inner.pos(&name)) {
            IdentifierReference::new(
                BindingLocator::declarative(
                    name,
                    self.inner.index.get(),
                    binding.index,
                    self.inner.unique_id,
                ),
                binding.is_lex(),
                binding.escapes(),
            )
        } else if let Some(outer) = &self.inner.outer {
            outer.get_identifier_reference(name)
        } else {
            IdentifierReference::new(BindingLocator::global(name), false, true)
        }
    }

    /// Returns the number of bindings in this scope.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn num_bindings(&self) -> u32 {
        self.inner.bindings.borrow().len() as u32
    }

    /// Returns the number of bindings in this scope that are not local.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn num_bindings_non_local(&self) -> u32 {
        self.inner
            .bindings
            .borrow()
            .iter()
            .filter(|binding| binding.escapes())
            .count() as u32
    }

    /// Adjust the binding indices to exclude local bindings.
    pub(crate) fn reorder_binding_indices(&self) {
        let mut bindings = self.inner.bindings.borrow_mut();
        let mut index = 0;
        for binding in bindings.iter_mut() {
            if !binding.escapes() {
                binding.index = 0;
                continue;
            }
            binding.index = index;
            index += 1;
        }
    }

    /// Returns the index of this scope.
    #[must_use]
    pub fn scope_index(&self) -> u32 {
        self.inner.index.get()
    }

    /// Set the index of this scope.
    pub(crate) fn set_index(&self, index: u32) {
        self.inner.index.set(index);
    }

    /// Check if the scope is a function scope.
    #[must_use]
    pub fn is_function(&self) -> bool {
        self.inner.function
    }

    /// Check if the scope is a global scope.
    #[must_use]
    pub fn is_global(&self) -> bool {
        self.inner.outer.is_none()
    }

    /// Check if a binding with the given name is mutable.
    ///
    /// Returns `Some(true)` for mutable bindings (`let`, `var`),
    /// `Some(false)` for immutable bindings (`const`),
    /// or `None` if the binding is not found in this or any outer scope.
    #[must_use]
    pub fn is_binding_mutable(&self, name: &JsString) -> Option<bool> {
        if let Some(binding) = self.inner.bindings.borrow().get(self.inner.pos(&name)) {
            Some(binding.is_mutable())
        } else if let Some(outer) = &self.inner.outer {
            outer.is_binding_mutable(name)
        } else {
            None
        }
    }

    /// Get the locator for a binding name.
    #[must_use]
    pub fn get_binding(&self, name: &JsString) -> Option<BindingLocator> {
        self.inner
            .bindings
            .borrow()
            .get(self.inner.pos(&name))
            .map(|binding| {
                BindingLocator::declarative(
                    name.clone(),
                    self.inner.index.get(),
                    binding.index,
                    self.inner.unique_id,
                )
            })
    }

    /// Get the locator for a binding name.
    #[must_use]
    pub fn get_binding_reference(&self, name: &JsString) -> Option<IdentifierReference> {
        self.inner
            .bindings
            .borrow()
            .get(self.inner.pos(&name))
            .map(|binding| {
                IdentifierReference::new(
                    BindingLocator::declarative(
                        name.clone(),
                        self.inner.index.get(),
                        binding.index,
                        self.inner.unique_id,
                    ),
                    binding.is_lex(),
                    binding.escapes(),
                )
            })
    }

    /// Simulate a binding access.
    ///
    /// - If the binding access crosses a function border, the binding is marked as escaping.
    /// - If the binding access is in an eval or with scope, the binding is marked as escaping.
    pub fn access_binding(&self, name: &JsString, eval_or_with: bool) {
        let mut crossed_function_border = false;
        let mut current = self;
        loop {
            current.inner.ensure_lookup();
            if let Some(binding) = current
                .inner
                .bindings
                .borrow_mut()
                .get_mut(current.inner.pos(&name))
            {
                binding.flags.insert(BindingFlags::ACCESSED);
                if crossed_function_border || eval_or_with {
                    binding.flags.insert(BindingFlags::ESCAPES);
                }
                return;
            }
            if let Some(outer) = &current.inner.outer {
                if current.inner.function {
                    crossed_function_border = true;
                }
                current = outer;
            } else {
                return;
            }
        }
    }

    /// Escape enclosing function environment's `this`.
    pub fn escape_this_in_enclosing_function_scope(&self) {
        let mut current = self;
        let mut crossed_function_border = false;

        loop {
            if crossed_function_border && current.is_function() {
                current.inner.this_escaped.set(true);
                return;
            }
            if let Some(outer) = &current.inner.outer {
                if current.is_function() {
                    crossed_function_border = true;
                }
                current = outer;
            } else {
                return;
            }
        }
    }

    /// Creates a mutable binding.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn create_mutable_binding(&self, name: JsString, function_scope: bool) -> BindingLocator {
        self.inner.ensure_lookup();
        let mut bindings = self.inner.bindings.borrow_mut();
        let binding_index = bindings.len() as u32;
        if let Some(binding) = bindings.get(self.inner.pos(&name)) {
            return BindingLocator::declarative(
                name,
                self.inner.index.get(),
                binding.index,
                self.inner.unique_id,
            );
        }
        let mut flags = BindingFlags::MUTABLE;
        flags.set(BindingFlags::LEX, !function_scope);
        flags.set(BindingFlags::ESCAPES, self.is_global());
        self.inner
            .lookup
            .borrow_mut()
            .insert(name.clone(), bindings.len());
        bindings.push(Binding {
            name: name.clone(),
            index: binding_index,
            flags,
        });
        BindingLocator::declarative(
            name,
            self.inner.index.get(),
            binding_index,
            self.inner.unique_id,
        )
    }

    /// Crate an immutable binding.
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn create_immutable_binding(&self, name: JsString, strict: bool) {
        self.inner.ensure_lookup();
        let mut bindings = self.inner.bindings.borrow_mut();
        if self.inner.pos(&name) != usize::MAX {
            return;
        }
        let binding_index = bindings.len() as u32;
        let mut flags = BindingFlags::LEX;
        flags.set(BindingFlags::STRICT, strict);
        flags.set(BindingFlags::ESCAPES, self.is_global());
        self.inner
            .lookup
            .borrow_mut()
            .insert(name.clone(), bindings.len());
        bindings.push(Binding {
            name,
            index: binding_index,
            flags,
        });
    }

    /// Return the binding locator for a mutable binding.
    ///
    /// # Errors
    /// Returns an error if the binding is not mutable or does not exist.
    pub fn set_mutable_binding(
        &self,
        name: JsString,
    ) -> Result<IdentifierReference, BindingLocatorError> {
        Ok(
            match self.inner.bindings.borrow().get(self.inner.pos(&name)) {
                Some(binding) if binding.is_mutable() => IdentifierReference::new(
                    BindingLocator::declarative(
                        name,
                        self.inner.index.get(),
                        binding.index,
                        self.inner.unique_id,
                    ),
                    binding.is_lex(),
                    binding.escapes(),
                ),
                Some(binding) if binding.is_strict() => {
                    return Err(BindingLocatorError::MutateImmutable);
                }
                Some(_) => return Err(BindingLocatorError::Silent),
                None => self.inner.outer.as_ref().map_or_else(
                    || {
                        Ok(IdentifierReference::new(
                            BindingLocator::global(name.clone()),
                            false,
                            true,
                        ))
                    },
                    |outer| outer.set_mutable_binding(name.clone()),
                )?,
            },
        )
    }

    #[cfg(feature = "annex-b")]
    /// Return the binding locator for a set operation on an existing var binding.
    ///
    /// # Errors
    /// Returns an error if the binding is not mutable or does not exist.
    pub fn set_mutable_binding_var(
        &self,
        name: JsString,
    ) -> Result<IdentifierReference, BindingLocatorError> {
        if !self.is_function() {
            return self.inner.outer.as_ref().map_or_else(
                || {
                    Ok(IdentifierReference::new(
                        BindingLocator::global(name.clone()),
                        false,
                        true,
                    ))
                },
                |outer| outer.set_mutable_binding_var(name.clone()),
            );
        }

        Ok(
            match self.inner.bindings.borrow().get(self.inner.pos(&name)) {
                Some(binding) if binding.is_mutable() => IdentifierReference::new(
                    BindingLocator::declarative(
                        name,
                        self.inner.index.get(),
                        binding.index,
                        self.inner.unique_id,
                    ),
                    binding.is_lex(),
                    binding.escapes(),
                ),
                Some(binding) if binding.is_strict() => {
                    return Err(BindingLocatorError::MutateImmutable);
                }
                Some(_) => return Err(BindingLocatorError::Silent),
                None => self.inner.outer.as_ref().map_or_else(
                    || {
                        Ok(IdentifierReference::new(
                            BindingLocator::global(name.clone()),
                            false,
                            true,
                        ))
                    },
                    |outer| outer.set_mutable_binding_var(name.clone()),
                )?,
            },
        )
    }

    /// Gets the outer scope of this scope.
    #[must_use]
    pub fn outer(&self) -> Option<&Self> {
        self.inner.outer.as_ref()
    }

    /// Returns the unique ID of this scope.
    #[must_use]
    pub fn unique_id(&self) -> u32 {
        self.inner.unique_id
    }

    /// Kitesurf: identity of the shared scope, for an embedder that has to tell scopes
    /// apart (several blocks hold the same one).
    #[must_use]
    pub fn ks_ptr(&self) -> usize {
        Rc::as_ptr(&self.inner) as usize
    }

    /// Kitesurf: this scope's own state apart from its outer scope, for an embedder that
    /// moves it out of memory and rebuilds it with [`Self::ks_rebuild`].
    #[must_use]
    pub fn ks_parts(&self) -> KsScopeParts {
        KsScopeParts {
            unique_id: self.inner.unique_id,
            index: self.inner.index.get(),
            function: self.inner.function,
            this_escaped: self.inner.this_escaped.get(),
            bindings: self
                .inner
                .bindings
                .borrow()
                .iter()
                .map(|b| (b.name.clone(), b.index, b.flags.bits()))
                .collect(),
        }
    }

    /// Kitesurf: a scope equal to the one [`Self::ks_parts`] read, under `outer`, sharing its
    /// unique-id counter. Its lookup map is built by the first query that needs one.
    #[must_use]
    pub fn ks_rebuild(outer: Option<Self>, parts: KsScopeParts) -> Self {
        let context = outer.as_ref().map(|o| o.inner.context.clone()).unwrap_or_default();
        let bindings: Vec<Binding> = parts
            .bindings
            .into_iter()
            .map(|(name, index, flags)| Binding {
                name,
                index,
                flags: BindingFlags::from_bits_retain(flags),
            })
            .collect();
        let dropped = !bindings.is_empty();
        Self {
            inner: Rc::new(Inner {
                unique_id: parts.unique_id,
                outer,
                index: Cell::new(parts.index),
                bindings: RefCell::new(bindings),
                lookup: RefCell::default(),
                lookup_dropped: Cell::new(dropped),
                function: parts.function,
                this_escaped: Cell::new(parts.this_escaped),
                context,
            }),
        }
    }

    /// Kitesurf: frees the name lookup map once the compiler is done with this scope; a later
    /// query rebuilds it. A minified bundle's flat function scopes hold thousands of names.
    pub fn ks_drop_lookup(&self) {
        if let Ok(mut lookup) = self.inner.lookup.try_borrow_mut() {
            if lookup.capacity() != 0 {
                *lookup = rustc_hash::FxHashMap::default();
                self.inner.lookup_dropped.set(true);
            }
        }
    }
}

/// Kitesurf: a scope's own state, as [`Scope::ks_parts`] reads it: everything but its outer
/// scope and its bookkeeping. Bindings are `(name, index, flags)`.
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub struct KsScopeParts {
    pub unique_id: u32,
    pub index: u32,
    pub function: bool,
    pub this_escaped: bool,
    pub bindings: Vec<(JsString, u32, u8)>,
}

/// Additional state that all Scopes of a single AST share for bookkeeping.
#[derive(Debug, PartialEq, Default)]
struct ScopeContext {
    /// A counter for unique IDs for Scopes generated as part of this scope tree.
    ///
    /// The value is the highest unique ID assigned so far (initialized with 0
    /// which is also the unique ID of the global scope).
    ///
    /// Only used in the root scope.
    unique_id_ctr: Cell<u32>,
}

impl ScopeContext {
    /// Returns the next unique ID for a scope in this scope tree.
    fn next_unique_id(&self) -> u32 {
        let id = self.unique_id_ctr.get() + 1;
        self.unique_id_ctr.set(id);
        id
    }
}

/// A reference to an identifier in a scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IdentifierReference {
    locator: BindingLocator,
    lexical: bool,
    escapes: bool,
}

impl IdentifierReference {
    /// Create a new identifier reference.
    pub(crate) fn new(locator: BindingLocator, lexical: bool, escapes: bool) -> Self {
        Self {
            locator,
            lexical,
            escapes,
        }
    }

    /// Get the binding locator for this identifier reference.
    #[must_use]
    pub fn locator(&self) -> BindingLocator {
        self.locator.clone()
    }

    /// Returns if the binding can be function local.
    #[must_use]
    pub fn local(&self) -> bool {
        self.locator.scope > 0 && !self.escapes
    }

    /// Returns if the binding is on the global object.
    #[must_use]
    pub fn is_global_object(&self) -> bool {
        self.locator.scope == 0
    }

    /// Check if this identifier reference is lexical.
    #[must_use]
    pub fn is_lexical(&self) -> bool {
        self.lexical
    }
}

/// A binding locator contains all information about a binding that is needed to resolve it at runtime.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BindingLocator {
    /// Name of the binding.
    name: JsString,

    /// Scope of the binding.
    /// - 0: Global object
    /// - 1: Global declarative scope
    /// - n: Stack scope at index n - 2
    scope: u32,

    /// Index of the binding in the scope.
    binding_index: u32,

    unique_scope_id: u32,
}

impl BindingLocator {
    /// Kitesurf: `(name, scope, binding index, unique scope id)`, for an embedder that moves
    /// compiled code out of memory; [`Self::ks_from_parts`] puts them back together.
    #[must_use]
    pub fn ks_parts(&self) -> (&JsString, u32, u32, u32) {
        (&self.name, self.scope, self.binding_index, self.unique_scope_id)
    }

    /// Kitesurf: the locator [`Self::ks_parts`] read, put back together for code rebuilt
    /// from the store.
    #[must_use]
    pub const fn ks_from_parts(name: JsString, scope: u32, binding_index: u32, unique_scope_id: u32) -> Self {
        Self {
            name,
            scope,
            binding_index,
            unique_scope_id,
        }
    }

    /// Creates a new declarative binding locator that has knows indices.
    pub(crate) const fn declarative(
        name: JsString,
        scope_index: u32,
        binding_index: u32,
        unique_scope_id: u32,
    ) -> Self {
        Self {
            name,
            scope: scope_index + 1,
            binding_index,
            unique_scope_id,
        }
    }

    /// Creates a binding locator that indicates that the binding is on the global object.
    pub(super) const fn global(name: JsString) -> Self {
        Self {
            name,
            scope: 0,
            binding_index: 0,
            unique_scope_id: 0,
        }
    }

    /// Returns the name of the binding.
    #[must_use]
    pub const fn name(&self) -> &JsString {
        &self.name
    }

    /// Returns if the binding is located on the global object.
    #[must_use]
    pub const fn is_global(&self) -> bool {
        self.scope == 0
    }

    /// Returns the scope of the binding.
    #[must_use]
    pub fn scope(&self) -> BindingLocatorScope {
        match self.scope {
            0 => BindingLocatorScope::GlobalObject,
            1 => BindingLocatorScope::GlobalDeclarative,
            n => BindingLocatorScope::Stack(n - 2),
        }
    }

    /// Sets the scope of the binding.
    pub fn set_scope(&mut self, scope: BindingLocatorScope) {
        self.scope = match scope {
            BindingLocatorScope::GlobalObject => 0,
            BindingLocatorScope::GlobalDeclarative => 1,
            BindingLocatorScope::Stack(index) => index + 2,
        };
    }

    /// Returns the binding index of the binding.
    #[must_use]
    pub const fn binding_index(&self) -> u32 {
        self.binding_index
    }

    /// Sets the binding index of the binding.
    pub fn set_binding_index(&mut self, index: u32) {
        self.binding_index = index;
    }

    /// Returns the unique scope ID of the binding.
    #[must_use]
    pub fn unique_scope_id(&self) -> u32 {
        self.unique_scope_id
    }
}

/// Action that is returned when a fallible binding operation.
#[derive(Copy, Clone, Debug)]
pub enum BindingLocatorError {
    /// Trying to mutate immutable binding,
    MutateImmutable,

    /// Indicates that any action is silently ignored.
    Silent,
}

/// The scope in which a binding is located.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingLocatorScope {
    /// The binding is located on the global object.
    GlobalObject,

    /// The binding is located in the global declarative scope.
    GlobalDeclarative,

    /// The binding is located in the scope stack at the given index.
    Stack(u32),
}

/// A collection of function scopes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FunctionScopes {
    pub(crate) function_scope: Scope,
    pub(crate) parameters_eval_scope: Option<Scope>,
    pub(crate) parameters_scope: Option<Scope>,
    pub(crate) lexical_scope: Option<Scope>,
    pub(crate) mapped_arguments_object: bool,
    pub(crate) requires_function_scope: bool,
}

impl FunctionScopes {
    /// Returns the function scope for this function.
    #[must_use]
    pub fn function_scope(&self) -> &Scope {
        &self.function_scope
    }

    /// Returns if the arguments object is accessed in this function.
    #[must_use]
    pub fn arguments_object_accessed(&self) -> bool {
        if self
            .function_scope
            .inner
            .bindings
            .borrow()
            .first()
            .as_ref()
            .is_some_and(|b| b.name == "arguments" && b.is_accessed())
        {
            return true;
        }

        if let Some(scope) = &self.parameters_eval_scope
            && scope
                .inner
                .bindings
                .borrow()
                .first()
                .as_ref()
                .is_some_and(|b| b.name == "arguments" && b.is_accessed())
        {
            return true;
        }

        false
    }

    /// Check if the creation of the function scope is required.
    #[must_use]
    pub fn requires_function_scope(&self) -> bool {
        self.requires_function_scope
    }

    /// Returns the parameters eval scope for this function.
    #[must_use]
    pub fn parameters_eval_scope(&self) -> Option<&Scope> {
        self.parameters_eval_scope.as_ref()
    }

    /// Returns the parameters scope for this function.
    #[must_use]
    pub fn parameters_scope(&self) -> Option<&Scope> {
        self.parameters_scope.as_ref()
    }

    /// Returns the lexical scope for this function.
    #[must_use]
    pub fn lexical_scope(&self) -> Option<&Scope> {
        self.lexical_scope.as_ref()
    }

    /// Returns the effective parameter scope for this function.
    #[must_use]
    pub fn parameter_scope(&self) -> Scope {
        if let Some(parameters_eval_scope) = &self.parameters_eval_scope {
            return parameters_eval_scope.clone();
        }
        self.function_scope.clone()
    }

    /// Returns the effective body scope for this function.
    pub(crate) fn body_scope(&self) -> Scope {
        if let Some(lexical_scope) = &self.lexical_scope {
            return lexical_scope.clone();
        }
        if let Some(parameters_scope) = &self.parameters_scope {
            return parameters_scope.clone();
        }
        if let Some(parameters_eval_scope) = &self.parameters_eval_scope {
            return parameters_eval_scope.clone();
        }
        self.function_scope.clone()
    }

    /// Marks all bindings in all scopes as escaping.
    pub(crate) fn escape_all_bindings(&self) {
        self.function_scope.escape_all_bindings();
        if let Some(parameters_eval_scope) = &self.parameters_eval_scope {
            parameters_eval_scope.escape_all_bindings();
        }
        if let Some(parameters_scope) = &self.parameters_scope {
            parameters_scope.escape_all_bindings();
        }
        if let Some(lexical_scope) = &self.lexical_scope {
            lexical_scope.escape_all_bindings();
        }
    }

    pub(crate) fn reorder_binding_indices(&self) {
        self.function_scope.reorder_binding_indices();
        if let Some(parameters_eval_scope) = &self.parameters_eval_scope {
            parameters_eval_scope.reorder_binding_indices();
        }
        if let Some(parameters_scope) = &self.parameters_scope {
            parameters_scope.reorder_binding_indices();
        }
        if let Some(lexical_scope) = &self.lexical_scope {
            lexical_scope.reorder_binding_indices();
        }
    }
}

#[cfg(feature = "arbitrary")]
impl<'a> arbitrary::Arbitrary<'a> for FunctionScopes {
    fn arbitrary(_u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(Self {
            function_scope: Scope::new_global(),
            parameters_eval_scope: None,
            parameters_scope: None,
            lexical_scope: None,
            mapped_arguments_object: false,
            requires_function_scope: false,
        })
    }
}
