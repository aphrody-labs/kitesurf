//! `CodeBlock`
//!
//! This module is for the `CodeBlock` which implements a function representation in the VM

use crate::{
    Context, JsBigInt, JsString, JsValue, SpannedSourceText,
    builtins::{
        OrdinaryObject,
        function::{OrdinaryFunction, ThisMode},
    },
    object::JsObject,
};
use bitflags::bitflags;
use boa_ast::scope::{BindingLocator, Scope};
use boa_gc::{Finalize, Gc, Trace, custom_trace, empty_trace};
use itertools::Itertools;
use std::{cell::Cell, cell::OnceCell, cell::UnsafeCell, fmt::Display, fmt::Write as _};
use thin_vec::ThinVec;

use super::{
    InlineCache,
    opcode::{Address, Bytecode, Instruction, InstructionIterator},
    source_info::{SourceInfo, SourceMap, SourcePath},
};

bitflags! {
    /// Flags for [`CodeBlock`].
    #[derive(Clone, Copy, Debug, Finalize)]
    pub(crate) struct CodeBlockFlags: u16 {
        /// Is this function in strict mode.
        const STRICT = 0b0000_0001;

        /// Indicates if the function is an expression and has a binding identifier.
        const HAS_BINDING_IDENTIFIER = 0b0000_0010;

        /// The `[[IsClassConstructor]]` internal slot.
        const IS_CLASS_CONSTRUCTOR = 0b0000_0100;

        /// The `[[ClassFieldInitializerName]]` internal slot.
        const IN_CLASS_FIELD_INITIALIZER = 0b0000_1000;

        /// `[[ConstructorKind]]`
        const IS_DERIVED_CONSTRUCTOR = 0b0001_0000;

        const IS_ASYNC = 0b0010_0000;
        const IS_GENERATOR = 0b0100_0000;

        /// Arrow and method functions don't have `"prototype"` property.
        const HAS_PROTOTYPE_PROPERTY = 0b1000_0000;

        /// If the function requires a function scope.
        const HAS_FUNCTION_SCOPE = 0b1_0000_0000;

        /// Kitesurf: a frame of this block was ever pushed. Only a block that never ran can
        /// leave its body to the code store: no frame and no nested function hold it yet.
        const KS_RAN = 0b10_0000_0000;

        /// Trace instruction execution to `stdout`.
        #[cfg(feature = "trace")]
        const TRACEABLE = 0b1000_0000_0000_0000;
    }
}

impl CodeBlockFlags {
    /// Check if the [`CodeBlock`] has a function scope.
    #[must_use]
    pub(crate) fn has_function_scope(self) -> bool {
        self.contains(Self::HAS_FUNCTION_SCOPE)
    }
}

// SAFETY: Nothing in CodeBlockFlags needs tracing, so this is safe.
unsafe impl Trace for CodeBlockFlags {
    empty_trace!();
}

/// This represents a range in the code that handles exception throws.
///
/// When a throw happens, we search for handler in the [`CodeBlock`] using
/// the [`CodeBlock::find_handler()`] method.
///
/// If any exception happens and gets caught by this handler, the `pc` will be set to `end` of the
/// [`Handler`] and remove any environments or stack values that where pushed after the handler.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Handler {
    pub(crate) start: Address,
    pub(crate) end: Address,
    pub(crate) environment_count: u32,
}

impl Handler {
    /// Get the handler address.
    pub(crate) const fn handler(&self) -> Address {
        self.end
    }

    /// Check if the provided `pc` is contained in the handler range.
    pub(crate) const fn contains(&self, pc: u32) -> bool {
        pc < self.end.as_u32() && pc >= self.start.as_u32()
    }
}

#[derive(Clone, Debug, Trace, Finalize)]
pub(crate) enum Constant {
    /// Property field names and private names `[[description]]`s.
    String(JsString),
    Function(Gc<CodeBlock>),
    BigInt(#[unsafe_ignore_trace] JsBigInt),

    /// Declarative or function scope.
    // Safety: Nothing in `Scope` needs tracing, so this is safe.
    Scope(#[unsafe_ignore_trace] Scope),
}

/// Binding information about a global function.
#[derive(Copy, Clone, Debug, Trace, Finalize)]
#[boa_gc(empty_trace)]
pub(crate) struct GlobalFunctionBinding {
    /// The index of the global function's name in the constants array.
    pub(crate) name_index: u32,
    /// The index of the global function in the constants array
    pub(crate) function_index: u32,
}

/// The internal representation of a JavaScript function.
///
/// A `CodeBlock` is generated for each function compiled by the
/// [`ByteCompiler`](crate::bytecompiler::ByteCompiler). It stores the bytecode and the other
/// attributes of the function.
#[derive(Finalize)]
pub struct CodeBlock {
    pub(crate) flags: Cell<CodeBlockFlags>,

    /// The number of arguments expected.
    pub(crate) length: u32,

    /// `[[ThisMode]]`
    pub(crate) this_mode: ThisMode,

    /// The function's name, for function objects and call frames; kept beside the body's
    /// source info so that making a function object reads nothing else.
    pub(crate) name: JsString,

    /// The function's source text, for `Function.prototype.toString`, its only reader,
    /// which so never needs the body.
    pub(crate) text_spanned: SpannedSourceText,

    /// Bytecode
    pub(crate) bytecode: Bytecode,

    // Used for identifying anonymous functions in compiled output and call frames.
    pub(crate) debug_id: u64,

    // Kitesurf: this block's share of the code counters (crate::ks_code_stats), refunded
    // when it drops; `ks_charged` sets it once the block's parts are in place.
    pub(crate) ks_charge: crate::ks_code_stats::CodeCharge,

    // Kitesurf: resident, or in the code store (crate::ks_body); see `replace_body` for when
    // it may change, which is what makes the references `body()` hands out sound.
    body: UnsafeCell<BodyState>,

    #[cfg(feature = "trace")]
    pub(crate) traced: Cell<bool>,
}

/// Kitesurf: where a block's body is: in memory, or in the code store under a
/// descriptor (crate::ks_body).
pub(crate) enum BodyState {
    Resident(Box<CodeBody>),
    Swapped(Box<crate::ks_body::BodyDesc>),
}

// SAFETY: the shell holds no GC pointers; a resident body is traced, a swapped one has its
// edges in the store. A fault-in installs the body only once it is complete.
unsafe impl Trace for CodeBlock {
    custom_trace!(this, mark, {
        if let Some(body) = this.resident_body() {
            mark(body);
        }
    });
}

impl std::fmt::Debug for CodeBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeBlock")
            .field("name", &self.name)
            .field("length", &self.length)
            .field("flags", &self.flags.get())
            .field("bytecode", &self.bytecode)
            .field("body", &self.resident_body())
            .finish_non_exhaustive()
    }
}

