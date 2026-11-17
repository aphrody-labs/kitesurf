use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

use boa_ast::LinearSpan;
use boa_gc::{Finalize, Trace};

use crate::{ks_code_stats as stats, ks_code_store::CodeStore};

// Kitesurf: resident until an idle point finds the script still alive and the embedder's
// store takes the text; `toString` then reads back only the span it asks for.
enum Repr {
    Resident(boa_ast::SourceText),
    Swapped {
        store: Rc<dyn CodeStore>,
        key: u32,
        bytes: usize,
        latin1: bool,
    },
}

enum Swap {
    Done,
    Busy,
    Refused,
}

struct Inner {
    repr: RefCell<Repr>,
    charged: Cell<i64>,
}

thread_local! {
    // Kitesurf: texts that may be swapped at the next idle point, held weakly so a script
    // that dies inside its own eval never reaches the store.
    static PENDING: RefCell<Vec<Weak<Inner>>> = const { RefCell::new(Vec::new()) };
}

impl Inner {
    fn new(mut source_text: boa_ast::SourceText, swappable: bool) -> Rc<Self> {
        // Kitesurf: kept for the script's life, so drop the lexer's growth slack
        // and store Latin-1 when it fits (half of a bundle's retained source).
        source_text.compact();
        let retained = source_text.retained_bytes();
        let charged = stats::bytes(retained);
        stats::update(|s| {
            s.source_texts += 1;
            s.source_bytes += charged;
        });
        let inner = Rc::new(Self {
            repr: RefCell::new(Repr::Resident(source_text)),
            charged: Cell::new(charged),
        });
        let wanted = swappable
            && crate::ks_code_store::current().is_some_and(|store| retained >= store.min_bytes());
        if wanted {
            let weak = Rc::downgrade(&inner);
            let _ = PENDING.try_with(|p| {
                if let Ok(mut p) = p.try_borrow_mut() {
                    p.push(weak);
                }
            });
        }
        inner
    }

    fn swap(&self, store: &Rc<dyn CodeStore>) -> Swap {
        let Ok(mut repr) = self.repr.try_borrow_mut() else {
            return Swap::Busy;
        };
        let Repr::Resident(text) = &*repr else {
            return Swap::Refused;
        };
        let Some((form, latin1)) = text.stored_form() else {
            return Swap::Refused;
        };
        let Some(key) = store.put(&form) else {
            return Swap::Refused;
        };
        let bytes = form.len();
        drop(form);
        *repr = Repr::Swapped {
            store: store.clone(),
            key,
            bytes,
            latin1,
        };
        let charged = self.charged.replace(0);
        stats::update(|s| {
            s.source_texts -= 1;
            s.source_bytes -= charged;
            s.source_swapped_texts += 1;
            s.source_swapped_bytes += stats::bytes(bytes);
        });
        Swap::Done
    }

    fn map_span<R>(&self, span: LinearSpan, f: impl FnOnce(&[u16]) -> R) -> Option<R> {
        let repr = self.repr.try_borrow().ok()?;
        let (store, key, bytes, latin1) = match &*repr {
            Repr::Resident(text) => return Some(f(&text.get_code_points_from_span(span))),
            Repr::Swapped {
                store,
                key,
                bytes,
                latin1,
            } => (store, *key, *bytes, *latin1),
        };
        let unit = if latin1 { 1 } else { 2 };
        let (start, end) = (span.start().pos() * unit, span.end().pos() * unit);
        stats::update(|s| s.source_reads += 1);
        let mut buf = Vec::new();
        if start > end || end > bytes || !store.read(key, start, end - start, &mut buf) || buf.len() != end - start {
            stats::update(|s| s.store_failures += 1);
            return None;
        }
        let points: Vec<u16> = if latin1 {
            buf.iter().map(|&b| u16::from(b)).collect()
        } else {
            buf.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
        };
        Some(f(&points))
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        match self.repr.get_mut() {
            Repr::Resident(_) => {
                let charged = self.charged.get();
                stats::update(|s| {
                    s.source_texts -= 1;
                    s.source_bytes -= charged;
                });
            }
            Repr::Swapped {
                store, key, bytes, ..
            } => {
                let len = *bytes;
                let bytes = stats::bytes(len);
                stats::update(|s| {
                    s.source_swapped_texts -= 1;
                    s.source_swapped_bytes -= bytes;
                });
                store.release(*key, len);
            }
        }
    }
}

