#![allow(clippy::inline_always)]
#![allow(clippy::doc_markdown)]
//! Boa's VM opcode definitions and bytecode dispatch.
//!
//! The bytecompiler lowers ECMAScript runtime semantics into these opcode families, which are then
//! executed by the virtual machine. Most opcode submodules mirror a part of the specification,
//! such as function calls, environment records, iteration, property access, or class evaluation.
//! See the general runtime semantics overview in the [specification][spec].
//!
//! When the specification marks an abstract operation as infallible with `!`, Boa treats the
//! corresponding condition as an internal bytecode invariant. In those cases, malformed
//! bytecode is treated as an engine bug and may trip an assertion instead of returning a
//! recoverable JavaScript exception.
//!
//! [spec]: https://tc39.es/ecma262/#sec-runtime-semantics-evaluation
use crate::{
    Context,
    vm::{completion_record::CompletionRecord, completion_record::IntoCompletionRecord},
};
use args::{Argument, read};

// Which side of a property access failed on a null/undefined base, so
// the message can say "read" or "set" the way V8's does.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Access {
    Read,
    Write,
}

// V8-shaped "cannot read properties of undefined (reading 'k')". The key
// is the one token that locates the access in a minified bundle; throw path only.
#[cold]
#[inline(never)]
pub(crate) fn null_base_error(value: &crate::JsValue, access: Access, key: &str) -> crate::JsError {
    let base = if value.is_null() { "null" } else { "undefined" };
    let (verb, gerund) = match access {
        Access::Read => ("read", "reading"),
        Access::Write => ("set", "setting"),
    };
    crate::JsNativeError::typ()
        .with_message(format!(
            "cannot {verb} properties of {base} ({gerund} '{key}')"
        ))
        .into()
}

// Describe a computed key without running user code (no getters or
// toString during error construction): primitives by value, the rest by type.
#[cold]
#[inline(never)]
pub(crate) fn describe_key(key: &crate::JsValue) -> String {
    use crate::value::JsVariant;
    match key.variant() {
        JsVariant::String(s) => s.to_std_string_escaped(),
        JsVariant::Integer32(i) => i.to_string(),
        JsVariant::Float64(r) => r.to_string(),
        JsVariant::Boolean(b) => b.to_string(),
        JsVariant::Null => "null".to_string(),
        JsVariant::Undefined => "undefined".to_string(),
        _ => format!("<{}>", key.type_of()),
    }
}

// The named-key error for a by-name access. The key comes from the
// inline cache, which holds the property name for every by-name site.
#[cold]
#[inline(never)]
pub(crate) fn null_base_error_by_name(
    value: &crate::JsValue,
    access: Access,
    ic_index: usize,
    context: &Context,
) -> crate::JsError {
    let name = context.vm.frame().code_block().body().ic_names[ic_index].to_std_string_escaped();
    null_base_error(value, access, &name)
}
use std::cell::{Cell, UnsafeCell};
use std::ops::ControlFlow;
use thin_vec::ThinVec;

mod args;

// Operation modules
mod arguments;
mod r#await;
mod binary_ops;
mod call;
mod concat;
mod control_flow;
mod copy;
mod define;
mod delete;
mod environment;
mod function;
mod generator;
mod get;
mod iteration;
mod meta;
mod new;
mod nop;
mod object;
mod pop;
mod push;
mod rest_parameter;
mod set;
mod switch;
mod templates;
mod to;
mod unary_ops;
mod value;

// Operation structs
#[doc(inline)]
pub(crate) use arguments::*;
#[doc(inline)]
pub(crate) use r#await::*;
#[doc(inline)]
pub(crate) use binary_ops::*;
#[doc(inline)]
pub(crate) use call::*;
#[doc(inline)]
pub(crate) use concat::*;
#[doc(inline)]
pub(crate) use control_flow::*;
#[doc(inline)]
pub(crate) use copy::*;
#[doc(inline)]
pub(crate) use define::*;
#[doc(inline)]
pub(crate) use delete::*;
#[doc(inline)]
pub(crate) use environment::*;
#[doc(inline)]
pub(crate) use function::*;
#[doc(inline)]
pub(crate) use generator::*;
#[doc(inline)]
pub(crate) use get::*;
#[doc(inline)]
pub(crate) use iteration::*;
#[doc(inline)]
pub(crate) use meta::*;
#[doc(inline)]
pub(crate) use new::*;
#[doc(inline)]
pub(crate) use nop::*;
#[doc(inline)]
pub(crate) use object::*;
#[doc(inline)]
pub(crate) use pop::*;
#[doc(inline)]
pub(crate) use push::*;
#[doc(inline)]
pub(crate) use rest_parameter::*;
#[doc(inline)]
pub(crate) use set::*;
#[doc(inline)]
pub(crate) use switch::*;
#[doc(inline)]
pub(crate) use templates::*;
#[doc(inline)]
pub(crate) use to::*;
#[doc(inline)]
pub(crate) use unary_ops::*;
#[doc(inline)]
pub(crate) use value::*;

/// Specific opcodes for bindings.
///
/// This separate enum exists to make matching exhaustive where needed.
#[derive(Clone, Copy, Debug)]
pub(crate) enum BindingOpcode {
    Var,
    InitVar,
    InitLexical,
    SetName,
}

/// The `Operation` trait implements the execution code along with the
/// identifying Name and Instruction value for an Boa Opcode.
///
/// This trait should be implemented for a struct that corresponds with
/// any arm of the `OpCode` enum.
pub(crate) trait Operation {
    const NAME: &'static str;
    #[allow(unused)] // TODO: need to double check usage.
    const INSTRUCTION: &'static str;
    const COST: u8;
}

/// The compile time representation of bytecode instructions.
#[derive(Debug)]
pub(crate) struct BytecodeEmitter {
    bytes: Vec<u8>,
}

impl BytecodeEmitter {
    /// Create a new [`BytecodeEmitter`] instance.
    pub(crate) fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Convert the [`BytecodeEmitter`] into a [`Bytecode`] instance.
    pub(crate) fn into_bytecode(self) -> Bytecode {
        Bytecode::pack(&self.bytes)
    }

    /// Get the location of the next opcode in the bytecode.
    pub(crate) fn next_opcode_location(&self) -> Address {
        Address::new(self.bytes.len() as u32)
    }

    /// Patch the jump instruction at the given label with the given address.
    pub(crate) fn patch_jump(&mut self, label: Address, patch: Address) {
        let pos = u32::from(label) as usize;
        let bytes = u32::from(patch).to_le_bytes();
        self.bytes[pos + 1] = bytes[0];
        self.bytes[pos + 2] = bytes[1];
        self.bytes[pos + 3] = bytes[2];
        self.bytes[pos + 4] = bytes[3];
    }

    /// Patch the jump instruction at the given label with jump table addresses.
    pub(crate) fn patch_jump_table(&mut self, label: Address, patch: &[Address]) {
        let length_offset = u32::from(label) as usize + 1;

        let (length, first_offset) = read::<u32>(&self.bytes, length_offset);
        assert_eq!(length as usize, patch.len());

        // Write patched address values.
        for (i, value) in patch.iter().enumerate() {
            let offset = first_offset + i * size_of::<u32>();
            self.bytes[offset..offset + size_of::<u32>()]
                .copy_from_slice(&u32::from(*value).to_le_bytes());
        }
    }
}

/// A codeblock's bytecode, kept packed (u32 operands as varints) until it first runs,
/// which most functions of a bundle never do; it is then expanded once to execute.
pub(crate) struct Bytecode {
    bytes: UnsafeCell<Box<[u8]>>,
    packed: Cell<Packed>,
    // Kitesurf: the executable length in the low 24 bits; the high 8 count the frames and
    // readers on the bytes (saturating, then sticky), which must be zero to drop them.
    len_pins: Cell<u32>,
}

// Kitesurf: where the packed form is. Swapped keeps a 12-byte descriptor in a 16-byte chunk,
// so a block swaps only when its packed form is bigger than that.
pub(crate) enum Packed {
    Resident(Box<[u8]>),
    Swapped(Box<SwapDesc>),
}

impl Default for Packed {
    fn default() -> Self {
        Self::Resident(Box::default())
    }
}

// Kitesurf: a never-run block's bytecode as a body record carries it: its place in the
// store, or its packed bytes when they stay resident (too small, or the store refused).
pub(crate) enum KsBytecodeExport {
    Desc { key: u32, len: u32, hash: u32 },
    Inline(Box<[u8]>),
}

// Kitesurf: a swapped packed form's place in the code store and the hash it must read back
// with: registers are read unchecked, so bytes that differ must never run.
pub(crate) struct SwapDesc {
    key: u32,
    len: u32,
    hash: u32,
}

const LEN_MASK: u32 = 0x00FF_FFFF;
const PIN: u32 = 0x0100_0000;
const PINS_STICKY: u32 = 0xFF;
// Kitesurf: a packed form this small frees less than its descriptor costs, so it stays
// resident; most blocks are well over it.
const MIN_SWAP_BYTES: usize = 24;

fn fnv1a(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0x811c_9dc5_u32, |h, &b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193))
}

impl Default for Bytecode {
    fn default() -> Self {
        Self {
            bytes: UnsafeCell::default(),
            packed: Cell::default(),
            len_pins: Cell::new(0),
        }
    }
}

impl Clone for Bytecode {
    fn clone(&self) -> Self {
        // Kitesurf: a clone gets its own resident packed form, since two owners of one store
        // key would release it twice.
        let packed = match self.peek_packed() {
            Packed::Resident(p) => p.clone(),
            Packed::Swapped(d) => self.read_swapped(d).into_boxed_slice(),
        };
        // No frame is on the clone yet, but a block that had to stay pinned (past 16 MiB, or
        // saturated) still does, since its count is lost.
        let sticky = if self.pins() == PINS_STICKY { PINS_STICKY << 24 } else { 0 };
        let clone = Self {
            // SAFETY: a shared read of the box, which no forced round can drop meanwhile:
            // cloning allocates outside any round, and rounds never nest.
            bytes: UnsafeCell::new(unsafe { (&*self.bytes.get()).clone() }),
            packed: Cell::new(Packed::Resident(packed)),
            len_pins: Cell::new(self.len_pins.get() & LEN_MASK | sticky),
        };
        clone.charge(1);
        clone
    }
}

