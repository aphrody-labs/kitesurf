//! Kitesurf: moves never-run function bodies, with everything nested in them, to the code
//! store as one record per block, and rebuilds a body (and shells for its nested blocks) on first read.

use std::rc::Rc;

use boa_ast::scope::{BindingLocator, KsScopeParts, Scope};
use boa_ast::{LinearPosition, LinearSpan};
use boa_gc::Gc;
use boa_string::{JsStr, JsStrVariant, JsString};
use rustc_hash::{FxHashMap, FxHashSet};
use thin_vec::ThinVec;

use crate::{
    CodeStore, JsBigInt, SpannedSourceText,
    builtins::function::ThisMode,
    vm::{
        BodyState, CodeBlock, CodeBlockFlags, CodeBody, Constant, GlobalFunctionBinding, Handler,
        opcode::{Address, Bytecode, KsBytecodeExport},
        source_info::{SourceInfo, SourceMap, SourcePath},
    },
};

const VERSION: u8 = 1;
// Kitesurf: a subtree nested deeper than this stays resident, since collecting and dumping
// it recurse on the shadow stack.
const MAX_DEPTH: usize = 256;

/// Kitesurf: where a swapped body is in the store, and what rebuilding it needs that its
/// record lacks: the scopes it hangs from and the script's path.
pub(crate) struct BodyDesc {
    key: u32,
    len: u32,
    hash: u32,
    scopes: ScopeCtx,
    path: SourcePath,
}

// Kitesurf: a subtree's root rebuilds the scope table under its anchor; the blocks below it
// index the table the root's fault-in built.
enum ScopeCtx {
    Anchor(Option<Scope>),
    Table(Rc<[Scope]>),
}

impl BodyDesc {
    fn new(key: u32, len: u32, hash: u32, scopes: ScopeCtx, path: SourcePath) -> Self {
        crate::ks_code_stats::update(|s| s.bodies_swapped += 1);
        Self { key, len, hash, scopes, path }
    }
}

impl Drop for BodyDesc {
    fn drop(&mut self) {
        crate::ks_code_stats::update(|s| s.bodies_swapped -= 1);
        if let Some(store) = crate::ks_code_store::current() {
            store.release(self.key, self.len as usize);
        }
    }
}

fn fnv1a(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0x811c_9dc5_u32, |h, &b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193))
}

#[derive(Default)]
struct Writer {
    out: Vec<u8>,
}

impl Writer {
    fn u8(&mut self, v: u8) {
        self.out.push(v);
    }
    fn var(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.out.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.out.push(v as u8);
    }
    fn u32(&mut self, v: u32) {
        self.var(u64::from(v));
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.out.extend_from_slice(b);
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn u8(&mut self) -> u8 {
        let v = self.buf[self.pos];
        self.pos += 1;
        v
    }
    fn var(&mut self) -> u64 {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let b = self.u8();
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return v;
            }
            shift += 7;
        }
    }
    fn u32(&mut self) -> u32 {
        self.var() as u32
    }
    fn bytes(&mut self) -> &[u8] {
        let n = self.u32() as usize;
        let b = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        b
    }
}

// The strings a record uses, each written once ahead of the record's body and named by
// its position from then on.
#[derive(Default)]
struct Strings {
    index: FxHashMap<JsString, u32>,
    order: Vec<JsString>,
}

