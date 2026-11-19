// Copyright 2026 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

#![expect(missing_docs, reason = "Deferred")]

use core::ops::Range;

use alloc::vec::Vec;
use thin_vec::ThinVec;

use crate::shape::Whitespace;

/// Data for a single character of the source text.
///
/// This is a character in the Unicode scalar value sense, i.e., it corresponds to a single `char`
/// of the source text.
///
/// The character's position in the source text is not stored here; see
/// [`ShapedText::char_byte_range`](crate::ShapedText::char_byte_range).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Character {
    /// Style index for this character.
    pub style_index: u16,
    /// The whitespace class of this character.
    pub whitespace: Whitespace,
    /// Properties of this character, including whether it begins a grapheme cluster.
    pub flags: CharacterFlags,
}

const _: () = assert!(size_of::<Character>() == 4);

impl Character {
    /// Returns if this character is any whitespace.
    #[inline(always)]
    pub fn is_whitespace(self) -> bool {
        self.whitespace != Whitespace::None
    }

    /// Whether this character begins a grapheme cluster.
    #[inline(always)]
    pub fn is_grapheme_start(self) -> bool {
        self.flags.is_grapheme_start()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShapedClusterFlags(u16);

impl ShapedClusterFlags {
    const GLYPH_LEN_MASK: u16 = 0x00FF;
    const INLINE_GLYPH: u16 = 1 << 8;
    const GRAPHEME_START: u16 = 1 << 9;
    const SAFE_TO_BREAK_BEFORE: u16 = 1 << 10;
    /// Whether a soft wrap opportunity exists before the cluster's first character.
    const SOFT_WRAP_BEFORE: u16 = 1 << 11;
    /// [`Whitespace`] class of the cluster's first character (3 bits).
    const WHITESPACE_SHIFT: u16 = 12;
    const WHITESPACE_MASK: u16 = 0b111 << Self::WHITESPACE_SHIFT;

    #[inline(always)]
    pub(crate) const fn new(glyph_len: u8) -> Self {
        Self(glyph_len as u16)
    }

    #[inline(always)]
    pub(crate) const fn with_inline_glyph(mut self, set: bool) -> Self {
        self.0 = self.0 & !Self::INLINE_GLYPH | if set { Self::INLINE_GLYPH } else { 0 };
        self
    }

    #[inline(always)]
    pub(crate) const fn with_grapheme_start(mut self, set: bool) -> Self {
        self.0 = self.0 & !Self::GRAPHEME_START | if set { Self::GRAPHEME_START } else { 0 };
        self
    }

    #[inline(always)]
    pub(crate) const fn with_safe_to_break_before(mut self, set: bool) -> Self {
        self.0 =
            self.0 & !Self::SAFE_TO_BREAK_BEFORE | if set { Self::SAFE_TO_BREAK_BEFORE } else { 0 };
        self
    }

    #[inline(always)]
    pub(crate) const fn with_first_char(
        mut self,
        soft_wrap_opportunity: bool,
        whitespace: Whitespace,
    ) -> Self {
        self.0 = self.0 & !(Self::SOFT_WRAP_BEFORE | Self::WHITESPACE_MASK)
            | if soft_wrap_opportunity {
                Self::SOFT_WRAP_BEFORE
            } else {
                0
            }
            | ((whitespace as u16) << Self::WHITESPACE_SHIFT);
        self
    }

    #[inline(always)]
    const fn is_soft_wrap_opportunity_before(self) -> bool {
        self.0 & Self::SOFT_WRAP_BEFORE != 0
    }

    #[inline(always)]
    const fn whitespace(self) -> Whitespace {
        match (self.0 & Self::WHITESPACE_MASK) >> Self::WHITESPACE_SHIFT {
            1 => Whitespace::Space,
            2 => Whitespace::NoBreakSpace,
            3 => Whitespace::IdeographicSpace,
            4 => Whitespace::OtherSpaceSeparator,
            5 => Whitespace::Tab,
            6 => Whitespace::Newline,
            7 => Whitespace::ControlWhitespace,
            _ => Whitespace::None,
        }
    }

    #[inline(always)]
    const fn glyph_len(self) -> u8 {
        (self.0 & Self::GLYPH_LEN_MASK) as u8
    }

    #[inline(always)]
    const fn has_inline_glyph(self) -> bool {
        self.0 & Self::INLINE_GLYPH != 0
    }

    #[inline(always)]
    const fn is_grapheme_start(self) -> bool {
        self.0 & Self::GRAPHEME_START != 0
    }

    #[inline(always)]
    const fn is_safe_to_break_before(self) -> bool {
        self.0 & Self::SAFE_TO_BREAK_BEFORE != 0
    }
}

/// A span of characters and the glyphs they shaped into.
///
/// Shaping may reorder, compose, decompose, etc.; there is no finer-grained correspondence between
/// characters and glyphs. For example, a base letter with a combining mark may shape into a single
/// glyph, a single character may shape into multiple glyphs, and a ligature may combine multiple
/// characters into shared glyphs.
///
/// Shaped cluster boundaries are not necessarily [`Grapheme`][crate::Grapheme] boundaries. The
/// shared boundaries are encoded by [`Atom`][crate::Atom]s.
///
/// For more information about clusters, see [HarfBuzz's documentation][harfbuzz].
///
/// [harfbuzz]: https://harfbuzz.github.io/working-with-harfbuzz-clusters.html
///
/// The cluster's first character and style are not stored here; see
/// [`ShapedText::cluster_chars_start`](crate::ShapedText::cluster_chars_start) and
/// [`ShapedText::cluster_style_index`](crate::ShapedText::cluster_style_index).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ShapedCluster {
    /// Advance width for this cluster
    pub advance: f32,

    /// The index into the glyph array where this cluster's glyphs start.
    ///
    /// If [`Self::has_inline_glyph`] is `true`, this is a glyph identifier instead. For more, see
    /// the documentation on that method.
    pub glyph_offset: u32,

    pub(crate) flags: ShapedClusterFlags,

    /// This cluster's first character index relative to its block base in [`ShapedIndex`].
    pub(crate) chars_delta: u16,
}

const _: () = assert!(size_of::<ShapedCluster>() == 12);

impl ShapedCluster {
    /// The number of glyphs of this cluster.
    #[inline(always)]
    pub fn glyph_len(self) -> u8 {
        if self.has_inline_glyph() {
            1
        } else {
            self.flags.glyph_len()
        }
    }

