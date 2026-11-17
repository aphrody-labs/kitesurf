use std::{
    fmt::Display,
    path::{Path, PathBuf},
    rc::Rc,
};

use boa_ast::Position;

mod builder;

use boa_gc::{Finalize, Trace};
use boa_string::JsString;
pub(crate) use builder::SourceMapBuilder;


#[cfg(test)]
mod tests;

/// Source information.
#[derive(Debug, Default, Clone, Finalize, Trace)]
// SAFETY: Nothing in Inner needs tracing, so this is safe.
#[boa_gc(unsafe_empty_trace)]
pub(crate) struct SourceInfo {
    inner: Rc<Inner>,
}

impl SourceInfo {
    pub(crate) fn new(source_map: SourceMap, function_name: JsString) -> Self {
        Self {
            inner: Rc::new(Inner {
                map: source_map,
                function_name,
            }),
        }
    }

    pub(crate) fn map(&self) -> &SourceMap {
        &self.inner.map
    }

    pub(crate) fn function_name(&self) -> &JsString {
        &self.inner.function_name
    }

}

#[derive(Debug, Default, Clone)]
struct Inner {
    map: SourceMap,
    function_name: JsString,
}

/// Bytecode to source code mapping.
///
/// Entries are delta/varint-encoded (about 3-4 bytes each instead of 12),
/// with a checkpoint every [`CHECKPOINT_EVERY`] entries so a lookup decodes few.
#[derive(Debug, Default, Clone)]
pub(crate) struct SourceMap {
    data: Box<[u8]>,
    checkpoints: Box<[Checkpoint]>,
    path: SourcePath,
}

const CHECKPOINT_EVERY: usize = 32;

impl SourceMap {
    /// Kitesurf: the heap bytes of the encoded entries and their checkpoints, for the code
    /// counters. The path is shared with the rest of the script, so it is left out.
    pub(crate) fn ks_heap_bytes(&self) -> usize {
        self.data.len() + size_of_val::<[Checkpoint]>(&self.checkpoints)
    }

    /// Kitesurf: the encoded entries and checkpoints (`pc, offset, line, column`), for an
    /// embedder that moves the map out of memory; [`Self::ks_from_parts`] rebuilds it.
    pub(crate) fn ks_parts(&self) -> (&[u8], impl Iterator<Item = [u32; 4]> + '_) {
        let checkpoints = self.checkpoints.iter().map(|c| [c.pc, c.offset, c.line, c.column]);
        (&self.data, checkpoints)
    }

    /// Kitesurf: the map [`Self::ks_parts`] read, rebuilt for `path`, which every block of
    /// a script shares and so is not stored with each.
    pub(crate) fn ks_from_parts(data: Box<[u8]>, checkpoints: &[[u32; 4]], path: SourcePath) -> Self {
        let checkpoints = checkpoints
            .iter()
            .map(|&[pc, offset, line, column]| Checkpoint { pc, offset, line, column })
            .collect();
        Self { data, checkpoints, path }
    }
}

/// Decoder state at the start of an entry: its pc, byte offset, and the last
/// line/column seen before it (the bases its deltas are relative to).
#[derive(Debug, Clone, Copy)]
struct Checkpoint {
    pc: u32,
    offset: u32,
    line: u32,
    column: u32,
}

fn write_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn read_varint(data: &[u8], pos: &mut usize) -> u32 {
    let mut v = 0u32;
    let mut shift = 0;
    loop {
        let b = data[*pos];
        *pos += 1;
        v |= u32::from(b & 0x7f) << shift;
        if b < 0x80 {
            return v;
        }
        shift += 7;
    }
}

const fn zigzag(v: i64) -> u32 {
    ((v << 1) ^ (v >> 63)) as u32
}

const fn unzigzag(v: u32) -> i64 {
    (v >> 1) as i64 ^ -((v & 1) as i64)
}

/// Streams the entries of a [`SourceMap`] from a checkpoint.
struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
    pc: u32,
    line: u32,
    column: u32,
}