/// Kitesurf: the part of a [`CodeBlock`] that only running it reads: constants, bindings,
/// handlers, inline caches, source map and global tables.
#[derive(Debug, Trace, Finalize)]
pub(crate) struct CodeBody {
    pub(crate) parameter_length: u32,

    pub(crate) register_count: u32,

    /// Used for constructing a `MappedArguments` object.
    #[unsafe_ignore_trace]
    pub(crate) mapped_arguments_binding_indices: ThinVec<Option<u32>>,

    pub(crate) constants: ThinVec<Constant>,

    /// Locators for all bindings in the codeblock.
    #[unsafe_ignore_trace]
    pub(crate) bindings: Box<[BindingLocator]>,

    /// Exception [`Handler`]s.
    #[unsafe_ignore_trace]
    pub(crate) handlers: ThinVec<Handler>,

    /// Property names of the inline caches, by IC index.
    pub(crate) ic_names: Box<[JsString]>,

    // The caches themselves are built on first use; most of a bundle's
    // functions never run, and a site's cache is 9x the size of its name.
    pub(crate) ic: OnceCell<Box<[InlineCache]>>,

    /// Bytecode to source code mapping.
    pub(crate) source_info: SourceInfo,

    pub(crate) global_lexs: Box<[u32]>,
    pub(crate) global_fns: Box<[GlobalFunctionBinding]>,
    pub(crate) global_vars: Box<[u32]>,
}

impl CodeBody {
    /// Kitesurf: the heap bytes this body owns, apart from its source map: the box and the
    /// arrays. Shared strings, scopes and nested blocks are not its own.
    pub(crate) fn ks_fixed_bytes(&self) -> usize {
        size_of::<Self>()
            + self.mapped_arguments_binding_indices.capacity() * size_of::<Option<u32>>()
            + self.constants.capacity() * size_of::<Constant>()
            + size_of_val::<[BindingLocator]>(&self.bindings)
            + self.handlers.capacity() * size_of::<Handler>()
            + size_of_val::<[JsString]>(&self.ic_names)
            + size_of_val::<[u32]>(&self.global_lexs)
            + size_of_val::<[GlobalFunctionBinding]>(&self.global_fns)
            + size_of_val::<[u32]>(&self.global_vars)
    }
}

/// ---- `CodeBlock` public API ----
impl CodeBlock {
    /// The inline caches, built from [`Self::ic_names`] the first time any is used.
    #[inline]
    pub(crate) fn ic(&self) -> &[InlineCache] {
        let body = self.body();
        body.ic.get_or_init(|| {
            let ic: Box<[InlineCache]> =
                body.ic_names.iter().cloned().map(InlineCache::new).collect();
            self.ks_charge.charge_ic(size_of_val::<[InlineCache]>(&ic));
            ic
        })
    }

    /// Kitesurf: charges this block to the code counters: the struct and the arrays it owns,
    /// and its source map apart. Shared strings, scopes and nested blocks are not its own.
    #[must_use]
    pub(crate) fn ks_charged(mut self) -> Self {
        let fixed = size_of::<Self>() + self.body().ks_fixed_bytes();
        let map = self.body().source_info.map().ks_heap_bytes();
        self.ks_charge = crate::ks_code_stats::CodeCharge::new(fixed, map);
        self
    }

    /// Creates a new `CodeBlock`.
    #[must_use]
    pub fn new(name: JsString, length: u32, strict: bool) -> Self {
        let mut flags = CodeBlockFlags::empty();
        flags.set(CodeBlockFlags::STRICT, strict);
        let body = CodeBody {
            parameter_length: 0,
            register_count: 0,
            mapped_arguments_binding_indices: ThinVec::new(),
            constants: ThinVec::default(),
            bindings: Box::default(),
            handlers: ThinVec::default(),
            ic_names: Box::default(),
            ic: OnceCell::new(),
            source_info: SourceInfo::new(SourceMap::new(Box::default(), SourcePath::None), name.clone()),
            global_lexs: Box::default(),
            global_fns: Box::default(),
            global_vars: Box::default(),
        };
        let fixed = size_of::<Self>() + body.ks_fixed_bytes();
        Self {
            flags: Cell::new(flags),
            length,
            this_mode: ThisMode::Global,
            name,
            text_spanned: SpannedSourceText::new_empty(),
            bytecode: Bytecode::default(),
            debug_id: CodeBlock::get_next_codeblock_id(),
            ks_charge: crate::ks_code_stats::CodeCharge::new(fixed, 0),
            body: UnsafeCell::new(BodyState::Resident(Box::new(body))),
            #[cfg(feature = "trace")]
            traced: Cell::new(false),
        }
    }

    /// Retrieves the name associated with this code block.
    #[must_use]
    pub fn name(&self) -> &JsString {
        &self.name
    }

    /// Retrieves the path of this code block.
    #[must_use]
    pub fn path(&self) -> &SourcePath {
        self.body().source_info.map().path()
    }

    /// Kitesurf: the part only running the block reads, fetched from the code store if it
    /// was swapped out.
    #[inline]
    pub(crate) fn body(&self) -> &CodeBody {
        match self.resident_body() {
            Some(body) => body,
            None => crate::ks_body::fault_in(self),
        }
    }