impl Drop for Bytecode {
    fn drop(&mut self) {
        self.charge(-1);
        let addr = std::ptr::from_ref::<Self>(self) as usize;
        if let Packed::Swapped(d) = self.packed.get_mut() {
            crate::ks_swap::forget_flushable(addr);
            if let Some(store) = crate::ks_code_store::current() {
                store.release(d.key, d.len as usize);
            }
        }
    }
}

impl std::fmt::Debug for Bytecode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bytecode")
            .field("len", &self.len())
            .field("packed", &self.is_packed())
            .finish()
    }
}

// Kitesurf: unpins on drop, so a reader that returns early or unwinds still lets go of the
// block, and a forced round can drop its bytes again.
pub(crate) struct PinGuard<'a>(&'a Bytecode);

impl Drop for PinGuard<'_> {
    fn drop(&mut self) {
        self.0.unpin();
    }
}

impl Bytecode {
    fn pack(bytes: &[u8]) -> Self {
        let len = bytes.len();
        // Kitesurf: the length shares a word with the pins, so a block past 16 MiB stays
        // expanded and permanently pinned: it is never packed, swapped or dropped.
        let bytecode = if len > LEN_MASK as usize {
            Self {
                bytes: UnsafeCell::new(bytes.into()),
                packed: Cell::default(),
                len_pins: Cell::new(PINS_STICKY << 24),
            }
        } else {
            Self {
                bytes: UnsafeCell::default(),
                packed: Cell::new(Packed::Resident(Self::pack_bytes(bytes))),
                len_pins: Cell::new(len as u32),
            }
        };
        bytecode.charge(1);
        bytecode
    }

    fn pack_bytes(bytes: &[u8]) -> Box<[u8]> {
        let mut packed = Vec::with_capacity(bytes.len() / 2);
        let mut pc = 0;
        while pc < bytes.len() {
            packed.push(bytes[pc]);
            pc = Self::pack_operands(Opcode::decode(bytes[pc]), bytes, pc + 1, &mut packed);
        }
        packed.into_boxed_slice()
    }

    fn unpack_bytes(&self, packed: &[u8]) -> Box<[u8]> {
        let mut bytes = Vec::with_capacity(self.exec_len());
        let mut pos = 0;
        while pos < packed.len() {
            bytes.push(packed[pos]);
            pos = Self::unpack_operands(Opcode::decode(packed[pos]), packed, pos + 1, &mut bytes);
        }
        bytes.into_boxed_slice()
    }

    // Kitesurf: the packed state, read in place. Only `expand`, the swap rounds, clone and
    // drop replace it, and none of them runs while a caller holds what this returns.
    fn peek_packed(&self) -> &Packed {
        // SAFETY: see above; every replacement goes through `Cell::take`/`set` on this block,
        // and no caller of this keeps the reference across one.
        unsafe { &*self.packed.as_ptr() }
    }

    // Kitesurf: the swapped packed form's bytes, checked; the swap rounds keep this
    // consistent, so a mismatch is a broken store, and running anything else is unsound.
    fn read_swapped(&self, d: &SwapDesc) -> Vec<u8> {
        let mut out = Vec::new();
        let ok = crate::ks_code_store::current().is_some_and(|store| {
            store.read(d.key, 0, d.len as usize, &mut out)
                && out.len() == d.len as usize
                && fnv1a(&out) == d.hash
        });
        if !ok {
            crate::ks_code_stats::update(|s| s.store_failures += 1);
            panic!("kitesurf: swapped bytecode could not be read back intact");
        }
        crate::ks_code_stats::update(|s| s.bytecode_reads += 1);
        out
    }

    // Kitesurf: adds (sign 1) or refunds (-1) this bytecode's bytes to the code counters,
    // under the forms it holds now: resident or swapped packed, and expanded once it ran.
    fn charge(&self, sign: i64) {
        let (packed, swapped) = match self.peek_packed() {
            Packed::Resident(p) => (p.len(), None),
            Packed::Swapped(d) => (0, Some(d.len as usize)),
        };
        // SAFETY: a shared read of the box's length only, taken and dropped before anything
        // else can run.
        let expanded = crate::ks_code_stats::bytes(unsafe { (&*self.bytes.get()).len() });
        crate::ks_code_stats::update(|s| {
            s.packed_bytes += sign * crate::ks_code_stats::bytes(packed);
            if let Some(n) = swapped {
                s.swapped_blocks += sign;
                s.swapped_bytes += sign * crate::ks_code_stats::bytes(n);
            }
            s.expanded_bytes += sign * expanded;
            s.blocks_run += sign * i64::from(expanded != 0);
        });
    }

    fn exec_len(&self) -> usize {
        (self.len_pins.get() & LEN_MASK) as usize
    }

    fn pins(&self) -> u32 {
        self.len_pins.get() >> 24
    }

    /// Kitesurf: a frame or a reader is on this block's executable bytes; a forced round
    /// leaves pinned bytes alone. Saturates and then stays pinned for good.
    #[inline]
    pub(crate) fn pin(&self) {
        if self.pins() < PINS_STICKY {
            self.len_pins.set(self.len_pins.get() + PIN);
        }
    }

    /// Kitesurf: undoes one `pin`; a block whose count saturated stays pinned, since what it
    /// counted is no longer known.
    #[inline]
    pub(crate) fn unpin(&self) {
        let pins = self.pins();
        if pins != 0 && pins < PINS_STICKY {
            self.len_pins.set(self.len_pins.get() - PIN);
        }
    }