impl Strings {
    fn id(&mut self, s: &JsString) -> u32 {
        if let Some(&i) = self.index.get(s) {
            return i;
        }
        let i = self.order.len() as u32;
        self.index.insert(s.clone(), i);
        self.order.push(s.clone());
        i
    }
    fn write(&self, w: &mut Writer) {
        w.u32(self.order.len() as u32);
        for s in &self.order {
            match s.as_str().variant() {
                JsStrVariant::Latin1(b) => {
                    w.u8(0);
                    w.bytes(b);
                }
                JsStrVariant::Utf16(units) => {
                    w.u8(1);
                    w.u32(units.len() as u32);
                    for &u in units {
                        w.out.extend_from_slice(&u.to_le_bytes());
                    }
                }
            }
        }
    }
    fn read(r: &mut Reader<'_>) -> Vec<JsString> {
        let n = r.u32() as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            if r.u8() == 0 {
                out.push(JsString::from(JsStr::latin1(r.bytes())));
            } else {
                let len = r.u32() as usize;
                let raw = &r.buf[r.pos..r.pos + len * 2];
                r.pos += len * 2;
                let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                out.push(JsString::from(JsStr::utf16(&units)));
            }
        }
        out
    }
}

fn this_mode_tag(mode: &ThisMode) -> u8 {
    match mode {
        ThisMode::Lexical => 0,
        ThisMode::Strict => 1,
        ThisMode::Global => 2,
    }
}

fn this_mode_of(tag: u8) -> ThisMode {
    match tag {
        0 => ThisMode::Lexical,
        1 => ThisMode::Strict,
        _ => ThisMode::Global,
    }
}

// What a parent's record says about a nested block: its own record, and where its
// bytecode is, which the shell a fault-in builds will own.
struct ChildRecord {
    key: u32,
    len: u32,
    hash: u32,
    bytecode: (KsBytecodeExport, u32),
}

// The scope table of a subtree, outer scopes first, keyed by identity: only the root's
// record carries it, and the blocks under the root index it.
struct ScopeTable {
    scopes: Vec<Scope>,
    index: FxHashMap<usize, u32>,
    anchor: Option<Scope>,
}

impl ScopeTable {
    // Every scope the subtree's constants hold, plus those between them and the anchor.
    // None when the chains leave the subtree at more than one scope, or reach the top.
    fn build(blocks: &[&CodeBlock], outside: &FxHashSet<usize>) -> Option<Self> {
        let is_outside = |s: &Scope| s.is_global() || outside.contains(&s.ks_ptr());
        let mut found: FxHashMap<usize, Scope> = FxHashMap::default();
        let mut anchor: Option<Scope> = None;
        for block in blocks {
            for constant in &block.resident_body()?.constants {
                let Constant::Scope(scope) = constant else { continue };
                if is_outside(scope) {
                    return None;
                }
                let mut cur = Some(scope.clone());
                while let Some(s) = cur {
                    if found.contains_key(&s.ks_ptr()) {
                        break;
                    }
                    if is_outside(&s) {
                        match &anchor {
                            Some(a) if a.ks_ptr() != s.ks_ptr() => return None,
                            Some(_) => {}
                            None => anchor = Some(s),
                        }
                        break;
                    }
                    cur = s.outer().cloned();
                    if cur.is_none() {
                        return None;
                    }
                    found.insert(s.ks_ptr(), s);
                }
            }
        }
        let depth = |s: &Scope| {
            let mut d = 0usize;
            let mut cur = s.outer();
            while let Some(o) = cur {
                d += 1;
                cur = o.outer();
            }
            d
        };
        let mut scopes: Vec<Scope> = found.into_values().collect();
        scopes.sort_by_cached_key(|s| (depth(s), s.unique_id()));
        let index = scopes.iter().enumerate().map(|(i, s)| (s.ks_ptr(), i as u32)).collect();
        Some(Self { scopes, index, anchor })
    }

    // How an entry names its outer scope: 0 for none, 1 for the anchor, 2 + i for the
    // table's scope i, which sorting outer-first has already rebuilt.
    fn outer_ref(&self, scope: &Scope) -> u32 {
        match scope.outer() {
            None => 0,
            Some(o) if self.anchor.as_ref().is_some_and(|a| a.ks_ptr() == o.ks_ptr()) => 1,
            Some(o) => 2 + self.index[&o.ks_ptr()],
        }
    }