impl Decoder<'_> {
    fn next(&mut self) -> Option<Entry> {
        if self.pos >= self.data.len() {
            return None;
        }
        self.pc += read_varint(self.data, &mut self.pos);
        let line = read_varint(self.data, &mut self.pos);
        let position = if line == 0 {
            None
        } else {
            self.line = (i64::from(self.line) + unzigzag(line - 1)) as u32;
            let column = read_varint(self.data, &mut self.pos);
            self.column = (i64::from(self.column) + unzigzag(column)) as u32;
            Some(Position::new(self.line, self.column))
        };
        Some(Entry { pc: self.pc, position })
    }
}

impl SourceMap {
    pub(crate) fn new(entries: Box<[Entry]>, path: SourcePath) -> Self {
        let mut data = Vec::new();
        let mut checkpoints = Vec::with_capacity(entries.len().div_ceil(CHECKPOINT_EVERY));
        let (mut pc, mut line, mut column) = (0u32, 0u32, 0u32);
        for (i, entry) in entries.iter().enumerate() {
            if i % CHECKPOINT_EVERY == 0 {
                checkpoints.push(Checkpoint {
                    pc: entry.pc,
                    offset: data.len() as u32,
                    line,
                    column,
                });
                // The checkpoint already holds the pc, so the delta restarts from it.
                pc = entry.pc;
            }
            write_varint(&mut data, entry.pc - pc);
            pc = entry.pc;
            match entry.position {
                None => write_varint(&mut data, 0),
                Some(p) => {
                    write_varint(&mut data, zigzag(i64::from(p.line_number()) - i64::from(line)) + 1);
                    write_varint(&mut data, zigzag(i64::from(p.column_number()) - i64::from(column)));
                    line = p.line_number();
                    column = p.column_number();
                }
            }
        }
        Self {
            data: data.into_boxed_slice(),
            checkpoints: checkpoints.into_boxed_slice(),
            path,
        }
    }

    fn decoder(&self, checkpoint: &Checkpoint) -> Decoder<'_> {
        Decoder {
            data: &self.data,
            pos: checkpoint.offset as usize,
            pc: checkpoint.pc,
            line: checkpoint.line,
            column: checkpoint.column,
        }
    }

    /// Every entry, decoded (only the code block printer wants them all).
    pub(crate) fn entries(&self) -> Vec<Entry> {
        let Some(first) = self.checkpoints.first() else {
            return Vec::new();
        };
        let mut decoder = self.decoder(first);
        let mut out = Vec::new();
        let mut i = 0;
        while let Some(mut entry) = decoder.next() {
            if i % CHECKPOINT_EVERY == 0 {
                entry.pc = self.checkpoints[i / CHECKPOINT_EVERY].pc;
                decoder.pc = entry.pc;
            }
            out.push(entry);
            i += 1;
        }
        out
    }

    pub(crate) fn find(&self, pc: u32) -> Option<Position> {
        let first = self.checkpoints.first()?;
        if pc < first.pc {
            return None;
        }
        let index = self.checkpoints.partition_point(|c| c.pc <= pc) - 1;
        let checkpoint = &self.checkpoints[index];
        let end = self
            .checkpoints
            .get(index + 1)
            .map_or(self.data.len(), |c| c.offset as usize);
        let mut decoder = self.decoder(checkpoint);
        decoder.data = &self.data[..end];
        let mut current = decoder.next()?;
        while let Some(next) = decoder.next() {
            if pc < next.pc {
                return current.position;
            }
            current = next;
        }
        // The last entry of a block runs up to the next checkpoint, which is past `pc`.
        current.position
    }

    pub(crate) fn path(&self) -> &SourcePath {
        &self.path
    }
}