    /// Whether this cluster's glyph is stored inline in [`Self::glyph_offset`].
    ///
    /// This is only possible if [`Self::glyph_len`] is one, and the glyph has no offset.
    /// [`Self::glyph_offset`] then encodes the glyph identifier rather than an index into the glyph
    /// array. The glyph's advance then is this cluster's advance.
    #[inline(always)]
    pub fn has_inline_glyph(self) -> bool {
        self.flags.has_inline_glyph()
    }

    /// Whether this shaped cluster's logical start also starts a grapheme.
    #[inline(always)]
    pub fn is_grapheme_start(self) -> bool {
        self.flags.is_grapheme_start()
    }

    /// Whether breaking logically before this shaped cluster requires reshaping.
    ///
    /// Note that if this shaped cluster does not start a grapheme (see
    /// [`Self::is_grapheme_start`]), you have to reshape regardless of this value.
    #[inline(always)]
    pub fn is_safe_to_break_before(self) -> bool {
        self.flags.is_safe_to_break_before()
    }

    /// Whether a soft wrap opportunity exists before this cluster's first character.
    ///
    /// This is [`Character::flags`]' soft-wrap bit of the first character of this cluster,
    /// cached here so that measuring text does not need to touch the character array.
    #[inline(always)]
    pub fn is_soft_wrap_opportunity_before(self) -> bool {
        self.flags.is_soft_wrap_opportunity_before()
    }