    fn write(&self, w: &mut Writer, strings: &mut Strings) {
        w.u32(self.scopes.len() as u32);
        for scope in &self.scopes {
            let parts = scope.ks_parts();
            w.u32(self.outer_ref(scope));
            w.u32(parts.unique_id);
            w.u32(parts.index);
            w.u8(u8::from(parts.function) | (u8::from(parts.this_escaped) << 1));
            w.u32(parts.bindings.len() as u32);
            for (name, index, flags) in &parts.bindings {
                w.u32(strings.id(name));
                w.u32(*index);
                w.u8(*flags);
            }
        }
    }

    fn read(r: &mut Reader<'_>, strings: &[JsString], anchor: Option<&Scope>) -> Vec<Scope> {
        let n = r.u32() as usize;
        let mut scopes: Vec<Scope> = Vec::with_capacity(n);
        for _ in 0..n {
            let outer = match r.u32() {
                0 => None,
                1 => anchor.cloned(),
                i => Some(scopes[i as usize - 2].clone()),
            };
            let unique_id = r.u32();
            let index = r.u32();
            let flags = r.u8();
            let count = r.u32() as usize;
            let bindings = (0..count)
                .map(|_| (strings[r.u32() as usize].clone(), r.u32(), r.u8()))
                .collect();
            let parts = KsScopeParts {
                unique_id,
                index,
                function: flags & 1 != 0,
                this_escaped: flags & 2 != 0,
                bindings,
            };
            scopes.push(Scope::ks_rebuild(outer, parts));
        }
        scopes
    }
}

fn write_body(
    block: &CodeBlock,
    body: &CodeBody,
    children: &FxHashMap<usize, ChildRecord>,
    table: &ScopeTable,
    w: &mut Writer,
    strings: &mut Strings,
) {
    w.u32(body.parameter_length);
    w.u32(body.register_count);
    w.u32(body.mapped_arguments_binding_indices.len() as u32);
    for i in &body.mapped_arguments_binding_indices {
        w.u32(i.map_or(0, |i| i + 1));
    }
    w.u32(body.constants.len() as u32);
    for constant in &body.constants {
        match constant {
            Constant::String(s) => {
                w.u8(0);
                w.u32(strings.id(s));
            }
            Constant::Function(child) => {
                let rec = &children[&(std::ptr::from_ref::<CodeBlock>(child) as usize)];
                w.u8(1);
                w.u32(rec.key);
                w.u32(rec.len);
                w.u32(rec.hash);
                w.u32(u32::from((child.flags.get() - CodeBlockFlags::KS_RAN).bits()));
                w.u32(child.length);
                w.u8(this_mode_tag(&child.this_mode));
                w.u32(strings.id(&child.name));
                match child.text_spanned.span() {
                    Some(span) => {
                        w.u8(1);
                        w.var(span.start().pos() as u64);
                        w.var(span.end().pos() as u64);
                    }
                    None => w.u8(0),
                }
                w.var(child.debug_id);
                match &rec.bytecode.0 {
                    KsBytecodeExport::Desc { key, len, hash } => {
                        w.u8(0);
                        w.u32(*key);
                        w.u32(*len);
                        w.u32(*hash);
                    }
                    KsBytecodeExport::Inline(bytes) => {
                        w.u8(1);
                        w.bytes(bytes);
                    }
                }
                w.u32(rec.bytecode.1);
            }
            Constant::BigInt(n) => {
                w.u8(2);
                w.bytes(n.to_string_radix(10).as_bytes());
            }
            Constant::Scope(s) => {
                w.u8(3);
                w.u32(table.index[&s.ks_ptr()]);
            }
        }
    }
    w.u32(body.bindings.len() as u32);
    for locator in &body.bindings {
        let (name, scope, index, unique) = locator.ks_parts();
        w.u32(strings.id(name));
        w.u32(scope);
        w.u32(index);
        w.u32(unique);
    }
    w.u32(body.handlers.len() as u32);
    for h in &body.handlers {
        w.u32(u32::from(h.start));
        w.u32(u32::from(h.end));
        w.u32(h.environment_count);
    }
    w.u32(body.ic_names.len() as u32);
    for name in &body.ic_names {
        w.u32(strings.id(name));
    }
    let (data, checkpoints) = body.source_info.map().ks_parts();
    w.bytes(data);
    let checkpoints: Vec<[u32; 4]> = checkpoints.collect();
    w.u32(checkpoints.len() as u32);
    for c in checkpoints {
        c.iter().for_each(|&v| w.u32(v));
    }
    w.u32(body.global_lexs.len() as u32);
    body.global_lexs.iter().for_each(|&v| w.u32(v));
    w.u32(body.global_fns.len() as u32);
    for f in &body.global_fns {
        w.u32(f.name_index);
        w.u32(f.function_index);
    }
    w.u32(body.global_vars.len() as u32);
    body.global_vars.iter().for_each(|&v| w.u32(v));
    let _ = block;
}