    /// Kitesurf: the body if it is in memory, without fetching it: what tracing and the
    /// swap rounds read, since neither may run a fault-in.
    #[inline]
    pub(crate) fn resident_body(&self) -> Option<&CodeBody> {
        // SAFETY: a shared read of the state under `replace_body`'s contract: nothing
        // replaces the state while the returned reference lives.
        match unsafe { &*self.body.get() } {
            BodyState::Resident(body) => Some(body),
            BodyState::Swapped(_) => None,
        }
    }

    /// Kitesurf: the swap descriptor, if the body is in the store; a fault-in reads it and
    /// then replaces it.
    pub(crate) fn body_desc(&self) -> Option<&crate::ks_body::BodyDesc> {
        // SAFETY: a shared read of the state under `replace_body`'s contract: nothing
        // replaces the state while the returned reference lives.
        match unsafe { &*self.body.get() } {
            BodyState::Resident(_) => None,
            BodyState::Swapped(desc) => Some(desc),
        }
    }

    /// Kitesurf: swaps the body state. Safety: no reference from `body`, `resident_body` or
    /// `body_desc` may be live; bodies leave only never-run blocks at idle, and return on read.
    pub(crate) unsafe fn replace_body(&self, state: BodyState) -> BodyState {
        // SAFETY: the caller guarantees no reference into the state is live, so the exclusive
        // access below aliases nothing.
        unsafe { std::mem::replace(&mut *self.body.get(), state) }
    }

    /// Kitesurf: a block whose body is in the store, as a fault-in rebuilds each block
    /// nested in the body it reads back.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn ks_shell(
        flags: CodeBlockFlags,
        length: u32,
        this_mode: ThisMode,
        name: JsString,
        text_spanned: SpannedSourceText,
        bytecode: Bytecode,
        debug_id: u64,
        desc: crate::ks_body::BodyDesc,
    ) -> Self {
        Self {
            flags: Cell::new(flags),
            length,
            this_mode,
            name,
            text_spanned,
            bytecode,
            debug_id,
            ks_charge: crate::ks_code_stats::CodeCharge::new(Self::ks_shell_bytes(), 0),
            body: UnsafeCell::new(BodyState::Swapped(Box::new(desc))),
            #[cfg(feature = "trace")]
            traced: Cell::new(false),
        }
    }

    /// Kitesurf: what a block whose body is in the store keeps in memory: the shell and
    /// its descriptor, for the code counters.
    pub(crate) fn ks_shell_bytes() -> usize {
        size_of::<Self>() + size_of::<crate::ks_body::BodyDesc>()
    }

    pub(crate) fn ks_ran(&self) -> bool {
        self.flags.get().contains(CodeBlockFlags::KS_RAN)
    }

    pub(crate) fn ks_mark_ran(&self) {
        let flags = self.flags.get();
        if !flags.contains(CodeBlockFlags::KS_RAN) {
            self.flags.set(flags | CodeBlockFlags::KS_RAN);
        }
    }

    /// Kitesurf: assembles a block from its two parts as the compiler finishes it, charging
    /// it to the code counters.
    pub(crate) fn from_parts(
        flags: CodeBlockFlags,
        length: u32,
        this_mode: ThisMode,
        name: JsString,
        text_spanned: SpannedSourceText,
        bytecode: Bytecode,
        body: CodeBody,
    ) -> Self {
        Self {
            flags: Cell::new(flags),
            length,
            this_mode,
            name,
            text_spanned,
            bytecode,
            debug_id: CodeBlock::get_next_codeblock_id(),
            ks_charge: crate::ks_code_stats::CodeCharge::default(),
            body: UnsafeCell::new(BodyState::Resident(Box::new(body))),
            #[cfg(feature = "trace")]
            traced: Cell::new(false),
        }
        .ks_charged()
    }

    /// Check if the function is traced.
    #[cfg(feature = "trace")]
    pub(crate) fn traceable(&self) -> bool {
        self.flags.get().contains(CodeBlockFlags::TRACEABLE)
    }
    /// Enable or disable instruction tracing to `stdout`.
    #[cfg(feature = "trace")]
    #[inline]
    pub fn set_traceable(&self, value: bool) {
        let mut flags = self.flags.get();
        flags.set(CodeBlockFlags::TRACEABLE, value);
        self.flags.set(flags);
    }

    /// Check if the function is a class constructor.
    pub(crate) fn is_class_constructor(&self) -> bool {
        self.flags
            .get()
            .contains(CodeBlockFlags::IS_CLASS_CONSTRUCTOR)
    }

    /// Check if the function is in strict mode.
    pub(crate) fn strict(&self) -> bool {
        self.flags.get().contains(CodeBlockFlags::STRICT)
    }

    /// Indicates if the function is an expression and has a binding identifier.
    pub(crate) fn has_binding_identifier(&self) -> bool {
        self.flags
            .get()
            .contains(CodeBlockFlags::HAS_BINDING_IDENTIFIER)
    }

    /// Does this function have the `[[ClassFieldInitializerName]]` internal slot set to non-empty value.
    pub(crate) fn in_class_field_initializer(&self) -> bool {
        self.flags
            .get()
            .contains(CodeBlockFlags::IN_CLASS_FIELD_INITIALIZER)
    }

    /// Returns true if this function is a derived constructor.
    pub(crate) fn is_derived_constructor(&self) -> bool {
        self.flags
            .get()
            .contains(CodeBlockFlags::IS_DERIVED_CONSTRUCTOR)
    }

    /// Returns true if this function an async function.
    pub(crate) fn is_async(&self) -> bool {
        self.flags.get().contains(CodeBlockFlags::IS_ASYNC)
    }

    /// Returns true if this function an generator function.
    pub(crate) fn is_generator(&self) -> bool {
        self.flags.get().contains(CodeBlockFlags::IS_GENERATOR)
    }

    /// Returns true if this function a async generator function.
    pub(crate) fn is_async_generator(&self) -> bool {
        self.flags
            .get()
            .contains(CodeBlockFlags::IS_ASYNC | CodeBlockFlags::IS_GENERATOR)
    }

    /// Returns true if this function an async function.
    pub(crate) fn is_ordinary(&self) -> bool {
        !self.is_async() && !self.is_generator()
    }

    /// Returns true if this function has the `"prototype"` property when function object is created.
    pub(crate) fn has_prototype_property(&self) -> bool {
        self.flags
            .get()
            .contains(CodeBlockFlags::HAS_PROTOTYPE_PROPERTY)
    }

    /// Returns true if this function requires a function scope.
    pub(crate) fn has_function_scope(&self) -> bool {
        self.flags.get().has_function_scope()
    }

    /// Find exception [`Handler`] in the code block given the current program counter (`pc`).
    #[inline]
    pub(crate) fn find_handler(&self, pc: u32) -> Option<(usize, &Handler)> {
        self.body()
            .handlers
            .iter()
            .enumerate()
            .rev()
            .find(|(_, handler)| handler.contains(pc))
    }

    /// Get the [`JsString`] constant from the [`CodeBlock`].
    ///
    /// # Panics
    ///
    /// If the type of the [`Constant`] is not [`Constant::String`].
    /// Or `index` is greater or equal to length of `constants`.
    pub(crate) fn constant_string(&self, index: usize) -> JsString {
        if let Some(Constant::String(value)) = self.body().constants.get(index) {
            return value.clone();
        }

        panic!("expected string constant at index {index}")
    }

    /// Get the function ([`Gc<CodeBlock>`]) constant from the [`CodeBlock`].
    ///
    /// # Panics
    ///
    /// If the type of the [`Constant`] is not [`Constant::Function`].
    /// Or `index` is greater or equal to length of `constants`.
    pub(crate) fn constant_function(&self, index: usize) -> Gc<Self> {
        if let Some(Constant::Function(value)) = self.body().constants.get(index) {
            return value.clone();
        }

        panic!("expected function constant at index {index}")
    }

    /// Get the [`Scope`] constant from the [`CodeBlock`].
    ///
    /// # Panics
    ///
    /// If the type of the [`Constant`] is not [`Constant::Scope`].
    /// Or `index` is greater or equal to length of `constants`.
    pub(crate) fn constant_scope(&self, index: usize) -> Scope {
        if let Some(Constant::Scope(value)) = self.body().constants.get(index) {
            return value.clone();
        }

        panic!("expected scope constant at index {index}")
    }

    pub(crate) fn source_info(&self) -> &SourceInfo {
        &self.body().source_info
    }

    pub(crate) fn get_next_codeblock_id() -> u64 {
        thread_local! {
            static CODEBLOCK_ID_COUNTER: Cell<u64> = const { Cell::new(0) };
        }

        CODEBLOCK_ID_COUNTER.with(|c| {
            let id = c.get();
            c.set(id + 1);
            id
        })
    }
}