    /// The [`Whitespace`] class of this cluster's first character.
    ///
    /// See [`Self::is_soft_wrap_opportunity_before`].
    #[inline(always)]
    pub fn whitespace(self) -> Whitespace {
        self.flags.whitespace()
    }
}

/// The number of graphemes the characters `chars` overlap, assuming the first starts one.
pub(crate) fn graphemes_overlapped(chars: Range<u32>, characters: &[Character]) -> u32 {
    let mut graphemes = 1;
    for character in &characters[chars.start as usize + 1..chars.end as usize] {
        graphemes += u32::from(character.is_grapheme_start());
    }
    graphemes
}

/// Log2 of the clusters per stored base in [`ShapedIndex`].
const CLUSTER_SHIFT: u8 = 12;
/// Characters per stored byte offset in [`ShapedIndex`].
const CHAR_BLOCK: usize = 32;

/// How [`ShapedIndex`] finds a character's byte offset.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
enum CharBytes {
    /// Every character so far is one byte, so the offset follows from the first.
    #[default]
    OneByte,
    /// A checkpoint every `CHAR_BLOCK` characters, plus the UTF-8 lengths before the character.
    Sparse,
    /// The characters are not contiguous in the text, so every offset is stored.
    Dense,
}

/// Positions of clusters and characters, kept sparse so neither stores a `u32` of its own.
///
/// A cluster's first character is its block's base plus [`ShapedCluster::chars_delta`]; once a
/// delta overflows, every cluster is its own block. Each table's first base is inline.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ShapedIndex {
    /// Base of every block of clusters after the first.
    cluster_chars: ThinVec<u32>,
    /// Offset of every `CHAR_BLOCK`th character after the first, or every character's when dense.
    char_bytes: ThinVec<u32>,
    first_cluster_chars: u32,
    first_char_byte: u32,
    /// Byte offset just past the last character.
    text_byte_end: u32,
    /// Log2 of the clusters per block: `CLUSTER_SHIFT`, or 0 once a delta overflowed.
    cluster_shift: u8,
    char_mode: CharBytes,
}

impl Default for ShapedIndex {
    fn default() -> Self {
        Self {
            cluster_chars: ThinVec::new(),
            char_bytes: ThinVec::new(),
            first_cluster_chars: 0,
            first_char_byte: 0,
            text_byte_end: 0,
            cluster_shift: CLUSTER_SHIFT,
            char_mode: CharBytes::OneByte,
        }
    }
}

impl ShapedIndex {
    pub(crate) fn clear(&mut self) {
        self.cluster_chars.clear();
        self.char_bytes.clear();
        self.first_cluster_chars = 0;
        self.first_char_byte = 0;
        self.text_byte_end = 0;
        self.cluster_shift = CLUSTER_SHIFT;
        self.char_mode = CharBytes::OneByte;
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.cluster_chars.shrink_to_fit();
        self.char_bytes.shrink_to_fit();
    }

