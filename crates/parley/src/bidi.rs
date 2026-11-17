// Copyright 2026 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Bidirectional text configuration.

/// The paragraph's base direction.
///
/// A superset of [`parlance::BaseDirection`] (which `parley_engine` takes) adding
/// [`Self::AutoPerParagraph`], the CSS `unicode-bidi: plaintext` behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BaseDirection {
    /// Choose direction automatically, by a single first-strong detection over the whole text.
    #[default]
    Auto,
    /// Choose direction automatically per UAX #9 paragraph: first-strong detection re-runs after
    /// every paragraph separator (CRLF counts as one).
    AutoPerParagraph,
    /// Left-to-right.
    Ltr,
    /// Right-to-left.
    Rtl,
}

impl From<parlance::BaseDirection> for BaseDirection {
    fn from(value: parlance::BaseDirection) -> Self {
        match value {
            parlance::BaseDirection::Auto => Self::Auto,
            parlance::BaseDirection::Ltr => Self::Ltr,
            parlance::BaseDirection::Rtl => Self::Rtl,
        }
    }
}

impl BaseDirection {
    /// The direction to pass to `parley_engine`, and whether its automatic detection runs per
    /// paragraph (`AnalysisOptions::auto_per_paragraph`).
    pub(crate) fn to_engine(self) -> (parlance::BaseDirection, bool) {
        match self {
            Self::Auto => (parlance::BaseDirection::Auto, false),
            Self::AutoPerParagraph => (parlance::BaseDirection::Auto, true),
            Self::Ltr => (parlance::BaseDirection::Ltr, false),
            Self::Rtl => (parlance::BaseDirection::Rtl, false),
        }
    }
}

// First-strong scan for HTML's dir=auto directionality.
// Callers walk DOM text nodes and stop at the first Some, applying
// the spec's subtree exclusions themselves. This only classifies
// one text chunk: strong L is Ltr, strong R or AL is Rtl.
/// Returns the direction of the first strong character in `text`, or `None` if it has none.
pub fn first_strong_direction(text: &str) -> Option<BaseDirection> {
    parley_engine::first_strong_direction(text).map(BaseDirection::from)
}