/// ---- `CodeBlock` private API ----
impl CodeBlock {
    /// Get the operands after the `Opcode` pointed to by `pc` as a `String`.
    /// Modifies the `pc` to point to the next instruction.
    ///
    /// Returns an empty `String` if no operands are present.
    pub(crate) fn instruction_operands(&self, instruction: &Instruction) -> String {
        match instruction {
            Instruction::SetRegisterFromAccumulator { dst }
            | Instruction::PopIntoRegister { dst }
            | Instruction::StoreZero { dst }
            | Instruction::StoreOne { dst }
            | Instruction::StoreNan { dst }
            | Instruction::StorePositiveInfinity { dst }
            | Instruction::StoreNegativeInfinity { dst }
            | Instruction::StoreNull { dst }
            | Instruction::StoreTrue { dst }
            | Instruction::StoreFalse { dst }
            | Instruction::StoreUndefined { dst }
            | Instruction::Exception { dst }
            | Instruction::This { dst }
            | Instruction::NewTarget { dst }
            | Instruction::ImportMeta { dst }
            | Instruction::CreateMappedArgumentsObject { dst }
            | Instruction::CreateUnmappedArgumentsObject { dst }
            | Instruction::RestParameterInit { dst }
            | Instruction::StoreNewArray { dst } => format!("dst:{dst}"),
            Instruction::Add { lhs, rhs, dst }
            | Instruction::Sub { lhs, rhs, dst }
            | Instruction::Div { lhs, rhs, dst }
            | Instruction::Mul { lhs, rhs, dst }
            | Instruction::Mod { lhs, rhs, dst }
            | Instruction::Pow { lhs, rhs, dst }
            | Instruction::ShiftRight { lhs, rhs, dst }
            | Instruction::ShiftLeft { lhs, rhs, dst }
            | Instruction::UnsignedShiftRight { lhs, rhs, dst }
            | Instruction::BitOr { lhs, rhs, dst }
            | Instruction::BitAnd { lhs, rhs, dst }
            | Instruction::BitXor { lhs, rhs, dst }
            | Instruction::In { lhs, rhs, dst }
            | Instruction::Eq { lhs, rhs, dst }
            | Instruction::StrictEq { lhs, rhs, dst }
            | Instruction::NotEq { lhs, rhs, dst }
            | Instruction::StrictNotEq { lhs, rhs, dst }
            | Instruction::GreaterThan { lhs, rhs, dst }
            | Instruction::GreaterThanOrEq { lhs, rhs, dst }
            | Instruction::LessThan { lhs, rhs, dst }
            | Instruction::LessThanOrEq { lhs, rhs, dst }
            | Instruction::InstanceOf { lhs, rhs, dst } => {
                format!("lhs:{lhs}, rhs:{rhs}, dst:{dst}")
            }
            Instruction::InPrivate { dst, index, rhs } => {
                format!("rhs:{rhs}, index:{index}, dst:{dst}")
            }
            Instruction::Inc { src, dst }
            | Instruction::Dec { src, dst }
            | Instruction::Move { src, dst }
            | Instruction::ToPropertyKey { src, dst } => {
                format!("src:{src}, dst:{dst}")
            }
            Instruction::SetFunctionName {
                function,
                name,
                prefix,
            } => {
                format!(
                    "function:{function}, name:{name}, prefix:{}",
                    match u32::from(*prefix) {
                        0 => "prefix:",
                        1 => "prefix: get",
                        2 => "prefix: set",
                        _ => unreachable!(),
                    }
                )
            }
            Instruction::StoreInt8 { value, dst } => {
                format!("value:{value}, dst:{dst}")
            }
            Instruction::StoreInt16 { value, dst } => {
                format!("value:{value}, dst:{dst}")
            }
            Instruction::StoreInt32 { value, dst } => {
                format!("value:{value}, dst:{dst}")
            }
            Instruction::StoreFloat { value, dst } => {
                format!("value:{value}, dst:{dst}")
            }
            Instruction::StoreDouble { value, dst } => {
                format!("value:{value}, dst:{dst}")
            }
            Instruction::StoreLiteral { index, dst }
            | Instruction::ThisForObjectEnvironmentName { index, dst }
            | Instruction::GetFunction { index, dst }
            | Instruction::GetArgument { index, dst } => {
                format!("index:{index}, dst:{dst}")
            }
            Instruction::ThrowNewTypeError { message }
            | Instruction::ThrowNewReferenceError { message } => format!("message:{message}"),
            Instruction::StoreRegexp {
                pattern_index,
                flags_index,
                dst,
            } => {
                format!("pattern:{pattern_index}, flags:{flags_index}, dst:{dst}")
            }
            Instruction::Jump { address } => format!("address:{address}"),
            Instruction::JumpIfTrue { address, value }
            | Instruction::JumpIfFalse { address, value }
            | Instruction::JumpIfNotUndefined { address, value }
            | Instruction::JumpIfNullOrUndefined { address, value }
            | Instruction::LogicalAnd { address, value }
            | Instruction::LogicalOr { address, value }
            | Instruction::Coalesce { address, value } => {
                format!("value:{value}, address:{address}")
            }
            Instruction::JumpIfNotLessThan { address, lhs, rhs }
            | Instruction::JumpIfNotLessThanOrEqual { address, lhs, rhs }
            | Instruction::JumpIfNotGreaterThan { address, lhs, rhs }
            | Instruction::JumpIfNotGreaterThanOrEqual { address, lhs, rhs }
            | Instruction::JumpIfNotEqual { address, lhs, rhs } => {
                format!("lhs:{lhs}, rhs:{rhs}, address:{address}")
            }
            Instruction::Case {
                address,
                value,
                condition,
            } => {
                format!("value:{value}, condition:{condition}, address:{address}")
            }
            Instruction::CallEval {
                argument_count,
                scope_index,
            } => {
                format!("argument_count:{argument_count}, scope_index:{scope_index}")
            }
            Instruction::CallEvalSpread { scope_index }
            | Instruction::PushScope { scope_index } => {
                format!("scope_index:{scope_index}")
            }
            Instruction::Call { argument_count }
            | Instruction::New { argument_count }
            | Instruction::SuperCall { argument_count } => {
                format!("argument_count:{argument_count}")
            }
            Instruction::DefVar { binding_index }
            | Instruction::DefEvalVar { binding_index }
            | Instruction::GetLocator { binding_index } => format!("binding_index:{binding_index}"),
            Instruction::DefInitVar { src, binding_index }
            | Instruction::PutLexicalValue { src, binding_index }
            | Instruction::SetName { src, binding_index } => {
                format!("src:{src}, binding_index:{binding_index}")
            }
            Instruction::GetName { dst, binding_index }
            | Instruction::GetNameAndLocator { dst, binding_index }
            | Instruction::GetNameOrUndefined { dst, binding_index }
            | Instruction::DeleteName { dst, binding_index } => {
                format!("dst:{dst}, binding_index:{binding_index}")
            }
            Instruction::GetNameGlobal {
                dst,
                binding_index,
                ic_index,
            } => {
                format!("dst:{dst}, binding_index:{binding_index}, ic_index:{ic_index}")
            }
            Instruction::DefineOwnPropertyByName {
                object,
                value,
                name_index,
            }
            | Instruction::SetPropertyGetterByName {
                object,
                value,
                name_index,
            }
            | Instruction::SetPropertySetterByName {
                object,
                value,
                name_index,
            }
            | Instruction::DefinePrivateField {
                object,
                value,
                name_index,
            }
            | Instruction::SetPrivateMethod {
                object,
                value,
                name_index,
            }
            | Instruction::SetPrivateSetter {
                object,
                value,
                name_index,
            }
            | Instruction::SetPrivateGetter {
                object,
                value,
                name_index,
            }
            | Instruction::PushClassPrivateGetter {
                object,
                value,
                name_index,
            }
            | Instruction::PushClassPrivateSetter {
                object,
                value,
                name_index,
            }
            | Instruction::DefineClassStaticMethodByName {
                object,
                value,
                name_index,
            }
            | Instruction::DefineClassMethodByName {
                object,
                value,
                name_index,
            }
            | Instruction::DefineClassStaticGetterByName {
                object,
                value,
                name_index,
            }
            | Instruction::DefineClassGetterByName {
                object,
                value,
                name_index,
            }
            | Instruction::DefineClassStaticSetterByName {
                object,
                value,
                name_index,
            }
            | Instruction::DefineClassSetterByName {
                object,
                value,
                name_index,
            }
            | Instruction::SetPrivateField {
                object,
                value,
                name_index,
            }
            | Instruction::PushClassFieldPrivate {
                object,
                value,
                name_index,
            } => {
                format!("object:{object}, value:{value}, name_index:{name_index}")
            }
            Instruction::GetPrivateField {
                dst,
                object,
                name_index,
            } => {
                format!("dst:{dst}, object:{object}, name_index:{name_index}")
            }
            Instruction::PushClassPrivateMethod {
                object,
                proto,
                value,
                name_index,
            } => {
                format!("object:{object}, proto:{proto}, value:{value}, name_index:{name_index}")
            }
            Instruction::ThrowMutateImmutable { index } => {
                format!("index:{index}")
            }
            Instruction::DeletePropertyByName { object, name_index }
            | Instruction::GetMethod { object, name_index } => {
                format!("object:{object}, name_index:{name_index}")
            }
            Instruction::GetLengthProperty {
                dst,
                value,
                ic_index,
            }
            | Instruction::GetPropertyByName {
                dst,
                value,
                ic_index,
            } => {
                let ic = &self.body().ic_names[u32::from(*ic_index) as usize].display_escaped();
                format!("dst:{dst}, value:{value}, ic:{ic}")
            }
            Instruction::GetPropertyByNameWithThis {
                dst,
                receiver,
                value,
                ic_index,
            } => {
                let ic = &self.body().ic_names[u32::from(*ic_index) as usize].display_escaped();
                format!("dst:{dst}, receiver:{receiver}, value:{value}, ic:{ic}")
            }
            Instruction::SetPropertyByName {
                value,
                object,
                ic_index,
            } => {
                let ic = &self.body().ic_names[u32::from(*ic_index) as usize].display_escaped();
                format!("object:{object}, value:{value}, ic:{ic}")
            }
            Instruction::SetPropertyByNameWithThis {
                value,
                receiver,
                object,
                ic_index,
            } => {
                let ic = &self.body().ic_names[u32::from(*ic_index) as usize].display_escaped();
                format!("object:{object}, receiver:{receiver}, value:{value}, ic:{ic}")
            }
            Instruction::GetPropertyByValue {
                dst,
                key,
                receiver,
                object,
            }
            | Instruction::GetPropertyByValuePush {
                dst,
                key,
                receiver,
                object,
            } => {
                format!("dst:{dst}, object:{object}, receiver:{receiver}, key:{key}")
            }
            Instruction::SetPropertyByValue {
                value,
                key,
                receiver,
                object,
            } => {
                format!("object:{object}, receiver:{receiver}, key:{key}, value:{value}")
            }
            Instruction::DefineOwnPropertyByValue { value, key, object }
            | Instruction::DefineClassStaticMethodByValue { value, key, object }
            | Instruction::DefineClassMethodByValue { value, key, object }
            | Instruction::SetPropertyGetterByValue { value, key, object }
            | Instruction::DefineClassStaticGetterByValue { value, key, object }
            | Instruction::DefineClassGetterByValue { value, key, object }
            | Instruction::SetPropertySetterByValue { value, key, object }
            | Instruction::DefineClassStaticSetterByValue { value, key, object }
            | Instruction::DefineClassSetterByValue { value, key, object } => {
                format!("object:{object}, key:{key}, value:{value}")
            }
            Instruction::DeletePropertyByValue { key, object } => {
                format!("object:{object}, key:{key}")
            }
            Instruction::CreateIteratorResult { value, done } => {
                format!("value:{value}, done:{done}")
            }
            Instruction::StoreClassPrototype {
                dst,
                class,
                superclass,
            } => {
                format!("dst:{dst}, class:{class}, superclass:{superclass}")
            }
            Instruction::SetClassPrototype {
                dst,
                prototype,
                class,
            } => {
                format!("dst:{dst}, prototype:{prototype}, class:{class}")
            }
            Instruction::SetHomeObject { function, home } => {
                format!("function:{function}, home:{home}")
            }
            Instruction::GetHomeObject { function } => {
                format!("function:{function}")
            }
            Instruction::SetPrototype { object, prototype } => {
                format!("object:{object}, prototype:{prototype}")
            }
            Instruction::GetPrototype { object } => {
                format!("object:{object}")
            }
            Instruction::PushValueToArray { value, array } => {
                format!("value:{value}, array:{array}")
            }
            Instruction::PushElisionToArray { array }
            | Instruction::PushIteratorToArray { array } => {
                format!("array:{array}")
            }
            Instruction::TypeOf { value }
            | Instruction::LogicalNot { value }
            | Instruction::Pos { value }
            | Instruction::Neg { value }
            | Instruction::IsObject { value }
            | Instruction::BindThisValue { value }
            | Instruction::BitNot { value } => {
                format!("value:{value}")
            }
            Instruction::ImportCall {
                specifier,
                options,
                phase,
            } => {
                let phase_str = match u32::from(*phase) {
                    0 => "evaluation",
                    1 => "defer",
                    2 => "source",
                    _ => "unknown",
                };
                format!("specifier:{specifier}, options:{options}, phase:{phase_str}")
            }
            Instruction::PushClassField {
                object,
                name,
                value,
                is_anonymous_function,
            } => {
                format!(
                    "object:{object}, value:{value}, name:{name}, is_anonymous_function:{is_anonymous_function}"
                )
            }
            Instruction::MaybeException {
                has_exception,
                exception,
            } => {
                format!("has_exception:{has_exception}, exception:{exception}")
            }
            Instruction::SetAccumulator { src }
            | Instruction::PushFromRegister { src }
            | Instruction::Throw { src }
            | Instruction::SetNameByLocator { src }
            | Instruction::PushObjectEnvironment { src }
            | Instruction::CreateForInIterator { src }
            | Instruction::GetIterator { src }
            | Instruction::GetAsyncIterator { src }
            | Instruction::ValueNotNullOrUndefined { src }
            | Instruction::GeneratorYield { src }
            | Instruction::AsyncGeneratorYield { src }
            | Instruction::Await { src } => {
                format!("src:{src}")
            }
            Instruction::IteratorPush { iterator, next }
            | Instruction::IteratorPop { iterator, next } => {
                format!("iterator:{iterator}, next:{next}")
            }
            Instruction::IteratorUpdateResult { result } => {
                format!("result:{result}")
            }
            Instruction::IteratorDone { dst }
            | Instruction::IteratorValue { dst }
            | Instruction::IteratorResult { dst }
            | Instruction::IteratorToArray { dst }
            | Instruction::IteratorStackEmpty { dst }
            | Instruction::StoreEmptyObject { dst } => {
                format!("dst:{dst}")
            }
            Instruction::IteratorFinishAsyncNext { resume_kind, value } => {
                format!("resume_kind:{resume_kind}, value:{value}")
            }
            Instruction::IteratorReturn { value, called } => {
                format!("value:{value}, called:{called}")
            }
            Instruction::PushPrivateEnvironment {
                class,
                name_indices,
            } => {
                format!("class:{class}, names:{name_indices:?}")
            }
            Instruction::TemplateLookup { address, site, dst } => {
                format!("address:{address}, site:{site}, dst:{dst}")
            }
            Instruction::JumpTable { index, addresses } => {
                format!(
                    "index:{index}, jump_table:({})",
                    addresses.iter().format(", ")
                )
            }
            Instruction::ConcatToString { dst, values } => {
                format!("dst:{dst}, values:{values:?}")
            }
            Instruction::CopyDataProperties {
                object,
                source,
                excluded_keys,
            } => {
                format!("object:{object}, source:{source}, excluded_keys:{excluded_keys:?}")
            }
            Instruction::TemplateCreate { site, dst, values } => {
                format!("site:{site}, dst:{dst}, values:{values:?}")
            }
            Instruction::GetFunctionObject { function_object } => {
                format!("function_object:{function_object}")
            }
            Instruction::Pop
            | Instruction::DeleteSuperThrow
            | Instruction::ReThrow
            | Instruction::CheckReturn
            | Instruction::Return
            | Instruction::AsyncGeneratorClose
            | Instruction::CreatePromiseCapability
            | Instruction::PopEnvironment
            | Instruction::IncrementLoopIteration
            | Instruction::IteratorNext
            | Instruction::SuperCallDerived
            | Instruction::CallSpread
            | Instruction::NewSpread
            | Instruction::SuperCallSpread
            | Instruction::PopPrivateEnvironment
            | Instruction::Generator
            | Instruction::AsyncGenerator => String::new(),
            Instruction::Reserved1
            | Instruction::Reserved2
            | Instruction::Reserved3
            | Instruction::Reserved4
            | Instruction::Reserved5
            | Instruction::Reserved6
            | Instruction::Reserved7
            | Instruction::Reserved8
            | Instruction::Reserved9
            | Instruction::Reserved10
            | Instruction::Reserved11
            | Instruction::Reserved12
            | Instruction::Reserved13
            | Instruction::Reserved14
            | Instruction::Reserved15
            | Instruction::Reserved16
            | Instruction::Reserved17
            | Instruction::Reserved18
            | Instruction::Reserved19
            | Instruction::Reserved20
            | Instruction::Reserved21
            | Instruction::Reserved22
            | Instruction::Reserved23
            | Instruction::Reserved24
            | Instruction::Reserved25
            | Instruction::Reserved26
            | Instruction::Reserved27
            | Instruction::Reserved28
            | Instruction::Reserved29
            | Instruction::Reserved30
            | Instruction::Reserved31
            | Instruction::Reserved32
            | Instruction::Reserved33
            | Instruction::Reserved34
            | Instruction::Reserved35
            | Instruction::Reserved36
            | Instruction::Reserved37
            | Instruction::Reserved38
            | Instruction::Reserved39
            | Instruction::Reserved40
            | Instruction::Reserved41
            | Instruction::Reserved42
            | Instruction::Reserved43
            | Instruction::Reserved44
            | Instruction::Reserved45
            | Instruction::Reserved46
            | Instruction::Reserved47
            | Instruction::Reserved48
            | Instruction::Reserved49
            | Instruction::Reserved50
            | Instruction::Reserved51
            | Instruction::Reserved52
            | Instruction::Reserved53
            | Instruction::Reserved54
            | Instruction::Reserved55
            | Instruction::Reserved56
            | Instruction::Reserved57
            | Instruction::Reserved58
            | Instruction::Reserved59 => unreachable!("Reserved opcodes are unreachable"),
        }
    }
}