    #[inline]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "checked against `u16::MAX`"
    )]
    pub(crate) fn push_cluster(
        &mut self,
        clusters: &mut Vec<ShapedCluster>,
        mut cluster: ShapedCluster,
        chars_start: u32,
    ) {
        cluster.chars_delta = 0;
        let index = clusters.len();
        if index == 0 {
            self.first_cluster_chars = chars_start;
        } else if index & ((1 << self.cluster_shift) - 1) == 0 {
            self.cluster_chars.push(chars_start);
        } else {
            let base = self.cluster_block_base(index >> self.cluster_shift);
            match chars_start.checked_sub(base) {
                Some(delta) if delta <= u32::from(u16::MAX) => cluster.chars_delta = delta as u16,
                _ => self.densify_clusters(clusters, chars_start),
            }
        }
        clusters.push(cluster);
    }

    /// Gives every cluster its own base, with a zero delta, then appends `chars_start`'s.
    #[cold]
    #[inline(never)]
    fn densify_clusters(&mut self, clusters: &mut [ShapedCluster], chars_start: u32) {
        self.cluster_chars = (1..clusters.len())
            .map(|i| self.cluster_chars_start(clusters, i))
            .collect();
        self.cluster_chars.push(chars_start);
        for cluster in clusters.iter_mut() {
            cluster.chars_delta = 0;
        }
        self.cluster_shift = 0;
    }

    #[inline(always)]
    fn cluster_block_base(&self, block: usize) -> u32 {
        if block == 0 {
            self.first_cluster_chars
        } else {
            self.cluster_chars[block - 1]
        }
    }

    #[inline(always)]
    pub(crate) fn cluster_chars_start(&self, clusters: &[ShapedCluster], index: usize) -> u32 {
        self.cluster_block_base(index >> self.cluster_shift)
            + u32::from(clusters[index].chars_delta)
    }

    #[inline]
    #[expect(clippy::cast_possible_truncation, reason = "`len_utf8` is at most 4")]
    pub(crate) fn push_char(
        &mut self,
        characters: &mut Vec<Character>,
        character: Character,
        byte_start: u32,
    ) {
        let index = characters.len();
        let len_utf8 = character.flags.len_utf8() as u32;
        if index == 0 {
            self.first_char_byte = byte_start;
        } else if self.char_mode != CharBytes::Dense && byte_start != self.text_byte_end {
            self.densify_chars(characters);
        }
        if self.char_mode == CharBytes::OneByte && len_utf8 != 1 {
            self.checkpoint_chars(index);
        }
        // A sparse index stores every `CHAR_BLOCK`th character's offset; the first is inline.
        let stores_checkpoint = index != 0 && index % CHAR_BLOCK == 0;
        match self.char_mode {
            CharBytes::OneByte => {}
            CharBytes::Sparse if !stores_checkpoint => {}
            CharBytes::Sparse | CharBytes::Dense => self.char_bytes.push(byte_start),
        }
        self.text_byte_end = byte_start + len_utf8;
        characters.push(character);
    }

    /// Stores every character's byte offset.
    #[cold]
    #[inline(never)]
    fn densify_chars(&mut self, characters: &[Character]) {
        self.char_bytes = (0..characters.len())
            .map(|i| self.char_byte_start(characters, i))
            .collect();
        self.char_mode = CharBytes::Dense;
    }

    /// Checkpoints the `len` one-byte characters so far, for a multi-byte one to follow.
    #[cold]
    #[inline(never)]
    #[expect(clippy::cast_possible_truncation, reason = "offsets are `u32`")]
    fn checkpoint_chars(&mut self, len: usize) {
        let first = self.first_char_byte;
        self.char_bytes = (1..len.div_ceil(CHAR_BLOCK))
            .map(|block| first + (block * CHAR_BLOCK) as u32)
            .collect();
        self.char_mode = CharBytes::Sparse;
    }

    /// The byte offset of the character at `index` in the source text.
    #[inline]
    #[expect(clippy::cast_possible_truncation, reason = "`len_utf8` is at most 4")]
    pub(crate) fn char_byte_start(&self, characters: &[Character], index: usize) -> u32 {
        match self.char_mode {
            CharBytes::OneByte => return self.first_char_byte + index as u32,
            CharBytes::Dense => return self.char_bytes[index],
            CharBytes::Sparse => {}
        }
        let block = index / CHAR_BLOCK;
        let base = if block == 0 {
            self.first_char_byte
        } else {
            self.char_bytes[block - 1]
        };
        let block_start = block * CHAR_BLOCK;
        let block_len = (characters.len() - block_start).min(CHAR_BLOCK) as u32;
        let block_end = self
            .char_bytes
            .get(block)
            .copied()
            .unwrap_or(self.text_byte_end);
        // A block of one-byte characters (nearly all of them) needs no scan.
        if block_end - base == block_len {
            return base + (index - block_start) as u32;
        }
        characters[block_start..index]
            .iter()
            .fold(base, |byte, c| byte + c.flags.len_utf8() as u32)
    }

    #[inline]
    pub(crate) fn char_byte_range(&self, characters: &[Character], index: usize) -> Range<usize> {
        let start = self.char_byte_start(characters, index) as usize;
        start..start + characters[index].flags.len_utf8()
    }
}

/// Properties of a [`Character`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CharacterFlags(u8);

impl CharacterFlags {
    /// Bits 0..2: the UTF-8 length of the source character minus one.
    ///
    /// (Note the UTF-8 length of any character is between 1 and 4 inclusive.)
    const LEN_UTF8_MASK: u8 = 0b11;
    /// Whether the source character is an emoji.
    const EMOJI: u8 = 1 << 2;
    /// Whether there is a word boundary before the character.
    const WORD_BOUNDARY: u8 = 1 << 3;
    /// Whether there is a soft wrap opportunity before the character.
    const SOFT_WRAP_OPPORTUNITY: u8 = 1 << 4;
    /// Whether the character begins a grapheme cluster. A flag rather than a `Character` field
    /// so a character packs into 8 bytes.
    const GRAPHEME_START: u8 = 1 << 5;