#[cfg(test)]
fn find_entry(entries: &[Entry], pc: u32) -> Option<Position> {
    let first = entries.first()?;

    if pc < first.pc() {
        return None;
    }

    let mut low = 0;
    let mut high = entries.len() - 1;

    while low <= high {
        let mid = low.midpoint(high);
        let entry = &entries[mid];
        let start = entry.pc;

        let end = entries.get(mid + 1).map_or(u32::MAX, |entry| entry.pc);

        if pc < start {
            high = mid;
        } else if pc >= end {
            low = mid + 1;
        } else {
            return entry.position();
        }
    }

    // Since the last element defines the start of the end of the range,
    // therefore we return the last element's position.
    entries.last().and_then(Entry::position)
}

// TODO: The line number increments slower than column,
//       maybe we can take advantage of this, for memory optimization?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    /// Represents the start of a bytecode range that falls under the given position.
    ///
    /// The end of the range is the pc of the next entry.
    /// If the entry is the last, the end of the range is [`u32::MAX`].
    pub(crate) pc: u32,

    /// Source code [`Position`].
    pub(crate) position: Option<Position>,
}

#[cfg(test)]
impl Entry {
    pub(crate) const fn pc(&self) -> u32 {
        self.pc
    }

    pub(crate) const fn position(&self) -> Option<Position> {
        self.position
    }
}

/// The Path and type of [`boa_engine::Source`]. This applies to functions and objects.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum SourcePath {
    /// There was no source associated with this call.
    #[default]
    None,
    /// The source is from an `eval()` call.
    // TODO: Could add more information, like path in which the eval is located.
    Eval,
    /// The source is from a `JSON.parse()` call.
    // TODO: Could add more information, like path in which the JSON.parse is located.
    Json,
    /// The source is from a file or module. This contains the [`Path`] of the
    /// module (e.g. the absolute file path).
    Path(Rc<Path>),
}

impl From<Option<PathBuf>> for SourcePath {
    fn from(value: Option<PathBuf>) -> Self {
        match value {
            None => Self::None,
            Some(path) => Self::Path(path.into()),
        }
    }
}

impl From<Option<Rc<Path>>> for SourcePath {
    fn from(value: Option<Rc<Path>>) -> Self {
        match value {
            None => Self::None,
            Some(path) => Self::Path(path),
        }
    }
}

impl Display for SourcePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourcePath::None => f.write_str("unknown at "),
            SourcePath::Eval => f.write_str("eval at "),
            SourcePath::Json => f.write_str("json at "),
            SourcePath::Path(path) => write!(f, "{}", path.display()),
        }
    }
}

/// A struct containing information about native source code.
///
/// # Note
///
/// If the `native-backtrace` feature is not enabled the this becomes [zero sized type][zst].
///
/// [zst]: https://doc.rust-lang.org/nomicon/exotic-sizes.html#zero-sized-types-zsts
#[derive(Debug, Clone, Copy)]
pub struct NativeSourceInfo {
    #[cfg(feature = "native-backtrace")]
    inner: &'static std::panic::Location<'static>,

    #[cfg(not(feature = "native-backtrace"))]
    inner: std::marker::PhantomData<()>,
}

impl NativeSourceInfo {
    /// Returns the source location of the caller of this function.
    ///
    /// If that function’s caller is annotated with `#[track_caller]`, then its call
    /// location will be returned, and so on up the stack to the first call within
    /// a non-tracked function body.
    #[inline]
    #[must_use]
    #[cfg_attr(feature = "native-backtrace", track_caller)]
    pub const fn caller() -> Self {
        Self {
            #[cfg(feature = "native-backtrace")]
            inner: std::panic::Location::caller(),

            #[cfg(not(feature = "native-backtrace"))]
            inner: std::marker::PhantomData,
        }
    }

    /// Return a [`std::panic::Location`].
    ///
    /// # Note
    ///
    /// If the `native-backtrace` feature is not enabled, then this always returns [`None`].
    #[inline]
    #[must_use]
    pub const fn as_location(self) -> Option<&'static std::panic::Location<'static>> {
        #[cfg(feature = "native-backtrace")]
        return Some(self.inner);

        #[cfg(not(feature = "native-backtrace"))]
        return None;
    }
}