    /// Kitesurf: pins the block for as long as the returned guard lives: what a reader of
    /// the executable bytes outside the dispatch loop holds while it decodes them.
    pub(crate) fn pin_guard(&self) -> PinGuard<'_> {
        self.pin();
        PinGuard(self)
    }

    /// Whether [`Self::executable`] is still empty because the bytecode has not been expanded.
    /// An empty code block never is: there is nothing to pack.
    #[inline]
    pub(crate) fn is_packed(&self) -> bool {
        // SAFETY: a shared read of the box, whose slice is checked for emptiness and not
        // kept past this expression.
        self.exec_len() != 0 && unsafe { (&*self.bytes.get()).is_empty() }
    }

    /// The executable bytecode, expanding the packed form the first time it is asked for.
    /// Callers outside the dispatch loop hold a `pin_guard` while they use the slice.
    #[inline]
    pub(crate) fn bytes(&self) -> &[u8] {
        if self.is_packed() {
            self.expand();
        }
        // SAFETY: expanded above; only a forced round drops the box, and never a pinned one,
        // which this block is for any caller that holds a pin guard.
        unsafe { &*self.bytes.get() }
    }

    /// Safety: empty while [`Self::is_packed`], and the slice must not outlive a call to
    /// [`Self::expand`], nor a forced round unless the block is pinned (frames pin theirs).
    #[inline(always)]
    pub(crate) unsafe fn executable(&self) -> &[u8] {
        // SAFETY: guaranteed by the caller.
        unsafe { &*self.bytes.get() }
    }

    /// Expands the packed bytecode into [`Self::executable`], fetching it from the code store
    /// when it was swapped; a no-op while expanded. The store copy stays, so it can again.
    #[cold]
    #[inline(never)]
    pub(crate) fn expand(&self) {
        if !self.is_packed() {
            return;
        }
        let bytes = match self.packed.take() {
            Packed::Resident(p) if !p.is_empty() => {
                let bytes = self.unpack_bytes(&p);
                let n = p.len();
                crate::ks_code_stats::update(|s| s.packed_bytes -= crate::ks_code_stats::bytes(n));
                bytes
            }
            Packed::Swapped(d) => {
                // Kitesurf: back in its cell before the read, so a failed one still leaves a
                // consistent block for unwinding to drop.
                let desc: *const SwapDesc = &raw const *d;
                self.packed.set(Packed::Swapped(d));
                // SAFETY: the box was just put back, and nothing replaces this block's packed
                // state before the read below returns.
                let packed = self.read_swapped(unsafe { &*desc });
                let bytes = self.unpack_bytes(&packed);
                crate::ks_swap::remember_flushable(std::ptr::from_ref(self) as usize);
                bytes
            }
            Packed::Resident(empty) => {
                self.packed.set(Packed::Resident(empty));
                panic!("kitesurf: bytecode has nothing to expand");
            }
        };
        debug_assert_eq!(bytes.len(), self.exec_len());
        let n = bytes.len();
        crate::ks_code_stats::update(|s| {
            s.expanded_bytes += crate::ks_code_stats::bytes(n);
            s.blocks_run += 1;
        });
        // SAFETY: `bytes()` expands before handing out a reference and the dispatch loop drops
        // its slice before expanding, so nothing borrows the empty box being replaced.
        unsafe { *self.bytes.get() = bytes };
    }

    /// Kitesurf: moves a never-run block's packed form to the store. Returns the bytes it
    /// freed; zero if the block ran, is small, or the store refused.
    pub(crate) fn swap_packed(&self, store: &dyn crate::CodeStore) -> usize {
        if !self.is_packed() {
            return 0;
        }
        let packed = match self.packed.take() {
            Packed::Resident(p) if p.len() > MIN_SWAP_BYTES => p,
            other => {
                self.packed.set(other);
                return 0;
            }
        };
        let hash = fnv1a(&packed);
        let Some(key) = store.put(&packed) else {
            self.packed.set(Packed::Resident(packed));
            return 0;
        };
        let len = packed.len();
        drop(packed);
        self.packed.set(Packed::Swapped(Box::new(SwapDesc {
            key,
            len: len as u32,
            hash,
        })));
        let n = crate::ks_code_stats::bytes(len);
        crate::ks_code_stats::update(|s| {
            s.packed_bytes -= n;
            s.swapped_blocks += 1;
            s.swapped_bytes += n;
        });
        len
    }

    /// Kitesurf: what a body record needs to rebuild this never-run bytecode, and its
    /// executable length; packs it to `store` first. None if it ever expanded or is pinned.
    pub(crate) fn ks_export(&self, store: &dyn crate::CodeStore) -> Option<(KsBytecodeExport, u32)> {
        let exec_len = self.exec_len() as u32;
        if exec_len == 0 {
            return Some((KsBytecodeExport::Inline(Box::default()), 0));
        }
        if self.pins() != 0 || !self.is_packed() {
            return None;
        }
        self.swap_packed(store);
        let export = match self.peek_packed() {
            Packed::Swapped(d) => KsBytecodeExport::Desc {
                key: d.key,
                len: d.len,
                hash: d.hash,
            },
            Packed::Resident(p) if !p.is_empty() => KsBytecodeExport::Inline(p.clone()),
            Packed::Resident(_) => return None,
        };
        Some((export, exec_len))
    }

    /// Kitesurf: hands this bytecode's store copy to the body record that exported it, so
    /// dropping the block does not release it; the block must never run again.
    pub(crate) fn ks_disown(&self) {
        match self.packed.take() {
            Packed::Swapped(d) => {
                let n = crate::ks_code_stats::bytes(d.len as usize);
                crate::ks_code_stats::update(|s| {
                    s.swapped_blocks -= 1;
                    s.swapped_bytes -= n;
                });
            }
            other => self.packed.set(other),
        }
    }

    /// Kitesurf: a never-run block's bytecode, rebuilt from what [`Self::ks_export`] gave; it
    /// owns the store copy from here on.
    pub(crate) fn ks_import(export: KsBytecodeExport, exec_len: u32) -> Self {
        let packed = match export {
            KsBytecodeExport::Desc { key, len, hash } => Packed::Swapped(Box::new(SwapDesc { key, len, hash })),
            KsBytecodeExport::Inline(p) => Packed::Resident(p),
        };
        let bytecode = Self {
            bytes: UnsafeCell::default(),
            packed: Cell::new(packed),
            len_pins: Cell::new(exec_len & LEN_MASK),
        };
        bytecode.charge(1);
        bytecode
    }

    /// Kitesurf: gives a block that ran before it could swap a copy in the store, so a forced
    /// round can drop its executable bytes later. Returns whether it now has one.
    pub(crate) fn store_expanded(&self, store: &dyn crate::CodeStore) -> bool {
        let expanded = !self.is_packed() && self.exec_len() != 0 && self.pins() < PINS_STICKY;
        if !expanded || !matches!(self.peek_packed(), Packed::Resident(p) if p.is_empty()) {
            return false;
        }
        // SAFETY: a shared read; only a forced round drops these bytes, and none starts inside
        // a round (idle or forced), which is the only caller.
        let packed = Self::pack_bytes(unsafe { &*self.bytes.get() });
        if packed.len() <= MIN_SWAP_BYTES {
            return false;
        }
        let hash = fnv1a(&packed);
        let Some(key) = store.put(&packed) else {
            return false;
        };
        let len = packed.len();
        drop(packed);
        self.packed.set(Packed::Swapped(Box::new(SwapDesc {
            key,
            len: len as u32,
            hash,
        })));
        crate::ks_code_stats::update(|s| {
            s.swapped_blocks += 1;
            s.swapped_bytes += crate::ks_code_stats::bytes(len);
        });
        crate::ks_swap::remember_flushable(std::ptr::from_ref(self) as usize);
        true
    }

    /// Kitesurf: drops the executable bytes of an unpinned block whose packed form is in the
    /// store; its next run fetches them back. Returns the bytes freed.
    pub(crate) fn flush_executable(&self) -> usize {
        if self.pins() != 0 || !matches!(self.peek_packed(), Packed::Swapped(_)) {
            return 0;
        }
        // SAFETY: no pin means no frame runs these bytes and no reader decodes them, so no
        // slice of the box is live while it is replaced.
        let bytes = unsafe { std::mem::take(&mut *self.bytes.get()) };
        let n = bytes.len();
        drop(bytes);
        if n != 0 {
            crate::ks_code_stats::update(|s| {
                s.expanded_bytes -= crate::ks_code_stats::bytes(n);
                s.blocks_run -= 1;
                s.flushes += 1;
            });
        }
        n
    }

    /// Length of the executable bytecode, without expanding it.
    /// The code block printer uses this.
    pub(crate) fn len(&self) -> usize {
        // SAFETY: a shared read of the box's length; a block past 16 MiB is only ever
        // expanded, so its length is always read from here.
        let expanded = unsafe { (&*self.bytes.get()).len() };
        if expanded != 0 { expanded } else { self.exec_len() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
/// An address is a bytecode offset, displayed as hexadecimal.
pub(crate) struct Address(u32);

impl Address {
    /// Create a new [`Address`] from a u32 value.
    pub(crate) const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the inner `u32` value.
    pub(crate) const fn as_u32(self) -> u32 {
        self.0
    }
}

impl From<Address> for u32 {
    fn from(addr: Address) -> Self {
        addr.0
    }
}

impl From<u32> for Address {
    fn from(value: u32) -> Self {
        Self::new(value)
    }
}

impl std::ops::Add<u32> for Address {
    type Output = Self;

    fn add(self, rhs: u32) -> Self {
        Self::new(self.0 + rhs)
    }
}

impl std::fmt::Display for Address {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:06x}", self.0)
    }
}

#[derive(Debug, Clone, Copy)]
/// A register operand is a register index used in bytecode instructions.
pub(crate) struct RegisterOperand(u32);

impl RegisterOperand {
    /// Create a new [`RegisterOperand`] from a u32 value.
    pub(crate) fn new(value: u32) -> Self {
        Self(value)
    }
}

impl From<RegisterOperand> for u32 {
    fn from(value: RegisterOperand) -> Self {
        value.0
    }
}

impl From<RegisterOperand> for usize {
    fn from(value: RegisterOperand) -> Self {
        value.0 as usize
    }
}

impl From<u8> for RegisterOperand {
    fn from(value: u8) -> Self {
        Self::new(value.into())
    }
}

impl From<u16> for RegisterOperand {
    fn from(value: u16) -> Self {
        Self::new(value.into())
    }
}

impl From<u32> for RegisterOperand {
    fn from(value: u32) -> Self {
        Self::new(value)
    }
}

impl std::fmt::Display for RegisterOperand {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "r{:02}", self.0)
    }
}

#[derive(Debug, Clone, Copy)]
/// A index operand is e.g. an index into the constant pool
pub(crate) struct IndexOperand(u32);

impl IndexOperand {
    /// Create a new [`IndexOperand`] from a u32 value.
    pub(crate) fn new(value: u32) -> Self {
        Self(value)
    }
}

impl From<IndexOperand> for u32 {
    fn from(value: IndexOperand) -> Self {
        value.0
    }
}

impl From<IndexOperand> for usize {
    fn from(value: IndexOperand) -> Self {
        value.0 as usize
    }
}

impl From<bool> for IndexOperand {
    fn from(value: bool) -> Self {
        Self::new(value.into())
    }
}

impl From<u8> for IndexOperand {
    fn from(value: u8) -> Self {
        Self::new(value.into())
    }
}

impl From<u16> for IndexOperand {
    fn from(value: u16) -> Self {
        Self::new(value.into())
    }
}

impl From<u32> for IndexOperand {
    fn from(value: u32) -> Self {
        Self::new(value)
    }
}

impl std::fmt::Display for IndexOperand {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Opcode {
    fn encode(self) -> u8 {
        self as u8
    }