impl Display for CodeBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = self.name();
        writeln!(
            f,
            "{:-^80}",
            format!(
                " Compiled Output: {} ",
                if name.is_empty() {
                    format!("[anon#{}]", self.debug_id)
                } else {
                    format!("'{}'", name.to_std_string_escaped())
                }
            ),
        )?;
        writeln!(
            f,
            "Location     Handler      Opcode                            Operands"
        )?;
        let mut iterator = InstructionIterator::new(&self.bytecode);
        while let Some((instruction_start_pc, opcode, instruction)) = iterator.next() {
            let opcode = opcode.as_str();
            let operands = self.instruction_operands(&instruction);
            let pc = iterator.pc();
            let handler = if let Some((i, handler)) = self.find_handler(instruction_start_pc as u32)
            {
                let border_char = if instruction_start_pc as u32 == u32::from(handler.start) {
                    '>'
                } else if pc as u32 == u32::from(handler.end) {
                    '<'
                } else {
                    ' '
                };
                format!("{border_char}{i:2}: {}", handler.handler())
            } else {
                "           ".to_string()
            };
            writeln!(
                f,
                "  {instruction_start_pc:>06x}    {handler}     {opcode:<32}  {operands}",
            )?;
        }
        writeln!(
            f,
            "\nRegister Count: {}, Flags: {:?}",
            self.body().register_count,
            self.flags.get()
        )?;
        f.write_str("Constants:")?;
        if self.body().constants.is_empty() {
            f.write_str(" <empty>\n")?;
        } else {
            f.write_char('\n')?;
            for (i, value) in self.body().constants.iter().enumerate() {
                write!(f, "    {i:04}: ")?;
                match value {
                    Constant::String(v) => {
                        writeln!(
                            f,
                            "[STRING] \"{}\"",
                            v.to_std_string_escaped().escape_debug()
                        )?;
                    }
                    Constant::BigInt(v) => writeln!(f, "[BIGINT] {v}n")?,
                    Constant::Function(code) => writeln!(
                        f,
                        "[FUNCTION] name: '{}' (length: {})",
                        code.name().to_std_string_escaped(),
                        code.length
                    )?,
                    Constant::Scope(v) => {
                        writeln!(
                            f,
                            "[SCOPE] index: {}, bindings: {}",
                            v.scope_index(),
                            v.num_bindings()
                        )?;
                    }
                }
            }
        }
        f.write_str("Bindings:")?;
        if self.body().bindings.is_empty() {
            f.write_str(" <empty>\n")?;
        } else {
            f.write_char('\n')?;
            for (i, binding_locator) in self.body().bindings.iter().enumerate() {
                writeln!(
                    f,
                    "    {i:04}: {}, scope: {:?}",
                    binding_locator.name().to_std_string_escaped(),
                    binding_locator.scope()
                )?;
            }
        }
        f.write_str("Handlers:")?;
        if self.body().handlers.is_empty() {
            f.write_str(" <empty>\n")?;
        } else {
            f.write_char('\n')?;
            for (i, handler) in self.body().handlers.iter().enumerate() {
                writeln!(
                    f,
                    "    {i:04}: Range: [{:04}, {:04}): Handler: {:04}, Environment: {:02}",
                    handler.start,
                    handler.end,
                    handler.handler(),
                    handler.environment_count,
                )?;
            }
        }
        f.write_str("Source Map:")?;
        if self.source_info().map().entries().is_empty() {
            f.write_str(" <empty>\n")?;
        } else {
            f.write_char('\n')?;

            let bytecode_len = self.bytecode.len() as u32;
            for (i, handler) in self.source_info().map().entries().windows(2).enumerate() {
                let current = handler[0];
                let next = handler.get(1);

                write!(
                    f,
                    "    {i:04}: {:?}: ",
                    current.pc..next.map_or(bytecode_len, |entry| entry.pc),
                )?;

                if let Some(position) = current.position {
                    writeln!(
                        f,
                        "({}, {})",
                        position.line_number(),
                        position.column_number()
                    )?;
                } else {
                    f.write_str("unknown")?;
                }
            }
        }
        Ok(())
    }
}

