// Copyright 2026 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Reusing the glyphs of words that repeat across runs.
//!
//! Long documents shape the same words over and over. When no shaping rule of a font acts across
//! a space, a run's glyphs are its words' and spaces' glyphs laid end to end, so each distinct
//! word needs shaping only once per font and set of shaping options.
//!
//! Some fonts position a few glyphs by the space next to them, such as a mark that follows a
//! space. Those glyphs are listed as space-sensitive: a word that begins or ends with one is never
//! reused, and every word is shaped next to its neighbouring pieces, so the space is there.

use alloc::boxed::Box;
use alloc::vec::Vec;
use hashbrown::{HashMap, HashSet};
use skrifa::raw::{
    FontRef, ReadError, TableProvider,
    tables::{
        gpos::{PairPos, PositionSubtables, SinglePos},
        gsub::{SingleSubst, SubstitutionSubtables},
        layout::{
            ChainedSequenceContext, ChainedSequenceContextFormat1, ChainedSequenceContextFormat2,
            ChainedSequenceContextFormat3, ClassDef, CoverageTable, LookupFlag, SequenceContext,
        },
    },
    types::{BigEndian, GlyphId, GlyphId16, Tag},
};

/// One shaped glyph, in font units, with its cluster as a character index into the shaped text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RawGlyph {
    pub id: u32,
    pub cluster: u32,
    pub x_advance: i32,
    pub x_offset: i32,
    pub y_offset: i32,
}

impl RawGlyph {
    pub(crate) fn from_harfrust(info: &harfrust::GlyphInfo, pos: &harfrust::GlyphPosition) -> Self {
        Self {
            id: info.glyph_id,
            cluster: info.cluster,
            x_advance: pos.x_advance,
            x_offset: pos.x_offset,
            y_offset: pos.y_offset,
        }
    }
}

/// Words longer than this are rare enough that caching them costs more than it saves.
pub(crate) const MAX_WORD_BYTES: usize = 48;

/// Distinct words kept per font and shaping options; the cache restarts once it fills.
const MAX_WORDS: usize = 2048;

/// The glyphs of the words shaped with one font and one set of shaping options.
#[derive(Default)]
pub(crate) struct WordGlyphs {
    words: HashMap<Box<str>, Box<[RawGlyph]>>,
}

impl WordGlyphs {
    pub(crate) fn get(&self, word: &str) -> Option<&[RawGlyph]> {
        self.words.get(word).map(|glyphs| &**glyphs)
    }

    /// Keep `glyphs`, whose clusters count from the word's first character.
    pub(crate) fn insert(&mut self, word: &str, glyphs: &[RawGlyph]) {
        if self.words.len() >= MAX_WORDS {
            self.words.clear();
        }
        self.words.insert(word.into(), glyphs.into());
    }
}

/// The glyphs of a font whose shaping can depend on the space next to them.
pub(crate) type SpaceSensitive = HashSet<u32>;

/// How a font's shaping can see the U+0020 space glyph, or `None` when words cannot be reused.
///
/// Words can be reused when no GSUB lookup reads the space and GPOS reads it only as the glyph
/// directly before or after the glyphs it moves: as chained-context backtrack or lookahead, or as
/// the base a mark attaches to. Those moved glyphs are returned. Lookups that skip base glyphs,
/// tables outside GSUB and GPOS that shape sequences or scale by size, and unreadable tables all
/// rule reuse out.
pub(crate) fn space_sensitive_glyphs(font: &FontRef<'_>) -> Option<SpaceSensitive> {
    let mut scan = Scan {
        space: font.cmap().ok()?.map_codepoint(' ')?,
        sensitive: SpaceSensitive::new(),
    };
    for tag in [b"kern", b"kerx", b"morx", b"mort", b"trak"] {
        if font.table_data(Tag::new(tag)).is_some() {
            return None;
        }
    }
    scan.check(font).ok()?.then_some(scan.sensitive)
}

struct Scan {
    space: GlyphId,
    sensitive: SpaceSensitive,
}