    pub(crate) fn decode(instruction: u8) -> Self {
        Self::from(instruction)
    }
}

fn encode_instruction<A: Argument>(opcode: Opcode, args: A, bytes: &mut Vec<u8>) {
    bytes.push(opcode.encode());
    args.encode(bytes);
}

macro_rules! generate_opcodes {
    (
        $(
            $(#[$comment:ident $($args:tt)*])*
            $Variant:ident $({
                $(
                    $(#[$fieldinner:ident $($fieldargs:tt)*])*
                    $FieldName:ident : $FieldType:ty
                ),*
                $(,)?
            })? $(=> $mapping:ident)?
        ),*
        $(,)?
    ) => {
        /// The opcodes of the vm.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(u8)]
        pub(crate) enum Opcode {
            $(
                $(#[$comment $($args)*])*
                $Variant
            ),*
        }

        impl From<u8> for Opcode {
            #[inline]
            #[allow(non_upper_case_globals)]
            fn from(value: u8) -> Self {
                $(
                    const $Variant: u8 = Opcode::$Variant as u8;
                )*
                match value {
                    $($Variant => Self::$Variant),*
                }
            }
        }

        impl Opcode {
            pub(crate) fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$Variant => $Variant::NAME),*
                }
            }
        }

        impl BytecodeEmitter {
            $(
                pastey::paste! {
                    #[allow(unused)]
                    pub(crate) fn [<emit_ $Variant:snake>](&mut self $( $(, $FieldName: $FieldType)* )? ) {
                        encode_instruction(
                            Opcode::$Variant,
                            ($($($FieldName),*)?),
                            &mut self.bytes,
                        );
                    }
                }
            )*
        }

        // The dispatch loop hands over the running frame's bytecode, which it
        // already loaded, instead of each handler re-walking frame -> code block -> bytes.
        type OpcodeHandler = fn(&mut Context, usize, *const [u8]) -> ControlFlow<CompletionRecord>;

        pub(crate) const OPCODE_HANDLERS: [OpcodeHandler; 256] = {
            [
                $(
                    pastey::paste! { [<handle_ $Variant:snake>] },
                )*
            ]
        };

        type OpcodeHandlerBudget = fn(&mut Context, usize, *const [u8], &mut u32) -> ControlFlow<CompletionRecord>;

        pub(crate) const OPCODE_HANDLERS_BUDGET: [OpcodeHandlerBudget; 256] = {
            [
                $(
                    pastey::paste! { [<handle_ $Variant:snake _budget>] },
                )*
            ]
        };

        $(
            pastey::paste! {
                #[inline(always)]
                #[allow(unused_parens)]
                fn [<handle_ $Variant:snake>](context: &mut Context, pc: usize, bytes: *const [u8]) -> ControlFlow<CompletionRecord> {
                    // SAFETY: the running frame's bytecode; the frame keeps its code block
                    // alive until `operation` runs, and the bytes are only read before it.
                    let bytes = unsafe { &*bytes };
                    let (args, next_pc) = <($($($FieldType),*)?)>::decode(bytes, pc + 1);
                    context.vm.frame_mut().pc = next_pc as u32;
                    let result = $Variant::operation(args, context);
                    IntoCompletionRecord::into_completion_record(result, context)
                }
            }
        )*

        $(
            pastey::paste! {
                #[inline(always)]
                #[allow(unused_parens)]
                fn [<handle_ $Variant:snake _budget>](context: &mut Context, pc: usize, bytes: *const [u8], budget: &mut u32) -> ControlFlow<CompletionRecord> {
                    *budget = budget.saturating_sub(u32::from($Variant::COST));
                    // SAFETY: as in the non-budget handler above.
                    let bytes = unsafe { &*bytes };
                    let (args, next_pc) = <($($($FieldType),*)?)>::decode(bytes, pc + 1);
                    context.vm.frame_mut().pc = next_pc as u32;
                    let result = $Variant::operation(args, context);
                    IntoCompletionRecord::into_completion_record(result, context)
                }
            }
        )*

        $(
            $(
                struct $Variant {}
                impl $Variant {
                    #[allow(unused_parens)]
                    #[allow(unused_variables)]
                    #[inline(always)]
                    fn operation(args: (), context: &mut Context) {
                        $mapping::operation(args, context)
                    }
                }

                impl Operation for $Variant {
                    const NAME: &'static str = "Reserved";
                    const INSTRUCTION: &'static str = "INST - Reserved";
                    const COST: u8 = 0;
                }
            )?
        )*

        pub(crate) enum Instruction {
            $(
                $Variant $({
                    $(
                        $(#[$fieldinner $($fieldargs)*])*
                        $FieldName : $FieldType
                    ),*
                })?
            ),*
        }

        impl Bytecode {
            #[allow(unused_parens)]
            fn pack_operands(opcode: Opcode, bytes: &[u8], pos: usize, out: &mut Vec<u8>) -> usize {
                match opcode {
                    $( Opcode::$Variant => <($($($FieldType),*)?)>::pack(bytes, pos, out), )*
                }
            }

            #[allow(unused_parens)]
            fn unpack_operands(opcode: Opcode, packed: &[u8], pos: usize, out: &mut Vec<u8>) -> usize {
                match opcode {
                    $( Opcode::$Variant => <($($($FieldType),*)?)>::unpack(packed, pos, out), )*
                }
            }

            #[allow(unused_parens)]
            pub(crate) fn next_instruction(&self, pc: usize) -> (Instruction, usize) {
                let bytes = self.bytes();
                let opcode = Opcode::decode(bytes[pc]);

                match opcode {
                    $(
                        Opcode::$Variant => {
                            let (($($($FieldName),*)?), read_size) = <($($($FieldType),*)?)>::decode(bytes, pc + 1);
                            (Instruction::$Variant $({
                                $(
                                    $FieldName: $FieldName
                                ),*
                            })?, read_size)
                        }
                    ),*
                }
            }
        }
    }
}

/// Iterator over the instructions in the compact bytecode.
// #[derive(Debug, Clone)]
pub(crate) struct InstructionIterator<'bytecode> {
    bytes: &'bytecode Bytecode,
    pc: usize,
}

// TODO: see if this can be exposed on all features.
// #[allow(unused)]
impl<'bytecode> InstructionIterator<'bytecode> {
    /// Create a new [`InstructionIterator`] from bytecode array.
    #[inline]
    #[must_use]
    pub(crate) const fn new(bytes: &'bytecode Bytecode) -> Self {
        Self { bytes, pc: 0 }
    }

    /// Get the current program counter.
    #[inline]
    #[must_use]
    pub(crate) const fn pc(&self) -> usize {
        self.pc
    }
}

impl Iterator for InstructionIterator<'_> {
    type Item = (usize, Opcode, Instruction);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let _pin = self.bytes.pin_guard();
        let start_pc = self.pc;
        let bytes = self.bytes.bytes();
        if self.pc >= bytes.len() {
            return None;
        }

        let opcode = Opcode::decode(bytes[self.pc]);
        // Get instruction and determine how much to advance pc
        let (instruction, read_size) = self.bytes.next_instruction(self.pc);
        self.pc = read_size;
        Some((start_pc, opcode, instruction))
    }
}

