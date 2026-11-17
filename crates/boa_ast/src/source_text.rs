use std::borrow::Cow;

use crate::{LinearPosition, LinearSpan};

/// Source text.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug)]
pub struct SourceText {
    repr: Repr,
}

// Collected as UTF-16, then kept for the script's lifetime (toString);
// `compact` stores it as Latin-1 when it fits and drops the growth slack.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug)]
enum Repr {
    Collecting(Vec<u16>),
    Latin1(Box<[u8]>),
    Utf16(Box<[u16]>),
}

impl SourceText {
    /// Constructs a new, empty `SourceText` with at least the specified capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            repr: Repr::Collecting(Vec::with_capacity(capacity)),
        }
    }

    fn len(&self) -> usize {
        match &self.repr {
            Repr::Collecting(v) => v.len(),
            Repr::Latin1(v) => v.len(),
            Repr::Utf16(v) => v.len(),
        }
    }

    fn slice(&self, start: usize, end: usize) -> Cow<'_, [u16]> {
        match &self.repr {
            Repr::Collecting(v) => Cow::Borrowed(&v[start..end]),
            Repr::Utf16(v) => Cow::Borrowed(&v[start..end]),
            Repr::Latin1(v) => Cow::Owned(v[start..end].iter().map(|&b| u16::from(b)).collect()),
        }
    }

    /// Get current `LinearPosition`.
    #[must_use]
    pub fn cur_linear_position(&self) -> LinearPosition {
        LinearPosition::new(self.len())
    }

    /// Get code points from `pos` to the current end.
    #[must_use]
    pub fn get_code_points_from_pos(&self, pos: LinearPosition) -> Cow<'_, [u16]> {
        self.slice(pos.pos(), self.len())
    }

    /// Get code points within `span`.
    #[must_use]
    pub fn get_code_points_from_span(&self, span: LinearSpan) -> Cow<'_, [u16]> {
        self.slice(span.start().pos(), span.end().pos())
    }

    /// Shrink the collected text for long-term storage, as Latin-1 when
    /// every code unit fits (most bundles), else as an exact-size UTF-16 slice.
    pub fn compact(&mut self) {
        let Repr::Collecting(v) = &mut self.repr else {
            return;
        };
        let v = std::mem::take(v);
        self.repr = if v.iter().all(|&c| c <= 0xFF) {
            Repr::Latin1(v.iter().map(|&c| c as u8).collect())
        } else {
            Repr::Utf16(v.into_boxed_slice())
        };
    }

    /// Kitesurf: the heap bytes this text holds, so an embedder can account for what a
    /// script retains for `toString` apart from its compiled code.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        match &self.repr {
            Repr::Collecting(v) => v.capacity() * 2,
            Repr::Latin1(v) => v.len(),
            Repr::Utf16(v) => v.len() * 2,
        }
    }

    /// Kitesurf: the compacted text as bytes an embedder can store, and whether they are
    /// Latin-1 (else UTF-16LE); None until `compact` has run.
    #[must_use]
    pub fn stored_form(&self) -> Option<(Cow<'_, [u8]>, bool)> {
        match &self.repr {
            Repr::Collecting(_) => None,
            Repr::Latin1(v) => Some((Cow::Borrowed(v), true)),
            Repr::Utf16(v) => Some((Cow::Owned(v.iter().flat_map(|c| c.to_le_bytes()).collect()), false)),
        }
    }

    // Collection after `compact` is not expected, but stays correct: re-expand.
    #[inline]
    fn collecting(&mut self) -> &mut Vec<u16> {
        if !matches!(self.repr, Repr::Collecting(_)) {
            let v = self.slice(0, self.len()).into_owned();
            self.repr = Repr::Collecting(v);
        }
        let Repr::Collecting(v) = &mut self.repr else {
            unreachable!("just converted to Collecting")
        };
        v
    }

    /// Remove last code point.
    #[inline]
    pub fn remove_last_code_point(&mut self) {
        self.collecting().pop();
    }

    /// Collect code point.
    ///
    /// # Panics
    ///
    /// On invalid code point.
    #[inline]
    pub fn collect_code_point(&mut self, cp: u32) {
        if let Ok(cu) = cp.try_into() {
            self.push(cu);
            return;
        }
        let cp = cp - 0x10000;
        let cu1 = (cp / 0x400 + 0xD800)
            .try_into()
            .expect("Invalid code point");
        let cu2 = (cp % 0x400 + 0xDC00)
            .try_into()
            .expect("Invalid code point");
        self.push(cu1);
        self.push(cu2);
    }

    #[inline]
    fn push(&mut self, cp: u16) {
        self.collecting().push(cp);
    }
}

const DEFAULT_CAPACITY: usize = 4 * 1024;

impl Default for SourceText {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}