// A record: version, whether it carries the scope table, the strings, the table, then
// the body. Only a subtree's root record carries the table.
fn record(
    block: &CodeBlock,
    body: &CodeBody,
    children: &FxHashMap<usize, ChildRecord>,
    table: &ScopeTable,
    with_table: bool,
) -> Vec<u8> {
    let mut strings = Strings::default();
    let mut scopes = Writer::default();
    if with_table {
        table.write(&mut scopes, &mut strings);
    }
    let mut rest = Writer::default();
    write_body(block, body, children, table, &mut rest, &mut strings);
    let mut w = Writer::default();
    w.u8(VERSION);
    w.u8(u8::from(with_table));
    strings.write(&mut w);
    w.out.extend_from_slice(&scopes.out);
    w.out.extend_from_slice(&rest.out);
    w.out
}

fn collect<'a>(block: &'a CodeBlock, out: &mut Vec<&'a CodeBlock>, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    out.push(block);
    let Some(body) = block.resident_body() else {
        return false;
    };
    for constant in &body.constants {
        if let Constant::Function(child) = constant {
            if child.ks_ran() || !collect(child, out, depth + 1) {
                return false;
            }
        }
    }
    true
}

/// Kitesurf: moves never-run `block`'s body and everything nested in it to `store`, leaving
/// a shell. `outside` holds the scopes of the blocks above it that ran. Returns the records.
pub(crate) fn swap_out(block: &CodeBlock, store: &dyn CodeStore, outside: &FxHashSet<usize>) -> usize {
    let skip = || {
        crate::ks_code_stats::update(|s| s.body_skips += 1);
        0
    };
    if block.ks_ran() {
        return 0;
    }
    let mut blocks = Vec::new();
    if block.resident_body().is_none() || !collect(block, &mut blocks, 0) {
        return skip();
    }
    let path = block.body().source_info.map().path().clone();
    for nested in &blocks[1..] {
        if !nested.text_spanned.ks_same_source(&block.text_spanned)
            || *nested.body().source_info.map().path() != path
        {
            return skip();
        }
    }
    let Some(table) = ScopeTable::build(&blocks, outside) else {
        return skip();
    };
    // Children before their parents, so each parent's record names its children's keys. A
    // failed put leaves every block as it was; what went to the store is never read.
    let mut children: FxHashMap<usize, ChildRecord> = FxHashMap::default();
    let mut root: Option<(u32, u32, u32)> = None;
    let mut written = 0usize;
    for (i, b) in blocks.iter().enumerate().rev() {
        let body = b.body();
        let bytes = record(b, body, &children, &table, i == 0);
        let hash = fnv1a(&bytes);
        let Some(key) = store.put(&bytes) else {
            return skip();
        };
        written += bytes.len();
        if i == 0 {
            root = Some((key, bytes.len() as u32, hash));
        } else {
            let Some(bytecode) = b.bytecode.ks_export(store) else {
                return skip();
            };
            children.insert(
                std::ptr::from_ref::<CodeBlock>(b) as usize,
                ChildRecord { key, len: bytes.len() as u32, hash, bytecode },
            );
        }
    }
    let Some((key, len, hash)) = root else {
        return skip();
    };
    for b in &blocks[1..] {
        b.bytecode.ks_disown();
    }
    let n = blocks.len() as i64;
    let desc = BodyDesc::new(key, len, hash, ScopeCtx::Anchor(table.anchor.clone()), path);
    drop(blocks);
    drop(children);
    // SAFETY: an idle round runs no JS, and the references taken above are gone; the block
    // never ran, so no frame holds its body either.
    let old = unsafe { block.replace_body(BodyState::Swapped(Box::new(desc))) };
    block.ks_charge.recharge(CodeBlock::ks_shell_bytes(), 0);
    drop(old);
    crate::ks_code_stats::update(|s| {
        s.body_records += n;
        s.body_record_bytes += crate::ks_code_stats::bytes(written);
    });
    written
}