generate_opcodes! {
    /// Pop the top value from the stack.
    ///
    /// - Stack: value **=>**
    Pop,

    /// Store integer `0` in dst.
    ///
    /// - Registers:
    ///   - Output: dst
    StoreZero { dst: RegisterOperand },

    /// Store integer `1` in dst.
    ///
    /// - Registers:
    ///   - Output: dst
    StoreOne { dst: RegisterOperand },

    /// Store `i8` value in dst.
    ///
    /// - Operands:
    ///   - value: `i8`
    /// - Registers:
    ///   - Output: dst
    StoreInt8 { dst: RegisterOperand, value: i8 },

    /// Store `i16` value in dst.
    ///
    /// - Operands:
    ///   - value: `i16`
    /// - Registers:
    ///   - Output: dst
    StoreInt16 { dst: RegisterOperand, value: i16 },

    /// Store `i32` value in dst.
    ///
    /// - Operands:
    ///   - value: `i32`
    /// - Registers:
    ///   - Output: dst
    StoreInt32 { dst: RegisterOperand, value: i32 },

    /// Store `f32` value in dst.
    ///
    /// - Operands:
    ///   - value: `f32`
    /// - Registers:
    ///   - Output: dst
    StoreFloat { dst: RegisterOperand, value: f32 },

    /// Store `f64` value in dst.
    ///
    /// - Operands:
    ///   - value: `f64`
    /// - Registers:
    ///   - Output: dst
    StoreDouble { dst: RegisterOperand, value: f64 },

    /// Store `NaN` in dst.
    ///
    /// - Registers:
    ///   - Output: dst
    StoreNan { dst: RegisterOperand },

    /// Store `Infinity` in dst.
    ///
    /// - Registers:
    ///   - Output: dst
    StorePositiveInfinity { dst: RegisterOperand },

    /// Store `-Infinity` in dst.
    ///
    /// - Registers:
    ///   - Output: dst
    StoreNegativeInfinity { dst: RegisterOperand },

    /// Store `null` in dst.
    ///
    /// - Registers:
    ///   - Output: dst
    StoreNull { dst: RegisterOperand },

    /// Store `true` in dst.
    ///
    /// - Registers:
    ///   - Output: dst
    StoreTrue { dst: RegisterOperand },

    /// Store `false` in dst.
    ///
    /// - Registers:
    ///   - Output: dst
    StoreFalse { dst: RegisterOperand },

    /// Store `undefined` in dst.
    ///
    /// - Registers:
    ///   - Output: dst
    StoreUndefined { dst: RegisterOperand },

    /// Store literal value in dst.
    ///
    /// Like strings and bigints. The index operand is used to index into the `literals`
    /// array to get the value.
    ///
    /// - Operands:
    ///   - index: `IndexOperand`
    /// - Registers:
    ///   - Output: dst
    StoreLiteral { dst: RegisterOperand, index: IndexOperand },

    /// Store regexp value in dst.
    ///
    /// - Operands:
    ///   - pattern_index: `IndexOperand`
    ///   - flags: `IndexOperand`
    /// - Registers:
    ///   - Output: dst
    StoreRegexp { dst: RegisterOperand, pattern_index: IndexOperand, flags_index: IndexOperand },

    /// Store empty object `{}` in dst.
    ///
    /// - Registers:
    ///   - Output: dst
    StoreEmptyObject { dst: RegisterOperand },

    /// Get the prototype of a superclass and store it in dst.
    ///
    /// Additionally this sets the `[[prototype]]` of the class and the `DERIVED` flag.
    ///
    /// - Registers:
    ///   - Input: class, superclass
    ///   - Output: dst
    StoreClassPrototype {
        dst: RegisterOperand,
        class: RegisterOperand,
        superclass: RegisterOperand
    },

    /// Set the prototype of a class object.
    ///
    /// - Registers:
    ///   - Input: class, prototype
    ///   - Output: dst
    SetClassPrototype {
        dst: RegisterOperand,
        prototype: RegisterOperand,
        class: RegisterOperand
    },

    /// Set home object internal slot of an object literal method.
    ///
    /// - Registers:
    ///   - Input: function, home
    SetHomeObject {
        function: RegisterOperand,
        home: RegisterOperand
    },

    /// Get home object internal slot of an object literal method.
    ///
    /// - Registers (inout):
    ///   - function:
    ///     - in: `JsObject<OrdinaryFunction>`.
    ///     - out: `JsObject` or `null` if the home object is not set.
    GetHomeObject {
        function: RegisterOperand,
    },

    /// Set the prototype of an object if the value is an object or null.
    ///
    /// - Registers (in):
    ///   - object: `JsObject`.
    ///   - prototype: `JsObject` or `null`
    SetPrototype {
        object: RegisterOperand,
        prototype: RegisterOperand
    },

    /// Get the prototype of an object.
    ///
    /// - Registers (inout):
    ///   - object:
    ///     - in: `JsObject`.
    ///     - out: `JsObject` or `null`.
    GetPrototype {
        object: RegisterOperand,
    },

    /// Store an empty array in dst.
    ///
    /// - Registers:
    ///   - Output: dst
    StoreNewArray { dst: RegisterOperand },

    /// Push a value to an array.
    ///
    /// - Registers:
    ///   - Input: array, value
    PushValueToArray { value: RegisterOperand, array: RegisterOperand },

    /// Push an empty element/hole to an array.
    ///
    /// - Registers:
    ///   - Input: array
    PushElisionToArray { array: RegisterOperand },

    /// Push all iterator values to an array.
    ///
    /// - Registers:
    ///   - Input: array
    PushIteratorToArray { array: RegisterOperand },

    /// Binary `+` operator.
    ///
    /// - Registers
    ///   - Input: lhs, rhs
    ///   - Output: dst
    Add { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `-` operator.
    ///
    /// - Registers
    ///   - Input: lhs, rhs
    ///   - Output: dst
    Sub { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `/` operator.
    ///
    /// - Registers
    ///   - Input: lhs, rhs
    ///   - Output: dst
    Div { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `*` operator.
    ///
    /// - Registers
    ///   - Input: lhs, rhs
    ///   - Output: dst
    Mul { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `%` operator.
    ///
    /// - Registers
    ///   - Input: lhs, rhs
    ///   - Output: dst
    Mod { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `**` operator.
    ///
    /// - Registers
    ///   - Input: lhs, rhs
    ///   - Output: dst
    Pow { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `>>` operator.
    ///
    /// - Registers
    ///   - Input: lhs, rhs
    ///   - Output: dst
    ShiftRight { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `<<` operator.
    ///
    /// - Registers:
    ///   - Input: lhs, rhs
    ///   - Output: dst
    ShiftLeft { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `>>>` operator.
    ///
    /// - Registers:
    ///   - Input: lhs, rhs
    ///   - Output: dst
    UnsignedShiftRight { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary bitwise `|` operator.
    ///
    /// - Registers:
    ///   - Input: lhs, rhs
    ///   - Output: dst
    BitOr { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary bitwise `&` operator.
    ///
    /// - Registers:
    ///   - Input: lhs, rhs
    ///   - Output: dst
    BitAnd { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary bitwise `^` operator.
    ///
    /// - Registers:
    ///   - Input: lhs, rhs
    ///   - Output: dst
    BitXor { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Unary bitwise `~` operator.
    ///
    /// - Registers:
    ///   - Input: lhs, rhs
    ///   - Output: dst
    BitNot { value: RegisterOperand },

    /// Binary `in` operator.
    ///
    /// - Registers:
    ///   - Input: lhs, rhs
    ///   - Output: dst
    In { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `in` operator for private names.
    ///
    /// - Operands: index: `u32`
    /// - Registers:
    ///   - Input: rhs
    ///   - Output: dst
    InPrivate { dst: RegisterOperand, index: IndexOperand, rhs: RegisterOperand },

    /// Binary `==` operator.
    ///
    /// - Registers:
    ///   - Input: lhs, rhs
    ///   - Output: dst
    Eq { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `===` operator.
    ///
    /// - Registers:
    ///   - Input: lhs, rhs
    ///   - Output: dst
    StrictEq { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `!=` operator.
    ///
    /// - Registers:
    ///   - Input: lhs, rhs
    ///   - Output: dst
    NotEq { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `!==` operator.
    ///
    /// - Registers:
    ///   - Input: lhs, rhs
    ///   - Output: dst
    StrictNotEq { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `>` operator.
    ///
    /// - Registers:
    ///   - Input: lhs, rhs
    ///   - Output: dst
    GreaterThan { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `>=` operator.
    ///
    /// - Registers:
    ///   - Input: lhs, rhs
    ///   - Output: dst
    GreaterThanOrEq { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `<` operator.
    ///
    /// - Registers:
    ///   - Input: lhs, rhs
    ///   - Output: dst
    LessThan { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `<=` operator.
    ///
    /// - Registers:
    ///   - Input: lhs, rhs
    ///   - Output: dst
    LessThanOrEq { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary `instanceof` operator.
    ///
    /// - Registers:
    ///   - Input: lhs, rhs
    ///   - Output: dst
    InstanceOf { dst: RegisterOperand, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Binary logical `&&` operator.
    ///
    /// This is a short-circuit operator, if the `lhs` value is `false`, then it jumps to `exit` address.
    ///
    /// - Operands:
    ///   - address: `u32`
    /// - Registers:
    ///   - Input: value
    ///   - Output: value
    LogicalAnd { address: Address, value: RegisterOperand },

    /// Binary logical `||` operator.
    ///
    /// This is a short-circuit operator, if the `lhs` value is `true`, then it jumps to `exit` address.
    ///
    /// - Operands:
    ///   - address: `Address`
    /// - Registers:
    ///   - Input: value
    ///   - Output: value
    LogicalOr { address: Address, value: RegisterOperand },

    /// Binary `??` operator.
    ///
    /// This is a short-circuit operator, if the `lhs` value is **not** `null` or `undefined`,
    /// then it jumps to `exit` address.
    ///
    /// - Operands:
    ///   - address: `Address`
    /// - Registers:
    ///   - Input: value
    ///   - Output: value
    Coalesce { address: Address, value: RegisterOperand },

    /// Unary `typeof` operator.
    ///
    /// - Registers:
    ///   - Input: value
    ///   - Output: value
    TypeOf { value: RegisterOperand },

    /// Unary logical `!` operator.
    ///
    /// - Registers:
    ///   - Input: value
    ///   - Output: value
    LogicalNot { value: RegisterOperand },

    /// Unary `+` operator.
    ///
    /// - Registers:
    ///   - Input: value
    ///   - Output: value
    Pos { value: RegisterOperand },

    /// Unary `-` operator.
    ///
    /// - Registers:
    ///   - Input: value
    ///   - Output: value
    Neg { value: RegisterOperand },

    /// Unary `++` operator.
    ///
    /// - Registers:
    ///   - Input: src
    ///   - Output: dst
    Inc { dst: RegisterOperand, src: RegisterOperand },

    /// Unary `--` operator.
    ///
    /// - Registers:
    ///   - Input: src
    ///   - Output: dst
    Dec { dst: RegisterOperand, src: RegisterOperand },

    /// Declare `var` type variable.
    ///
    /// - Operands:
    ///   - binding_index: `IndexOperand`
    DefVar { binding_index: IndexOperand },

    /// Declare and initialize `var` type variable.
    ///
    /// - Operands:
    ///   - binding_index: `IndexOperand`
    /// - Registers:
    ///   - Input: src
    DefInitVar { src: RegisterOperand, binding_index: IndexOperand },

    /// Initialize a lexical binding.
    ///
    /// - Operands:
    ///   - binding_index: `IndexOperand`
    /// - Registers:
    ///   - Input: src
    PutLexicalValue { src: RegisterOperand, binding_index: IndexOperand },

    /// Throws an error because the binding access is illegal.
    ///
    /// - Operands:
    ///   -index: `IndexOperand`
    ThrowMutateImmutable { index: IndexOperand },

    /// Get i-th argument of the current frame.
    ///
    /// Returns `undefined` if `arguments.len()` < `index`.
    ///
    /// - Operands:
    ///   - index: `IndexOperand`
    /// - Registers:
    ///   - Output: dst
    GetArgument { index: IndexOperand, dst: RegisterOperand },

    /// Find a binding on the environment chain and store its value in dst.
    ///
    /// - Operands:
    ///   - binding_index: `IndexOperand`
    /// - Registers:
    ///   - Output: dst
    GetName { dst: RegisterOperand, binding_index: IndexOperand },

    /// Find a binding in the global object and store its value in dst.
    ///
    /// - Operands:
    ///   - binding_index: `IndexOperand`
    ///   - ic_index: `IndexOperand`
    /// - Registers:
    ///   - Output: dst
    GetNameGlobal { dst: RegisterOperand, binding_index: IndexOperand, ic_index: IndexOperand },

    /// Find a binding on the environment and set the `current_binding` of the current frame.
    ///
    /// - Operands:
    ///   - binding_index: `IndexOperand`
    GetLocator { binding_index: IndexOperand },

    /// Find a binding on the environment chain and store its value in dst, and push its
    /// `BindingLocator` to the `bindings_stack`.
    ///
    /// - Operands:
    ///   - binding_index: `IndexOperand`
    /// - Registers:
    ///   - Output: dst
    GetNameAndLocator { dst: RegisterOperand, binding_index: IndexOperand },

    /// Find a binding on the environment chain and store its value in dst. If the binding does not exist, store undefined.
    ///
    /// - Operands:
    ///   - binding_index: `IndexOperand`
    /// - Registers:
    ///   - Output: dst
    GetNameOrUndefined { dst: RegisterOperand, binding_index: IndexOperand },

    /// Find a binding on the environment chain and assign its value.
    ///
    /// - Operands:
    ///   - binding_index: `IndexOperand`
    /// - Registers:
    ///   - Input: src
    SetName { src: RegisterOperand, binding_index: IndexOperand },

    /// Assigns a value to the binding pointed by the top of the `bindings_stack`.
    ///
    /// - Registers:
    ///   - Input: src
    SetNameByLocator { src: RegisterOperand },

    /// Deletes a property of the global object.
    ///
    /// - Operands:
    ///   - binding_index: `IndexOperand`
    /// - Registers:
    ///   - Output: dst
    DeleteName { dst: RegisterOperand, binding_index: IndexOperand },

    /// Gets a method from an object, or `undefined` if the method does not exist.
    ///
    /// Operands:
    ///  - name_index: constant `JsString`.
    ///
    /// Registers (inout)
    ///  - object: `JsObject` as input, `JsObject` or `undefined` as output.
    GetMethod { object: RegisterOperand, name_index: IndexOperand },

    /// Get the length property by name from an object.
    ///
    /// Like `object.name`
    ///
    /// - Operands:
    ///   - ic_index: `IndexOperand`
    /// - Registers:
    ///   - Input: value
    ///   - Output: dst
    GetLengthProperty {
        dst: RegisterOperand,
        value: RegisterOperand,
        ic_index: IndexOperand
    },

    /// Get a property by name from an object.
    ///
    /// Like `object.name`
    ///
    /// - Operands:
    ///   - ic_index: `IndexOperand`
    /// - Registers:
    ///   - Input: value
    ///   - Output: dst
    GetPropertyByName {
        dst: RegisterOperand,
        value: RegisterOperand,
        ic_index: IndexOperand
    },

    /// Get a property by name from an object.
    ///
    /// Like `object.name`
    ///
    /// - Operands:
    ///   - ic_index: `IndexOperand`
    /// - Registers:
    ///   - Input: receiver, value
    ///   - Output: dst
    GetPropertyByNameWithThis {
        dst: RegisterOperand,
        receiver: RegisterOperand,
        value: RegisterOperand,
        ic_index: IndexOperand
    },

    /// Get a property by value from an object.
    ///
    /// Like `object[key]`
    ///
    /// - Registers:
    ///   - Input: object, receiver, key
    ///   - Output: dst
    GetPropertyByValue {
        dst: RegisterOperand,
        key: RegisterOperand,
        receiver: RegisterOperand,
        object: RegisterOperand
    },

    /// Get a property by value from an object.
    ///
    /// Like `object[key]`
    ///
    /// - Registers:
    ///   - Input: object, receiver
    ///   - Output: dst, key
    GetPropertyByValuePush {
        dst: RegisterOperand,
        key: RegisterOperand,
        receiver: RegisterOperand,
        object: RegisterOperand
    },

    /// Sets a property by name of an object.
    ///
    /// Like `object.name = value`
    ///
    /// - Operands:
    ///   - ic_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    SetPropertyByName {
        value: RegisterOperand,
        object: RegisterOperand,
        ic_index: IndexOperand
    },

    /// Sets a property by name of an object.
    ///
    /// Like `object.name = value`
    ///
    /// - Operands:
    ///   - ic_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object,receiver, value
    SetPropertyByNameWithThis {
        value: RegisterOperand,
        receiver: RegisterOperand,
        object: RegisterOperand,
        ic_index: IndexOperand
    },

    /// Sets the name of a function object.
    ///
    /// This operation is corresponds to the `SetFunctionName` abstract operation in the [spec].
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-setfunctionname
    ///
    /// - Operands:
    ///   - prefix
    ///     - 0: no prefix
    ///     - 1: "get "
    ///     - 2: "set "
    /// - Registers:
    ///   - Input: function, name
    SetFunctionName { function: RegisterOperand, name: RegisterOperand, prefix: IndexOperand },

    /// Defines a own property of an object by name.
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    DefineOwnPropertyByName { object: RegisterOperand, value: RegisterOperand, name_index: IndexOperand },

    /// Defines a static class method by name.
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    DefineClassStaticMethodByName {
        value: RegisterOperand,
        object: RegisterOperand,
        name_index: IndexOperand
    },

    /// Defines a class method by name.
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    DefineClassMethodByName {
        value: RegisterOperand,
        object: RegisterOperand,
        name_index: IndexOperand
    },

    /// Sets a property by value of an object.
    ///
    /// Like `object[key] = value`
    ///
    /// - Registers:
    ///   - Input: value, key, receiver, object
    SetPropertyByValue {
        value: RegisterOperand,
        key: RegisterOperand,
        receiver: RegisterOperand,
        object: RegisterOperand
    },

    /// Defines a own property of an object by value.
    ///
    /// - Registers:
    ///   - Input: object, key, value
    DefineOwnPropertyByValue {
        value: RegisterOperand,
        key: RegisterOperand,
        object: RegisterOperand
    },

    /// Defines a static class method by value.
    ///
    /// - Registers:
    ///   - Input: object, key, value
    DefineClassStaticMethodByValue {
        value: RegisterOperand,
        key: RegisterOperand,
        object: RegisterOperand
    },

    /// Defines a class method by value.
    ///
    /// - Registers:
    ///   - Input: object, key, value
    DefineClassMethodByValue {
        value: RegisterOperand,
        key: RegisterOperand,
        object: RegisterOperand
    },

    /// Sets a getter property by name of an object.
    ///
    /// Like `get name() value`
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    SetPropertyGetterByName { object: RegisterOperand, value: RegisterOperand, name_index: IndexOperand },

    /// Defines a static getter class method by name.
    ///
    /// Like `static get name() value`
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    DefineClassStaticGetterByName {
        value: RegisterOperand,
        object: RegisterOperand,
        name_index: IndexOperand
    },

    /// Defines a getter class method by name.
    ///
    /// Like `get name() value`
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    DefineClassGetterByName {
        value: RegisterOperand,
        object: RegisterOperand,
        name_index: IndexOperand
    },

    /// Sets a getter property by value of an object.
    ///
    /// Like `get [key]() value`
    ///
    /// - Registers:
    ///   - Input: object, key, value
    SetPropertyGetterByValue {
        value: RegisterOperand,
        key: RegisterOperand,
        object: RegisterOperand
    },

    /// Defines a static getter class method by value.
    ///
    /// Like `static get [key]() value`
    ///
    /// - Registers:
    ///   - Input: object, key, value
    DefineClassStaticGetterByValue {
        value: RegisterOperand,
        key: RegisterOperand,
        object: RegisterOperand
    },

    /// Defines a getter class method by value.
    ///
    /// Like `get [key]() value`
    ///
    /// - Registers:
    ///   - Input: object, key, value
    DefineClassGetterByValue {
        value: RegisterOperand,
        key: RegisterOperand,
        object: RegisterOperand
    },

    /// Sets a setter property by name of an object.
    ///
    /// Like `set name() value`
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    SetPropertySetterByName { object: RegisterOperand, value: RegisterOperand, name_index: IndexOperand },

    /// Defines a static setter class method by name.
    ///
    /// Like `static set name() value`
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    DefineClassStaticSetterByName {
        value: RegisterOperand,
        object: RegisterOperand,
        name_index: IndexOperand
    },

    /// Defines a setter class method by name.
    ///
    /// Like `set name() value`
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    DefineClassSetterByName {
        value: RegisterOperand,
        object: RegisterOperand,
        name_index: IndexOperand
    },

    /// Sets a setter property by value of an object.
    ///
    /// Like `set [key]() value`
    ///
    /// - Registers:
    ///   - Input: object, key, value
    SetPropertySetterByValue {
        value: RegisterOperand,
        key: RegisterOperand,
        object: RegisterOperand
    },

    /// Defines a static setter class method by value.
    ///
    /// Like `static set [key]() value`
    ///
    /// - Registers:
    ///   - Input: object, key, value
    DefineClassStaticSetterByValue {
        value: RegisterOperand,
        key: RegisterOperand,
        object: RegisterOperand
    },

    /// Defines a setter class method by value.
    ///
    /// Like `set [key]() value`
    ///
    /// - Registers:
    ///   - Input: object, key, value
    DefineClassSetterByValue {
        value: RegisterOperand,
        key: RegisterOperand,
        object: RegisterOperand
    },

    /// Set the value of a private property of an object by it's name.
    ///
    /// Like `obj.#name = value`
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    SetPrivateField { value: RegisterOperand, object: RegisterOperand, name_index: IndexOperand },

    /// Define a private property of a class constructor by it's name.
    ///
    /// Like `#name = value`
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    DefinePrivateField { object: RegisterOperand, value: RegisterOperand, name_index: IndexOperand },

    /// Set a private method of a class constructor by it's name.
    ///
    /// Like `#name() {}`
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    SetPrivateMethod { object: RegisterOperand, value: RegisterOperand, name_index: IndexOperand },

    /// Set a private setter property of a class constructor by it's name.
    ///
    /// Like `set #name() {}`
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    SetPrivateSetter { object: RegisterOperand, value: RegisterOperand, name_index: IndexOperand },

    /// Set a private getter property of a class constructor by it's name.
    ///
    /// Like `get #name() {}`
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    SetPrivateGetter { object: RegisterOperand, value: RegisterOperand, name_index: IndexOperand },

    /// Get a private property by name from an object and store it in dst.
    ///
    /// Like `object.#name`
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object
    ///   - Output: dst
    GetPrivateField { dst: RegisterOperand, object: RegisterOperand, name_index: IndexOperand },

    /// Push a field to a class.
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    ///   - is_anonymous_function: `bool`
    /// - Registers:
    ///   - Input: object, value
    PushClassField { object: RegisterOperand, name: RegisterOperand, value: RegisterOperand, is_anonymous_function: IndexOperand },

    /// Push a private field to the class.
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    PushClassFieldPrivate { object: RegisterOperand, value: RegisterOperand, name_index: IndexOperand },

    /// Push a private getter to the class.
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    PushClassPrivateGetter { object: RegisterOperand, value: RegisterOperand, name_index: IndexOperand },

    /// Push a private setter to the class.
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, value
    PushClassPrivateSetter { object: RegisterOperand, value: RegisterOperand, name_index: IndexOperand },

    /// Push a private method to the class.
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object, proto, value
    PushClassPrivateMethod { object: RegisterOperand, proto: RegisterOperand, value: RegisterOperand, name_index: IndexOperand },

    /// Deletes a property by name of an object.
    ///
    /// Like `delete object.key`
    ///
    /// - Operands:
    ///   - name_index: `IndexOperand`
    /// - Registers:
    ///   - Input: object
    DeletePropertyByName { object: RegisterOperand, name_index: IndexOperand },

    /// Deletes a property by value of an object.
    ///
    /// Like `delete object[key]`
    ///
    /// - Registers:
    ///   - Input: object, key
    DeletePropertyByValue { object: RegisterOperand, key: RegisterOperand },

    /// Throws an error when trying to delete a property of `super`
    DeleteSuperThrow,

    /// Copy all properties of one object to another object.
    ///
    /// - Registers:
    ///   - Input: object, source, excluded_keys
    CopyDataProperties { object: RegisterOperand, source: RegisterOperand, excluded_keys: ThinVec<RegisterOperand> },

    /// Call ToPropertyKey on the value on the stack.
    ///
    /// - Registers:
    ///   - Input: src
    ///   - Output: dst
    ToPropertyKey { src: RegisterOperand, dst: RegisterOperand },

    /// Unconditional jump to address.
    ///
    /// - Operands:
    ///   - address: `u32`
    Jump { address: Address },

    /// Conditional jump to address.
    ///
    /// If the value popped is [`truthy`][truthy] then jump to `address`.
    ///
    /// - Operands:
    ///   - address: `Address`
    /// - Registers (in):
    ///   - `value`: `JsValue`
    ///
    /// [truthy]: https://developer.mozilla.org/en-US/docs/Glossary/Truthy
    JumpIfTrue { address: Address, value: RegisterOperand },

    /// Conditional jump to address.
    ///
    /// If the value popped is [`falsy`][falsy] then jump to `address`.
    ///
    /// - Operands:
    ///   - address: `Address`
    /// - Registers (in):
    ///   - `value`: `JsValue`
    ///
    /// [falsy]: https://developer.mozilla.org/en-US/docs/Glossary/Falsy
    JumpIfFalse { address: Address, value: RegisterOperand },

    /// Conditional jump to address.
    ///
    /// If the value popped is not undefined jump to `address`.
    ///
    /// - Operands:
    ///   - address: `Address`.
    /// - Registers (in):
    ///   - value: `JsValue`
    JumpIfNotUndefined { address: Address, value: RegisterOperand },

    /// Conditional jump to address.
    ///
    /// If the value popped is undefined jump to `address`.
    ///
    /// - Operands:
    ///   - address: `Address`.
    /// - Registers (in):
    ///   - value: `JsValue`.
    JumpIfNullOrUndefined { address: Address, value: RegisterOperand },

    /// Fused `<` comparison + conditional jump.
    ///
    /// Jumps to `address` if `!(lhs < rhs)`.
    ///
    /// - Operands:
    ///   - address: `u32`
    /// - Registers:
    ///   - Input: lhs, rhs
    JumpIfNotLessThan { address: Address, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Fused `<=` comparison + conditional jump.
    ///
    /// Jumps to `address` if `!(lhs <= rhs)`.
    ///
    /// - Operands:
    ///   - address: `u32`
    /// - Registers:
    ///   - Input: lhs, rhs
    JumpIfNotLessThanOrEqual { address: Address, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Fused `>` comparison + conditional jump.
    ///
    /// Jumps to `address` if `!(lhs > rhs)`.
    ///
    /// - Operands:
    ///   - address: `u32`
    /// - Registers:
    ///   - Input: lhs, rhs
    JumpIfNotGreaterThan { address: Address, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Fused `>=` comparison + conditional jump.
    ///
    /// Jumps to `address` if `!(lhs >= rhs)`.
    ///
    /// - Operands:
    ///   - address: `u32`
    /// - Registers:
    ///   - Input: lhs, rhs
    JumpIfNotGreaterThanOrEqual { address: Address, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Conditional jump to address.
    ///
    /// Jump to `address` if two values are not equal.
    ///
    /// - Operands:
    ///   - address: `Address`
    /// - Registers (in):
    ///   - lhs: `JsValue`.
    ///   - rhs: `JsValue`.
    JumpIfNotEqual { address: Address, lhs: RegisterOperand, rhs: RegisterOperand },

    /// Jump table that jumps depending on top value of the stack.
    ///
    /// This is used to handle special cases when we call `continue`, `break` or `return` in a try block,
    /// that has finally block.
    ///
    /// Operands: index: Register, count: `u32`, address: `Address` * count
    JumpTable { index: u32, addresses: ThinVec<Address> },

    /// Throw exception.
    ///
    /// This sets pending exception and searches for an exception handler.
    ///
    /// - Registers:
    ///   - Input: src
    Throw { src: RegisterOperand },

    /// Rethrow thrown exception.
    ///
    /// This is also used to handle generator `return()` call, we throw an empty exception, by setting pending exception to [`None`],
    /// propagating it and calling finally code until there is no exception handler left, in that case we consume the empty exception and return
    /// from the generator.
    ReThrow,

    /// Get the thrown pending exception (if it's set) and store it in dst.
    ///
    /// If there is no pending exception, which can happen if we are handling `return()` call on generator,
    /// then we rethrow the empty exception. See [`Opcode::ReThrow`].
    ///
    /// - Registers:
    ///   - Output: dst
    Exception { dst: RegisterOperand },

    /// Get the thrown pending exception if it's set and store `true` in `has_exception`, otherwise store `false`.
    ///
    /// - Registers:
    ///   - Output: exception, has_exception
    MaybeException { has_exception: RegisterOperand, exception: RegisterOperand },

    /// Throw a new `TypeError` exception
    ///
    /// - Operands:
    ///   - message: `IndexOperand`
    ThrowNewTypeError { message: IndexOperand },

    /// Throw a new `ReferenceError` exception
    ///
    /// - Operands:
    ///   - message: `IndexOperand`
    ThrowNewReferenceError { message: IndexOperand },

    /// Gets the function object of the current function environment
    ///
    /// - Registers (out):
    ///   - function_object: `JsObject`.
    GetFunctionObject { function_object: RegisterOperand },

    /// Pushes `this` value
    ///
    /// - Registers:
    ///   - Output: dst
    This { dst: RegisterOperand },

    /// Pushes `this` value that is related to the object environment of the given binding
    ///
    /// - Operands:
    ///   - index: `IndexOperand`
    /// - Registers:
    ///   - Output: dst
    ThisForObjectEnvironmentName { dst: RegisterOperand, index: IndexOperand },

    /// Execute the `super()` method.
    ///
    /// - Operands:
    ///   - argument_count: `IndexOperand`
    /// - Stack: super_constructor, argument_1, ... argument_n **=>**
    SuperCall { argument_count: IndexOperand },

    /// Execute the `super()` method where the arguments contain spreads.
    ///
    /// Operands:
    ///
    /// Stack: super_constructor, arguments_array **=>**
    SuperCallSpread,

    /// Execute the `super()` method when no constructor of the class is defined.
    ///
    /// Operands:
    ///
    /// Stack: argument_n, ... argument_1 **=>**
    SuperCallDerived,

    /// Binds `this` value and initializes the instance elements.
    ///
    /// Performs steps 7-12 of [`SuperCall: super Arguments`][spec]
    ///
    /// - Registers:
    ///   - Input: value
    ///   - Output: value
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-super-keyword-runtime-semantics-evaluation
    BindThisValue { value: RegisterOperand },

    /// Dynamically import a module.
    ///
    /// - Operands:
    ///   - phase: `IndexOperand` (0 = evaluation, 1 = defer, 2 = source)
    /// - Registers:
    ///   - Input: specifier, options
    ///   - Output: specifier
    ImportCall { specifier: RegisterOperand, options: RegisterOperand, phase: IndexOperand },

    /// Strict equal compare two register values,
    /// if true jumps to address.
    /// - Operands:
    ///   - address: `Address`
    /// - Registers:
    ///   - Input: value, condition
    Case { address: Address, value: RegisterOperand, condition: RegisterOperand },

    /// Get function from the pre-compiled inner functions.
    ///
    /// - Operands:
    ///   - index: `IndexOperand`
    /// - Registers:
    ///   - Output: dst
    GetFunction { dst: RegisterOperand, index: IndexOperand },

    /// Call a function named "eval".
    ///
    /// - Operands:
    ///   - argument_count: `IndexOperand`
    ///   - scope_index: `IndexOperand`
    /// - Stack: this, func, argument_1, ... argument_n **=>** result
    CallEval { argument_count: IndexOperand, scope_index: IndexOperand },

    /// Call a function named "eval" where the arguments contain spreads.
    ///
    /// - Operands:
    ///   - scope_index: `IndexOperand`
    /// - Stack: Stack: this, func, arguments_array **=>** result
    CallEvalSpread { scope_index: IndexOperand },

    /// Call a function.
    ///
    /// - Operands:
    ///   - argument_count: `IndexOperand`
    /// - Stack: this, func, argument_1, ... argument_n **=>** result
    Call { argument_count: IndexOperand },

    /// Call a function where the arguments contain spreads.
    ///
    /// Operands:
    ///
    /// Stack: this, func, arguments_array **=>** result
    CallSpread,

    /// Call construct on a function.
    ///
    /// - Operands:
    ///   - argument_count: `IndexOperand`
    /// - Stack: this, func, argument_1, ... argument_n **=>** result
    New { argument_count: IndexOperand },

    /// Call construct on a function where the arguments contain spreads.
    ///
    /// Operands:
    ///
    /// Stack: arguments_array, func **=>** result
    NewSpread,

    /// Check return from a function.
    CheckReturn,

    /// Return from a function.
    Return,

    /// Close an async generator function.
    AsyncGeneratorClose,

    /// Creates the Generator object and yields.
    ///
    /// - Stack: **=>** resume_kind
    Generator,

    /// Creates the AsyncGenerator object and yields.
    ///
    /// - Stack: **=>** resume_kind
    AsyncGenerator,

    /// Set return value of a function.
    ///
    /// - Registers:
    ///   - Input: src
    SetAccumulator { src: RegisterOperand },

    // Set return value of a function.
    ///
    /// - Registers:
    ///   - Output: dst
    SetRegisterFromAccumulator { dst: RegisterOperand },

    /// Move value of operand `src` to register `dst`.
    ///
    /// - Registers:
    ///   - Input: src
    ///   - Output: dst
    Move { dst: RegisterOperand, src: RegisterOperand },

    /// Pop value from the stack and push to register `dst`
    ///
    /// - Registers:
    ///   - Output: dst
    PopIntoRegister { dst: RegisterOperand },

    /// Copy value at register `src` and push it on the stack.
    ///
    /// - Registers:
    ///   - Input: src
    PushFromRegister { src: RegisterOperand },

    /// Push a declarative environment.
    ///
    /// - Operands:
    ///   - scope_index: `IndexOperand`
    PushScope { scope_index: IndexOperand },

    /// Push an object environment.
    ///
    /// - Registers:
    ///   - Input: src
    PushObjectEnvironment { src: RegisterOperand },

    /// Pop the current environment.
    PopEnvironment,

    /// Increment loop iteration count.
    ///
    /// Used for limiting the loop iteration.
    IncrementLoopIteration,

    /// Creates the ForInIterator of an object.
    ///
    /// - Registers:
    ///   - Input: src
    CreateForInIterator { src: RegisterOperand },

    /// Gets the iterator of an object.
    ///
    /// - Registers:
    ///   - Input: src
    /// - Iterator Stack: **=>** `iterator`
    GetIterator { src: RegisterOperand },

    /// Gets the async iterator of an object.
    ///
    /// - Registers:
    ///   - Input: src
    /// - Iterator Stack: **=>** `iterator`
    GetAsyncIterator { src: RegisterOperand },

    /// Pop an iterator from the iterators stack
    /// - Registers (out)
    ///   - iterator: `JsObject`.
    ///   - next: `JsValue`.
    IteratorPop { iterator: RegisterOperand, next: RegisterOperand },

    /// Pushes an iterator on the iterators stack
    /// - Registers (in)
    ///   - iterator: `JsObject`.
    ///   - next: `JsValue`.
    IteratorPush { iterator: RegisterOperand, next: RegisterOperand },

    /// Calls the `next` method of `iterator`, updating its record with the next value.
    ///
    /// - Iterator Stack: `iterator` **=>** `iterator`
    IteratorNext,

    /// Updates the result of the currently active iterator.
    /// - Registers (inout)
    ///  - result: `JsValue` (in), `bool` (out) with the `done` value of the iterator.
    IteratorUpdateResult { result: RegisterOperand },

    /// Returns `true` if the current iterator is done, or `false` otherwise
    ///
    /// - Registers:
    ///   - Output: dst
    /// - Iterator Stack: `iterator` **=>** `iterator`
    IteratorDone { dst: RegisterOperand },

    /// Finishes the call to `Opcode::IteratorNext` within a `for await` loop by setting the current
    /// result of the current iterator.
    ///
    /// - Registers:
    ///   - Input: resume_kind, value
    /// - Iterator Stack: `iterator` **=>** `iterator`
    IteratorFinishAsyncNext { resume_kind: RegisterOperand, value: RegisterOperand },

    /// Gets the `value` property of the current iterator record.
    ///
    /// - Registers:
    ///   - Output: dst
    /// - Iterator Stack: `iterator` **=>** `iterator`
    IteratorValue { dst: RegisterOperand },

    /// Gets the last iteration result of the current iterator record.
    ///
    /// - Registers:
    ///   - Output: dst
    /// - Iterator Stack: `iterator` **=>** `iterator`
    IteratorResult { dst: RegisterOperand },

    /// Consume the iterator and construct and array with all the values.
    ///
    /// - Registers:
    ///   - Output: dst
    /// - Iterator Stack: `iterator` **=>** `iterator`
    IteratorToArray { dst: RegisterOperand },

    /// Store `true` in dst if the iterator stack is empty.
    ///
    /// - Registers:
    ///   - Output: dst
    /// - Iterator Stack: **=>**
    IteratorStackEmpty { dst: RegisterOperand },

    /// Creates a new iterator result object.
    ///
    /// - Operands:
    ///   - done: `bool` (codified as u8 with `0` -> `false` and `!0` -> `true`)
    /// - Registers:
    ///   - Input: value
    ///   - Output: value
    CreateIteratorResult { value: RegisterOperand, done: IndexOperand },

    /// Calls `return` on the current iterator and returns the result.
    ///
    /// - Registers:
    ///   - Output: value, called
    /// - Iterator Stack: `iterator` **=>**
    IteratorReturn { value: RegisterOperand, called: RegisterOperand },

    /// Concat multiple stack objects into a string.
    ///
    /// - Registers:
    ///   - Input: values
    ///   - Output: dst
    ConcatToString { dst: RegisterOperand, values: ThinVec<RegisterOperand> },

    /// Require the stack value to be neither null nor undefined.
    ///
    /// - Registers:
    ///   - Input: src
    ValueNotNullOrUndefined { src: RegisterOperand },

    /// Initialize the rest parameter value of a function from the remaining arguments.
    ///
    /// - Stack: `argument_1` .. `argument_n` **=>**
    /// - Registers:
    ///   - Output: dst
    RestParameterInit { dst: RegisterOperand },

    /// Yields from the current generator execution.
    ///
    /// - Registers:
    ///   - Input: src
    ///   - Output: resume_kind, received
    GeneratorYield { src: RegisterOperand },

    /// Yields from the current async generator execution.
    ///
    /// - Registers:
    ///   - Input: src
    ///   - Output: resume_kind, received
    AsyncGeneratorYield { src: RegisterOperand },

    /// Create a promise capacity for an async function, if not already set.
    CreatePromiseCapability,

    /// Stops the current async function and schedules it to resume later.
    ///
    /// - Registers:
    ///   - Input: src
    ///   - Output: resume_kind, received
    Await { src: RegisterOperand },

    /// Store the current new target in dst.
    ///
    /// - Registers:
    ///   - Output: dst
    NewTarget { dst: RegisterOperand },

    /// Store the current `import.meta` in dst.
    ///
    /// - Registers:
    ///   - Output: dst
    ImportMeta { dst: RegisterOperand },

    /// Store `true` in the register if the value is an object, or `false` otherwise.
    ///
    /// - Registers:
    ///   - Input: value
    ///   - Output: value
    IsObject { value: RegisterOperand },

    /// Lookup if a tagged template object is cached and skip the creation if it is.
    ///
    /// - Operands:
    ///   - address: `u32`
    ///   - site: `u64`
    /// - Registers:
    ///   - Output: dst
    TemplateLookup { address: Address, site: u64, dst: RegisterOperand },

    /// Create a new tagged template object and cache it.
    ///
    /// - Operands:
    ///   - site: `u64`
    /// - Registers:
    ///   - Inputs: values
    ///   - Output: dst
    TemplateCreate { site: u64, dst: RegisterOperand, values: ThinVec<u32> },

    /// Push a private environment.
    ///
    /// Operands: count: `u32`, count * name_index: `u32`
    ///
    /// - Registers:
    ///   - Input: class, name_indices
    PushPrivateEnvironment { class: RegisterOperand, name_indices: ThinVec<u32> },

    /// Pop a private environment.
    PopPrivateEnvironment,

    /// Creates a mapped `arguments` object.
    ///
    /// Performs [`10.4.4.7 CreateMappedArgumentsObject ( func, formals, argumentsList, env )`]
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-createunmappedargumentsobject
    ///
    /// - Registers:
    ///   - Output: dst
    CreateMappedArgumentsObject { dst: RegisterOperand },

    /// Creates an unmapped `arguments` object.
    ///
    /// Performs: [`10.4.4.6 CreateUnmappedArgumentsObject ( argumentsList )`]
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-createmappedargumentsobject
    ///
    /// - Registers:
    ///   - Output: dst
    CreateUnmappedArgumentsObject { dst: RegisterOperand },

    /// Reserved [`Opcode`].
    Reserved1 => Reserved,
    /// Reserved [`Opcode`].
    Reserved2 => Reserved,
    /// Reserved [`Opcode`].
    Reserved3 => Reserved,
    /// Reserved [`Opcode`].
    Reserved4 => Reserved,
    /// Reserved [`Opcode`].
    Reserved5 => Reserved,
    /// Reserved [`Opcode`].
    Reserved6 => Reserved,
    /// Reserved [`Opcode`].
    Reserved7 => Reserved,
    /// Reserved [`Opcode`].
    Reserved8 => Reserved,
    /// Reserved [`Opcode`].
    Reserved9 => Reserved,
    /// Reserved [`Opcode`].
    Reserved10 => Reserved,
    /// Reserved [`Opcode`].
    Reserved11 => Reserved,
    /// Reserved [`Opcode`].
    Reserved12 => Reserved,
    /// Reserved [`Opcode`].
    Reserved13 => Reserved,
    /// Reserved [`Opcode`].
    Reserved14 => Reserved,
    /// Reserved [`Opcode`].
    Reserved15 => Reserved,
    /// Reserved [`Opcode`].
    Reserved16 => Reserved,
    /// Reserved [`Opcode`].
    Reserved17 => Reserved,
    /// Reserved [`Opcode`].
    Reserved18 => Reserved,
    /// Reserved [`Opcode`].
    Reserved19 => Reserved,
    /// Reserved [`Opcode`].
    Reserved20 => Reserved,
    /// Reserved [`Opcode`].
    Reserved21 => Reserved,
    /// Reserved [`Opcode`].
    Reserved22 => Reserved,
    /// Reserved [`Opcode`].
    Reserved23 => Reserved,
    /// Reserved [`Opcode`].
    Reserved24 => Reserved,
    /// Reserved [`Opcode`].
    Reserved25 => Reserved,
    /// Reserved [`Opcode`].
    Reserved26 => Reserved,
    /// Reserved [`Opcode`].
    Reserved27 => Reserved,
    /// Reserved [`Opcode`].
    Reserved28 => Reserved,
    /// Reserved [`Opcode`].
    Reserved29 => Reserved,
    /// Reserved [`Opcode`].
    Reserved30 => Reserved,
    /// Reserved [`Opcode`].
    Reserved31 => Reserved,
    /// Reserved [`Opcode`].
    Reserved32 => Reserved,
    /// Reserved [`Opcode`].
    Reserved33 => Reserved,
    /// Reserved [`Opcode`].
    Reserved34 => Reserved,
    /// Reserved [`Opcode`].
    Reserved35 => Reserved,
    /// Reserved [`Opcode`].
    Reserved36 => Reserved,
    /// Reserved [`Opcode`].
    Reserved37 => Reserved,
    /// Reserved [`Opcode`].
    Reserved38 => Reserved,
    /// Reserved [`Opcode`].
    Reserved39 => Reserved,
    /// Reserved [`Opcode`].
    Reserved40 => Reserved,
    /// Reserved [`Opcode`].
    Reserved41 => Reserved,
    /// Reserved [`Opcode`].
    Reserved42 => Reserved,
    /// Reserved [`Opcode`].
    Reserved43 => Reserved,
    /// Reserved [`Opcode`].
    Reserved44 => Reserved,
    /// Reserved [`Opcode`].
    Reserved45 => Reserved,
    /// Reserved [`Opcode`].
    Reserved46 => Reserved,
    /// Reserved [`Opcode`].
    Reserved47 => Reserved,
    /// Reserved [`Opcode`].
    Reserved48 => Reserved,
    /// Reserved [`Opcode`].
    Reserved49 => Reserved,
    /// Reserved [`Opcode`].
    Reserved50 => Reserved,
    /// Reserved [`Opcode`].
    Reserved51 => Reserved,
    /// Reserved [`Opcode`].
    Reserved52 => Reserved,
    /// Reserved [`Opcode`].
    Reserved53 => Reserved,
    /// Reserved [`Opcode`].
    Reserved54 => Reserved,
    /// Reserved [`Opcode`].
    Reserved55 => Reserved,
    /// Reserved [`Opcode`].
    Reserved56 => Reserved,
    /// Reserved [`Opcode`].
    Reserved57 => Reserved,
    /// Reserved [`Opcode`].
    Reserved58 => Reserved,
    /// Reserved [`Opcode`].
    Reserved59 => Reserved,
    /// Declare `var` type variable during eval declaration instantiation.
    ///
    /// - Operands:
    ///   - binding_index: `IndexOperand`
    DefEvalVar { binding_index: IndexOperand },
}