/// Creates a new function object.
///
/// This is used in cases that the prototype is not known if it's [`None`] or [`Some`].
///
/// If the prototype given is [`None`] it will use [`create_function_object_fast`]. Otherwise
/// it will construct the function from template objects that have all the fields except the
/// prototype, and will perform a prototype transition change to set the prototype.
///
/// This is slower than direct object template construction that is done in [`create_function_object_fast`].
pub(crate) fn create_function_object(
    code: Gc<CodeBlock>,
    prototype: JsObject,
    context: &mut Context,
) -> JsObject {
    let name: JsValue = code.name().clone().into();
    let length: JsValue = code.length.into();

    let script_or_module = context.get_active_script_or_module();

    let is_async = code.is_async();
    let is_generator = code.is_generator();
    let function = OrdinaryFunction::new(
        code,
        context.vm.frame().environments.snapshot_for_closure(),
        script_or_module,
        context.realm().clone(),
    );

    let templates = context.intrinsics().templates();

    let (mut template, storage, constructor_prototype) = if is_generator {
        let prototype = JsObject::from_proto_and_data_with_shared_shape(
            context.root_shape(),
            if is_async {
                context.intrinsics().objects().async_generator()
            } else {
                context.intrinsics().objects().generator()
            },
            OrdinaryObject,
        );

        (
            templates.function_with_prototype_without_proto().clone(),
            vec![length, name, prototype.into()],
            None,
        )
    } else if is_async {
        (
            templates.function_without_proto().clone(),
            vec![length, name],
            None,
        )
    } else {
        let constructor_prototype = templates
            .function_prototype()
            .create(OrdinaryObject, vec![JsValue::undefined()]);

        let template = templates.function_with_prototype_without_proto();

        (
            template.clone(),
            vec![length, name, constructor_prototype.clone().into()],
            Some(constructor_prototype),
        )
    };

    template.set_prototype(prototype);

    let constructor = template.create(function, storage);

    if let Some(constructor_prototype) = &constructor_prototype {
        constructor_prototype.borrow_mut().properties_mut().storage[0] = constructor.clone().into();
    }
    constructor
}