/// Kitesurf: hands the installed code store every still-live source text registered since
/// the last call. Safe at any point: a text being read is retried at the next call.
pub(crate) fn swap_source_texts() -> usize {
    let Some(store) = crate::ks_code_store::current() else {
        return 0;
    };
    let pending = PENDING
        .try_with(|p| p.try_borrow_mut().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default())
        .unwrap_or_default();
    let mut busy = Vec::new();
    let mut swapped = 0;
    for weak in pending {
        let Some(inner) = weak.upgrade() else {
            continue;
        };
        match inner.swap(&store) {
            Swap::Done => swapped += 1,
            Swap::Busy => busy.push(weak),
            Swap::Refused => {}
        }
    }
    if !busy.is_empty() {
        let _ = PENDING.try_with(|p| {
            if let Ok(mut p) = p.try_borrow_mut() {
                p.extend(busy);
            }
        });
    }
    swapped
}

#[derive(Default, Trace, Finalize, Clone)]
pub(crate) struct SourceText {
    #[unsafe_ignore_trace]
    source_text: Option<Rc<Inner>>,
}

impl SourceText {
    #[must_use]
    pub(crate) fn new(source_text: boa_ast::SourceText) -> Self {
        Self {
            source_text: Some(Inner::new(source_text, true)),
        }
    }

    /// Kitesurf: a text that lives no longer than the call making it (JSON.parse), which
    /// must never cost a store write.
    #[must_use]
    pub(crate) fn new_transient(source_text: boa_ast::SourceText) -> Self {
        Self {
            source_text: Some(Inner::new(source_text, false)),
        }
    }

    fn new_empty() -> Self {
        Self { source_text: None }
    }

    fn is_empty(&self) -> bool {
        self.source_text.is_none()
    }
}

/// Contains pointer to source code and span of the object.
#[derive(Default, Clone)]
pub struct SpannedSourceText {
    source_text: SourceText,
    span: Option<LinearSpan>,
}

impl SpannedSourceText {
    pub(crate) fn new(source_text: SourceText, span: Option<LinearSpan>) -> Self {
        Self { source_text, span }
    }

    pub(crate) fn new_source_only(source_text: SourceText) -> Self {
        Self {
            source_text,
            span: None,
        }
    }

    pub(crate) fn new_empty() -> Self {
        Self {
            source_text: SourceText::new_empty(),
            span: None,
        }
    }

    /// Creates new [`SpannedSourceText`] with the same [`SourceText`] but without its span.
    pub(crate) fn clone_only_source(&self) -> Self {
        Self {
            source_text: self.source_text.clone(),
            span: None,
        }
    }

    /// Returns the [`SourceText`].
    #[must_use]
    pub(crate) fn source_text(&self) -> SourceText {
        self.source_text.clone()
    }

    /// Kitesurf: the span alone, for an embedder that rebuilds this text from its parts
    /// around a source text it already holds.
    pub(crate) fn span(&self) -> Option<LinearSpan> {
        self.span
    }

    /// Kitesurf: whether `other` points into the same source text, which the blocks of
    /// one swapped subtree must share.
    pub(crate) fn ks_same_source(&self, other: &Self) -> bool {
        match (&self.source_text.source_text, &other.source_text.source_text) {
            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    }

    /// Test if the span is empty.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        let span_is_empty = if let Some(x) = self.span {
            x.is_empty()
        } else {
            true
        };
        span_is_empty || self.source_text.is_empty()
    }

    /// Kitesurf: runs `f` on the code points of the span, from memory or from the store
    /// the text was swapped to. None without a span, or when the store cannot read them.
    pub fn map_code_points<R>(&self, f: impl FnOnce(&[u16]) -> R) -> Option<R> {
        let (inner, span) = (self.source_text.source_text.as_ref()?, self.span?);
        inner.map_span(span, f)
    }
}

impl std::fmt::Debug for SpannedSourceText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceTextSpanned").finish()
    }
}
impl std::fmt::Debug for SourceText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceTextInner").finish()
    }
}