impl Scan {
    /// Whether reuse is possible, collecting the space-sensitive glyphs on the way.
    fn check(&mut self, font: &FontRef<'_>) -> Result<bool, ReadError> {
        if let Ok(gdef) = font.gdef()
            && let Some(class_def) = gdef.glyph_class_def()
            && class_def?.get(self.space) > 1
        {
            // A space classed as a ligature, mark or component can be skipped by lookup flags.
            return Ok(false);
        }
        let ignores_bases = |flag: LookupFlag| flag.contains(LookupFlag::IGNORE_BASE_GLYPHS);
        if let Ok(gsub) = font.gsub() {
            for lookup in gsub.lookup_list()?.lookups().iter() {
                let lookup = lookup?;
                if ignores_bases(lookup.lookup_flag())
                    || gsub_reads(&lookup.subtables()?, self.space)?
                {
                    return Ok(false);
                }
            }
        }
        if let Ok(gpos) = font.gpos() {
            for lookup in gpos.lookup_list()?.lookups().iter() {
                let lookup = lookup?;
                if ignores_bases(lookup.lookup_flag()) || !self.gpos_allows(&lookup.subtables()?)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    fn gpos_allows(&mut self, subtables: &PositionSubtables<'_>) -> Result<bool, ReadError> {
        let g = self.space;
        match subtables {
            PositionSubtables::Single(s) => none(s.iter(), |t| match t {
                SinglePos::Format1(t) => covers(t.coverage(), g),
                SinglePos::Format2(t) => covers(t.coverage(), g),
            }),
            PositionSubtables::Pair(s) => none(s.iter(), |t| match t {
                PairPos::Format1(t) => Ok(covers(t.coverage(), g)?
                    || any(t.pair_sets().iter(), |set| {
                        any(set.pair_value_records().iter(), |record| {
                            Ok(record.second_glyph().to_u32() == g.to_u32())
                        })
                    })?),
                // Class 0 of the second class definition holds every glyph it does not list.
                PairPos::Format2(t) => Ok(covers(t.coverage(), g)?
                    || class_of(t.class_def2(), g) != 0
                    || t.class2_count() > 0 && class_zero_adjusts(&t)?),
            }),
            PositionSubtables::Cursive(s) => none(s.iter(), |t| covers(t.coverage(), g)),
            PositionSubtables::MarkToBase(s) => {
                for t in s.iter() {
                    let t = t?;
                    if covers(t.mark_coverage(), g)? {
                        return Ok(false);
                    }
                    if covers(t.base_coverage(), g)? {
                        self.sensitive
                            .extend(t.mark_coverage()?.iter().map(|m| m.to_u32()));
                    }
                }
                Ok(true)
            }
            PositionSubtables::MarkToLig(s) => none(s.iter(), |t| {
                Ok(covers(t.mark_coverage(), g)? || covers(t.ligature_coverage(), g)?)
            }),
            PositionSubtables::MarkToMark(s) => none(s.iter(), |t| {
                Ok(covers(t.mark1_coverage(), g)? || covers(t.mark2_coverage(), g)?)
            }),
            PositionSubtables::Contextual(s) => none(s.iter(), |t| context_reads(&t, g)),
            PositionSubtables::ChainContextual(s) => {
                for t in s.iter() {
                    if !self.chain_allows(&t?)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            PositionSubtables::EmptyExtension => Ok(true),
        }
    }

    /// Whether a chained context reads the space only at the far end of its backtrack or
    /// lookahead, adding the glyphs from the space up to and including its input. A rule that
    /// reads past the space would see the neighbouring word.
    fn chain_allows(&mut self, context: &ChainedSequenceContext<'_>) -> Result<bool, ReadError> {
        match context {
            ChainedSequenceContext::Format1(t) => self.chain_allows_glyphs(t),
            ChainedSequenceContext::Format2(t) => self.chain_allows_classes(t),
            ChainedSequenceContext::Format3(t) => self.chain_allows_coverages(t),
        }
    }

    /// [`Self::chain_allows`] for a context whose rules name glyphs.
    fn chain_allows_glyphs(
        &mut self,
        t: &ChainedSequenceContextFormat1<'_>,
    ) -> Result<bool, ReadError> {
        let g = self.space;
        let coverage = t.coverage()?;
        if coverage.get(g).is_some() {
            return Ok(false);
        }
        let is_space = is_glyph(g);
        for (first, set) in coverage.iter().zip(t.chained_seq_rule_sets().iter()) {
            let Some(set) = set else { continue };
            for rule in set?.chained_seq_rules().iter() {
                let rule = rule?;
                let (backtrack, lookahead) = (rule.backtrack_sequence(), rule.lookahead_sequence());
                if rule.input_sequence().iter().any(&is_space) {
                    return Ok(false);
                }
                let (Some(backtrack_at), Some(lookahead_at)) = (
                    reach(backtrack.iter().map(&is_space)),
                    reach(lookahead.iter().map(&is_space)),
                ) else {
                    return Ok(false);
                };
                if backtrack_at.is_none() && lookahead_at.is_none() {
                    continue;
                }
                let glyph = |g: &BigEndian<GlyphId16>| g.get().to_u32();
                self.sensitive.insert(first.to_u32());
                self.sensitive
                    .extend(rule.input_sequence().iter().map(glyph));
                self.sensitive
                    .extend(backtrack[..backtrack_at.unwrap_or(0)].iter().map(glyph));
                self.sensitive
                    .extend(lookahead[..lookahead_at.unwrap_or(0)].iter().map(glyph));
            }
        }
        Ok(true)
    }

    /// [`Self::chain_allows`] for a context whose rules name glyph classes.
    fn chain_allows_classes(
        &mut self,
        t: &ChainedSequenceContextFormat2<'_>,
    ) -> Result<bool, ReadError> {
        let g = self.space;
        if covers(t.coverage(), g)? {
            return Ok(false);
        }
        let space_class = [
            class_of(t.backtrack_class_def(), g),
            class_of(t.input_class_def(), g),
            class_of(t.lookahead_class_def(), g),
        ];

        // The classes, per class definition, of the glyphs the rules reading the space move or
        // match between it and their input.
        let mut moved_classes: [HashSet<u16>; 3] = Default::default();
        for (rule_set_class, set) in t.chained_class_seq_rule_sets().iter().enumerate() {
            let Some(set) = set else { continue };
            for rule in set?.chained_class_seq_rules().iter() {
                let rule = rule?;
                let (backtrack, lookahead) = (rule.backtrack_sequence(), rule.lookahead_sequence());
                if rule
                    .input_sequence()
                    .iter()
                    .any(|c| c.get() == space_class[1])
                {
                    return Ok(false);
                }
                let (Some(backtrack_at), Some(lookahead_at)) = (
                    reach(backtrack.iter().map(|c| c.get() == space_class[0])),
                    reach(lookahead.iter().map(|c| c.get() == space_class[2])),
                ) else {
                    return Ok(false);
                };
                if backtrack_at.is_none() && lookahead_at.is_none() {
                    continue;
                }
                #[expect(clippy::cast_possible_truncation, reason = "class set counts are u16")]
                moved_classes[1].insert(rule_set_class as u16);
                moved_classes[1].extend(rule.input_sequence().iter().map(|c| c.get()));
                moved_classes[0].extend(
                    backtrack[..backtrack_at.unwrap_or(0)]
                        .iter()
                        .map(|c| c.get()),
                );
                moved_classes[2].extend(
                    lookahead[..lookahead_at.unwrap_or(0)]
                        .iter()
                        .map(|c| c.get()),
                );
            }
        }

        let class_defs = [
            t.backtrack_class_def(),
            t.input_class_def(),
            t.lookahead_class_def(),
        ];
        for (classes, class_def) in moved_classes.iter().zip(class_defs) {
            if !self.mark_class_glyphs(classes, class_def)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Mark the glyphs of `class_def` in any of `classes` as space-sensitive.
    fn mark_class_glyphs(
        &mut self,
        classes: &HashSet<u16>,
        class_def: Result<ClassDef<'_>, ReadError>,
    ) -> Result<bool, ReadError> {
        if classes.is_empty() {
            return Ok(true);
        }
        if classes.contains(&0) {
            // Class 0 is every glyph its class definition leaves out.
            return Ok(false);
        }
        for (glyph, class) in class_def?.iter() {
            if classes.contains(&class) {
                self.sensitive.insert(glyph.to_u32());
            }
        }
        Ok(true)
    }

    /// [`Self::chain_allows`] for a context whose rules name coverages.
    fn chain_allows_coverages(
        &mut self,
        t: &ChainedSequenceContextFormat3<'_>,
    ) -> Result<bool, ReadError> {
        let g = self.space;
        if any(t.input_coverages().iter(), |c| covers(Ok(c), g))? {
            return Ok(false);
        }
        let backtrack = t.backtrack_coverages();
        let lookahead = t.lookahead_coverages();
        let reads = |c: Result<CoverageTable<'_>, ReadError>| covers(c, g);
        let (Some(backtrack_at), Some(lookahead_at)) = (
            reach(
                backtrack
                    .iter()
                    .map(reads)
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter(),
            ),
            reach(
                lookahead
                    .iter()
                    .map(reads)
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter(),
            ),
        ) else {
            return Ok(false);
        };
        if backtrack_at.is_none() && lookahead_at.is_none() {
            return Ok(true);
        }
        let between = backtrack.iter().take(backtrack_at.unwrap_or(0));
        let ahead = lookahead.iter().take(lookahead_at.unwrap_or(0));
        for coverage in t.input_coverages().iter().chain(between).chain(ahead) {
            self.sensitive.extend(coverage?.iter().map(|g| g.to_u32()));
        }
        Ok(true)
    }
}

/// Where a context sequence holds the space: `Some(None)` nowhere, `Some(Some(k))` at position
/// `k` with nothing past it, and `None` when it reads past the space.
fn reach(holds_space: impl Iterator<Item = bool>) -> Option<Option<usize>> {
    let mut found = None;
    for (position, holds) in holds_space.enumerate() {
        if found.is_some() {
            return None;
        }
        if holds {
            found = Some(position);
        }
    }
    Some(found)
}

fn covers(
    coverage: Result<CoverageTable<'_>, ReadError>,
    glyph: GlyphId,
) -> Result<bool, ReadError> {
    Ok(coverage?.get(glyph).is_some())
}

fn class_of(class_def: Result<ClassDef<'_>, ReadError>, glyph: GlyphId) -> u16 {
    // A missing class definition puts every glyph in class 0.
    class_def.map_or(0, |class_def| class_def.get(glyph))
}

fn is_glyph(glyph: GlyphId) -> impl Fn(&BigEndian<GlyphId16>) -> bool {
    move |g| g.get().to_u32() == glyph.to_u32()
}

fn any<T>(
    items: impl Iterator<Item = Result<T, ReadError>>,
    mut f: impl FnMut(T) -> Result<bool, ReadError>,
) -> Result<bool, ReadError> {
    for item in items {
        if f(item?)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn none<T>(
    items: impl Iterator<Item = Result<T, ReadError>>,
    f: impl FnMut(T) -> Result<bool, ReadError>,
) -> Result<bool, ReadError> {
    Ok(!any(items, f)?)
}

/// Whether some first glyph is adjusted next to a second glyph in class 0.
fn class_zero_adjusts(
    t: &skrifa::raw::tables::gpos::PairPosFormat2<'_>,
) -> Result<bool, ReadError> {
    any(t.class1_records().iter(), |record| {
        let Some(first) = record.class2_records().iter().next() else {
            return Ok(false);
        };
        let first = first?;
        Ok([first.value_record1(), first.value_record2()]
            .iter()
            .any(|value| {
                let nonzero = |v: Option<BigEndian<i16>>| v.is_some_and(|v| v.get() != 0);
                nonzero(value.x_placement)
                    || nonzero(value.y_placement)
                    || nonzero(value.x_advance)
                    || nonzero(value.y_advance)
                    || !value.x_placement_device.get().is_null()
                    || !value.y_placement_device.get().is_null()
                    || !value.x_advance_device.get().is_null()
                    || !value.y_advance_device.get().is_null()
            }))
    })
}

fn gsub_reads(subtables: &SubstitutionSubtables<'_>, g: GlyphId) -> Result<bool, ReadError> {
    match subtables {
        SubstitutionSubtables::Single(s) => any(s.iter(), |t| match t {
            SingleSubst::Format1(t) => covers(t.coverage(), g),
            SingleSubst::Format2(t) => covers(t.coverage(), g),
        }),
        SubstitutionSubtables::Multiple(s) => any(s.iter(), |t| covers(t.coverage(), g)),
        SubstitutionSubtables::Alternate(s) => any(s.iter(), |t| covers(t.coverage(), g)),
        SubstitutionSubtables::Ligature(s) => any(s.iter(), |t| {
            if covers(t.coverage(), g)? {
                return Ok(true);
            }
            any(t.ligature_sets().iter(), |set| {
                any(set.ligatures().iter(), |lig| {
                    Ok(lig.component_glyph_ids().iter().any(is_glyph(g)))
                })
            })
        }),
        SubstitutionSubtables::Contextual(s) => any(s.iter(), |t| context_reads(&t, g)),
        SubstitutionSubtables::ChainContextual(s) => any(s.iter(), |t| chain_reads(&t, g)),
        SubstitutionSubtables::Reverse(s) => any(s.iter(), |t| {
            Ok(covers(t.coverage(), g)?
                || any(t.backtrack_coverages().iter(), |c| covers(Ok(c), g))?
                || any(t.lookahead_coverages().iter(), |c| covers(Ok(c), g))?)
        }),
        SubstitutionSubtables::EmptyExtension => Ok(false),
    }
}

fn context_reads(context: &SequenceContext<'_>, g: GlyphId) -> Result<bool, ReadError> {
    match context {
        SequenceContext::Format1(t) => {
            if covers(t.coverage(), g)? {
                return Ok(true);
            }
            any(t.seq_rule_sets().iter().flatten(), |set| {
                any(set.seq_rules().iter(), |rule| {
                    Ok(rule.input_sequence().iter().any(is_glyph(g)))
                })
            })
        }
        SequenceContext::Format2(t) => {
            if covers(t.coverage(), g)? {
                return Ok(true);
            }
            let class = class_of(t.class_def(), g);
            any(t.class_seq_rule_sets().iter().flatten(), |set| {
                any(set.class_seq_rules().iter(), |rule| {
                    Ok(rule.input_sequence().iter().any(|c| c.get() == class))
                })
            })
        }
        SequenceContext::Format3(t) => any(t.coverages().iter(), |c| covers(Ok(c), g)),
    }
}

fn chain_reads(context: &ChainedSequenceContext<'_>, g: GlyphId) -> Result<bool, ReadError> {
    match context {
        ChainedSequenceContext::Format1(t) => {
            if covers(t.coverage(), g)? {
                return Ok(true);
            }
            any(t.chained_seq_rule_sets().iter().flatten(), |set| {
                any(set.chained_seq_rules().iter(), |rule| {
                    Ok(rule.backtrack_sequence().iter().any(is_glyph(g))
                        || rule.input_sequence().iter().any(is_glyph(g))
                        || rule.lookahead_sequence().iter().any(is_glyph(g)))
                })
            })
        }
        ChainedSequenceContext::Format2(t) => {
            if covers(t.coverage(), g)? {
                return Ok(true);
            }
            let backtrack = class_of(t.backtrack_class_def(), g);
            let input = class_of(t.input_class_def(), g);
            let lookahead = class_of(t.lookahead_class_def(), g);
            any(t.chained_class_seq_rule_sets().iter().flatten(), |set| {
                any(set.chained_class_seq_rules().iter(), |rule| {
                    Ok(rule
                        .backtrack_sequence()
                        .iter()
                        .any(|c| c.get() == backtrack)
                        || rule.input_sequence().iter().any(|c| c.get() == input)
                        || rule
                            .lookahead_sequence()
                            .iter()
                            .any(|c| c.get() == lookahead))
                })
            })
        }
        ChainedSequenceContext::Format3(t) => {
            Ok(any(t.backtrack_coverages().iter(), |c| covers(Ok(c), g))?
                || any(t.input_coverages().iter(), |c| covers(Ok(c), g))?
                || any(t.lookahead_coverages().iter(), |c| covers(Ok(c), g))?)
        }
    }
}