/// Creates a new function object.
///
/// This is preferred over [`create_function_object`] if prototype is [`None`],
/// because it constructs the function from a pre-initialized object template,
/// with all the properties and prototype set.
pub(crate) fn create_function_object_fast(code: Gc<CodeBlock>, context: &mut Context) -> JsObject {
    let name: JsValue = code.name().clone().into();
    let length: JsValue = code.length.into();

    let script_or_module = context.get_active_script_or_module();

    let is_async = code.is_async();
    let is_generator = code.is_generator();
    let has_prototype_property = code.has_prototype_property();
    let function = OrdinaryFunction::new(
        code,
        context.vm.frame().environments.snapshot_for_closure(),
        script_or_module,
        context.realm().clone(),
    );

    if is_generator {
        let prototype = JsObject::from_proto_and_data_with_shared_shape(
            context.root_shape(),
            if is_async {
                context.intrinsics().objects().async_generator()
            } else {
                context.intrinsics().objects().generator()
            },
            OrdinaryObject,
        );
        let template = if is_async {
            context.intrinsics().templates().async_generator_function()
        } else {
            context.intrinsics().templates().generator_function()
        };

        template.create(function, vec![length, name, prototype.into()])
    } else if is_async {
        context
            .intrinsics()
            .templates()
            .async_function()
            .create(function, vec![length, name])
    } else if !has_prototype_property {
        context
            .intrinsics()
            .templates()
            .function()
            .create(function, vec![length, name])
    } else {
        let prototype = context
            .intrinsics()
            .templates()
            .function_prototype()
            .create(OrdinaryObject, vec![JsValue::undefined()]);

        let constructor = context
            .intrinsics()
            .templates()
            .function_with_prototype()
            .create(function, vec![length, name, prototype.clone().into()]);

        prototype.borrow_mut().properties_mut().storage[0] = constructor.clone().into();

        constructor
    }
}