#[cold]
#[inline(never)]
pub(crate) fn fault_in(block: &CodeBlock) -> &CodeBody {
    let Some(desc) = block.body_desc() else {
        return block.resident_body().expect("a block's body is resident or swapped");
    };
    let mut bytes = Vec::new();
    let ok = crate::ks_code_store::current().is_some_and(|store| {
        store.read(desc.key, 0, desc.len as usize, &mut bytes)
            && bytes.len() == desc.len as usize
            && fnv1a(&bytes) == desc.hash
    });
    if !ok {
        crate::ks_code_stats::update(|s| s.store_failures += 1);
        panic!("kitesurf: a swapped function body could not be read back intact");
    }
    let path = desc.path.clone();
    let mut r = Reader { buf: &bytes, pos: 0 };
    assert_eq!(r.u8(), VERSION, "kitesurf: a body record of another version");
    let with_table = r.u8() != 0;
    let strings = Strings::read(&mut r);
    let table: Rc<[Scope]> = match (&desc.scopes, with_table) {
        (ScopeCtx::Anchor(anchor), true) => ScopeTable::read(&mut r, &strings, anchor.as_ref()).into(),
        (ScopeCtx::Table(table), false) => table.clone(),
        _ => panic!("kitesurf: a body record that does not match its descriptor"),
    };
    let body = read_body(block, &mut r, &strings, &table, &path);
    let fixed = size_of::<CodeBlock>() + body.ks_fixed_bytes();
    let map = body.source_info.map().ks_heap_bytes();
    // SAFETY: the body was swapped, so no reference into it exists, and `desc` (the only
    // reference taken above) is not used past this point.
    let old = unsafe { block.replace_body(BodyState::Resident(Box::new(body))) };
    drop(old);
    block.ks_charge.recharge(fixed, map);
    crate::ks_code_stats::update(|s| s.body_faults += 1);
    block.resident_body().expect("installed just above")
}