    #[inline(always)]
    pub fn new(source_char: char, is_word_boundary: bool, is_soft_wrap_opportunity: bool) -> Self {
        // TODO: Defer to ICU4X properties (see: https://docs.rs/icu/latest/icu/properties/props/struct.Emoji.html).
        let is_emoji = matches!(source_char as u32, 0x1F600..=0x1F64F | 0x1F300..=0x1F5FF | 0x1F680..=0x1F6FF | 0x2600..=0x26FF | 0x2700..=0x27BF);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "`len_utf8` is between 1 and 4 inclusive"
        )]
        let len_utf8 = source_char.len_utf8() as u8;
        Self(
            (len_utf8 - 1)
                | if is_emoji { Self::EMOJI } else { 0 }
                | if is_word_boundary {
                    Self::WORD_BOUNDARY
                } else {
                    0
                }
                | if is_soft_wrap_opportunity {
                    Self::SOFT_WRAP_OPPORTUNITY
                } else {
                    0
                },
        )
    }

    /// Whether there is a soft wrap opportunity before this character ([UAX #14][line-breaking]).
    ///
    /// Note mandatory breaks (like `\n`) are encoded as [`Whitespace`].
    ///
    /// [line-breaking]: https://www.unicode.org/reports/tr14/
    #[inline(always)]
    pub fn is_soft_wrap_opportunity(self) -> bool {
        self.0 & Self::SOFT_WRAP_OPPORTUNITY != 0
    }

    /// Returns if there is a word boundary before the character.
    #[inline(always)]
    pub fn is_word_boundary(self) -> bool {
        self.0 & Self::WORD_BOUNDARY != 0
    }

    /// Returns if the character is an emoji.
    #[inline(always)]
    pub fn is_emoji(self) -> bool {
        self.0 & Self::EMOJI != 0
    }

    /// Returns if the character begins a grapheme cluster.
    #[inline(always)]
    pub fn is_grapheme_start(self) -> bool {
        self.0 & Self::GRAPHEME_START != 0
    }

    /// Set whether the character begins a grapheme cluster.
    #[inline(always)]
    #[must_use]
    pub const fn with_grapheme_start(self, set: bool) -> Self {
        if set {
            Self(self.0 | Self::GRAPHEME_START)
        } else {
            Self(self.0 & !Self::GRAPHEME_START)
        }
    }

    /// Returns the number of bytes the source character would need if encoded in UTF-8.
    ///
    /// That number of bytes is always between 1 and 4, inclusive.
    #[inline(always)]
    pub fn len_utf8(self) -> usize {
        (self.0 & Self::LEN_UTF8_MASK) as usize + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn character_flags() {
        for (ch, len, emoji) in [
            ('a', 1, false),
            (' ', 1, false),
            ('\t', 1, false),
            ('\r', 1, false),
            ('\n', 1, false),
            ('\u{00e9}', 2, false),
            ('\u{2028}', 3, false),
            ('\u{2600}', 3, true),
            ('\u{1F600}', 4, true),
        ] {
            for (is_word_boundary, is_soft_wrap_opportunity) in
                [(false, false), (true, false), (false, true), (true, true)]
            {
                let flags = CharacterFlags::new(ch, is_word_boundary, is_soft_wrap_opportunity);
                assert_eq!(flags.len_utf8(), len, "{ch:?}");
                assert_eq!(flags.is_emoji(), emoji, "{ch:?}");
                assert_eq!(flags.is_word_boundary(), is_word_boundary, "{ch:?}");
                assert_eq!(
                    flags.is_soft_wrap_opportunity(),
                    is_soft_wrap_opportunity,
                    "{ch:?}"
                );
            }

            let character = Character {
                style_index: 7,
                whitespace: Whitespace::from_char(ch),
                flags: CharacterFlags::new(ch, false, false).with_grapheme_start(true),
            };
            assert!(character.is_grapheme_start(), "{ch:?}");
            assert_eq!(character.flags.len_utf8(), len, "{ch:?}");
            assert_eq!(character.flags.is_emoji(), emoji, "{ch:?}");
        }
    }

    fn character(ch: char) -> Character {
        Character {
            style_index: 0,
            whitespace: Whitespace::from_char(ch),
            flags: CharacterFlags::new(ch, false, false),
        }
    }

    fn cluster() -> ShapedCluster {
        ShapedCluster {
            advance: 1.,
            glyph_offset: 0,
            flags: ShapedClusterFlags::new(1),
            chars_delta: 0,
        }
    }

    /// Pushes `text`'s characters starting at byte `start` and checks every offset.
    fn check_chars(index: &mut ShapedIndex, chars: &mut Vec<Character>, text: &str, start: u32) {
        let first = chars.len();
        let mut expected = Vec::new();
        for (offset, ch) in text.char_indices() {
            let byte = start + offset as u32;
            expected.push(byte as usize..byte as usize + ch.len_utf8());
            index.push_char(chars, character(ch), byte);
        }
        for (i, range) in expected.into_iter().enumerate() {
            assert_eq!(
                index.char_byte_range(chars, first + i),
                range,
                "char {}",
                first + i
            );
        }
    }

    #[test]
    fn char_byte_offsets_across_blocks() {
        let mut index = ShapedIndex::default();
        let mut chars = Vec::new();
        let text = "a".repeat(40) + &"é☀😀b".repeat(20) + &"c".repeat(70);
        check_chars(&mut index, &mut chars, &text, 5);
        assert_eq!(index.char_mode, CharBytes::Sparse);
        assert_eq!(index.char_bytes.len(), chars.len().div_ceil(CHAR_BLOCK) - 1);
    }

    #[test]
    fn one_byte_char_offsets_store_no_checkpoints() {
        let mut index = ShapedIndex::default();
        let mut chars = Vec::new();
        check_chars(&mut index, &mut chars, &"a b\t".repeat(50), 9);
        assert_eq!(index.char_mode, CharBytes::OneByte);
        assert!(index.char_bytes.is_empty());
        // A first multi-byte character checkpoints the one-byte blocks before it.
        check_chars(&mut index, &mut chars, "é☀z", 209);
        assert_eq!(index.char_mode, CharBytes::Sparse);
        for i in 0..200 {
            assert_eq!(index.char_byte_start(&chars, i), 9 + i as u32);
        }
    }

    #[test]
    fn char_byte_offsets_fall_back_to_dense_on_a_gap() {
        let mut index = ShapedIndex::default();
        let mut chars = Vec::new();
        check_chars(&mut index, &mut chars, &"aé".repeat(30), 0);
        let end = index.text_byte_end;
        check_chars(&mut index, &mut chars, "x😀y", end + 7);
        assert_eq!(index.char_mode, CharBytes::Dense);
        let mut offset = 0;
        for (i, ch) in "aé".repeat(30).chars().enumerate() {
            assert_eq!(index.char_byte_start(&chars, i), offset);
            offset += ch.len_utf8() as u32;
        }
    }

    #[test]
    fn cluster_chars_start_across_blocks_and_overflow() {
        let mut index = ShapedIndex::default();
        let mut clusters = Vec::new();
        let mut starts = Vec::new();
        for i in 0..(1 << CLUSTER_SHIFT) + 10 {
            starts.push(i * 3);
            index.push_cluster(&mut clusters, cluster(), i * 3);
        }
        assert_eq!(index.cluster_shift, CLUSTER_SHIFT);
        assert_eq!(index.cluster_chars.len(), 1);
        let far = starts.last().unwrap() + u32::from(u16::MAX) + 1;
        starts.push(far);
        index.push_cluster(&mut clusters, cluster(), far);
        starts.push(far + 1);
        index.push_cluster(&mut clusters, cluster(), far + 1);
        assert_eq!(index.cluster_shift, 0);
        assert_eq!(index.cluster_chars.len(), clusters.len() - 1);
        for (i, start) in starts.into_iter().enumerate() {
            assert_eq!(
                index.cluster_chars_start(&clusters, i),
                start,
                "cluster {i}"
            );
        }
    }

    #[test]
    fn first_char_flags_round_trip() {
        let soft_wraps = [false, true];
        let whitespaces = [
            Whitespace::None,
            Whitespace::Space,
            Whitespace::NoBreakSpace,
            Whitespace::IdeographicSpace,
            Whitespace::OtherSpaceSeparator,
            Whitespace::Tab,
            Whitespace::Newline,
            Whitespace::ControlWhitespace,
        ];
        for soft_wrap in soft_wraps {
            for whitespace in whitespaces {
                let flags = ShapedClusterFlags::new(3)
                    .with_grapheme_start(true)
                    .with_first_char(soft_wrap, whitespace);
                assert_eq!(flags.is_soft_wrap_opportunity_before(), soft_wrap);
                assert_eq!(flags.whitespace(), whitespace);
                assert_eq!(flags.glyph_len(), 3);
                assert!(flags.is_grapheme_start());
            }
        }
    }
}