fn read_body(block: &CodeBlock, r: &mut Reader<'_>, strings: &[JsString], table: &Rc<[Scope]>, path: &SourcePath) -> CodeBody {
    let parameter_length = r.u32();
    let register_count = r.u32();
    let mapped_arguments_binding_indices = (0..r.u32())
        .map(|_| r.u32().checked_sub(1))
        .collect::<ThinVec<_>>();
    let n = r.u32() as usize;
    let mut constants = ThinVec::with_capacity(n);
    for _ in 0..n {
        let constant = match r.u8() {
            0 => Constant::String(strings[r.u32() as usize].clone()),
            1 => {
                let (key, len, hash) = (r.u32(), r.u32(), r.u32());
                let flags = CodeBlockFlags::from_bits_retain(r.u32() as u16);
                let length = r.u32();
                let this_mode = this_mode_of(r.u8());
                let name = strings[r.u32() as usize].clone();
                let span = (r.u8() != 0).then(|| {
                    let start = LinearPosition::new(r.var() as usize);
                    LinearSpan::new(start, LinearPosition::new(r.var() as usize))
                });
                let debug_id = r.var();
                let export = if r.u8() == 0 {
                    KsBytecodeExport::Desc { key: r.u32(), len: r.u32(), hash: r.u32() }
                } else {
                    KsBytecodeExport::Inline(r.bytes().into())
                };
                let bytecode = Bytecode::ks_import(export, r.u32());
                let desc = BodyDesc::new(key, len, hash, ScopeCtx::Table(table.clone()), path.clone());
                let text = SpannedSourceText::new(block.text_spanned.source_text(), span);
                Constant::Function(Gc::new(CodeBlock::ks_shell(
                    flags, length, this_mode, name, text, bytecode, debug_id, desc,
                )))
            }
            2 => {
                let digits = std::str::from_utf8(r.bytes()).unwrap_or_default();
                Constant::BigInt(JsBigInt::from_string(digits).expect("a stored BigInt"))
            }
            _ => Constant::Scope(table[r.u32() as usize].clone()),
        };
        constants.push(constant);
    }
    let bindings = (0..r.u32())
        .map(|_| {
            let name = strings[r.u32() as usize].clone();
            BindingLocator::ks_from_parts(name, r.u32(), r.u32(), r.u32())
        })
        .collect::<Box<[_]>>();
    let handlers = (0..r.u32())
        .map(|_| Handler {
            start: Address::from(r.u32()),
            end: Address::from(r.u32()),
            environment_count: r.u32(),
        })
        .collect::<ThinVec<_>>();
    let ic_names = (0..r.u32())
        .map(|_| strings[r.u32() as usize].clone())
        .collect::<Box<[_]>>();
    let data: Box<[u8]> = r.bytes().into();
    let checkpoints: Vec<[u32; 4]> = (0..r.u32()).map(|_| [r.u32(), r.u32(), r.u32(), r.u32()]).collect();
    let source_info = SourceInfo::new(SourceMap::ks_from_parts(data, &checkpoints, path.clone()), block.name.clone());
    let global_lexs = (0..r.u32()).map(|_| r.u32()).collect::<Box<[_]>>();
    let global_fns = (0..r.u32())
        .map(|_| GlobalFunctionBinding { name_index: r.u32(), function_index: r.u32() })
        .collect::<Box<[_]>>();
    let global_vars = (0..r.u32()).map(|_| r.u32()).collect::<Box<[_]>>();
    CodeBody {
        parameter_length,
        register_count,
        mapped_arguments_binding_indices,
        constants,
        bindings,
        handlers,
        ic_names,
        ic: std::cell::OnceCell::new(),
        source_info,
        global_lexs,
        global_fns,
        global_vars,
    }
}

/// Kitesurf: a compiled tree as text (each block's disassembly and its scopes' parts),
/// fetching swapped bodies; tests compare a swapped tree with one that never left memory.
#[doc(hidden)]
#[must_use]
pub fn ks_debug_dump(block: &CodeBlock) -> String {
    let mut out = String::new();
    dump(block, &mut out, 0);
    out
}

fn dump(block: &CodeBlock, out: &mut String, depth: usize) {
    use std::fmt::Write;
    let _ = write!(out, "{block}");
    for constant in &block.body().constants {
        if let Constant::Scope(scope) = constant {
            let parts = scope.ks_parts();
            let mut chain = 0;
            let mut cur = scope.outer();
            while let Some(o) = cur {
                chain += 1;
                cur = o.outer();
            }
            let _ = writeln!(out, "scope depth {chain}: {parts:?}");
        }
    }
    if depth < MAX_DEPTH {
        for constant in &block.body().constants {
            if let Constant::Function(child) = constant {
                dump(child, out, depth + 1);
            }
        }
    }
}
