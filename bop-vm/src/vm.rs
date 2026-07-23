//! Bytecode VM execution.
//!
//! This is step 2b of the execution-modes roadmap: a stack-based
//! dispatch loop that walks a [`Chunk`] produced by [`crate::compile`]
//! and produces the same observable behaviour as the tree-walking
//! evaluator in `bop-lang`.
//!
//! # Stack model
//!
//! All runtime values live on a single [`Slot`] stack. A [`Slot`] is
//! either a [`Value`], an in-progress `for`-loop iterator, or an
//! in-progress `repeat` counter. Iterators and counters are sidecar
//! items pushed by [`Instr::MakeIter`] / [`Instr::MakeRepeatCount`]
//! and consumed by [`Instr::IterNext`] / [`Instr::RepeatNext`] on
//! exhaustion or [`Instr::PopLoopState`] on `break`; only bytecode that
//! participates in iteration ever sees them, so the rest of the dispatch
//! loop treats the stack as a stack of `Value`.
//!
//! # Frames
//!
//! Each function call — including the top-level program — runs in its
//! own [`Frame`]. A frame owns its chunk (wrapped in `Rc` so repeated
//! calls share the compiled code), its instruction pointer, and its
//! lexical scope stack. Returning from a function pops the frame and
//! truncates the value stack back to the frame's base in case the body
//! left anything behind.
//!
//! # Resource limits
//!
//! [`BopLimits`] is enforced exactly as in the tree-walker:
//!
//! - A tick fires at every bytecode dispatch. It bumps `steps`,
//!   checks `max_steps`, checks [`bop::memory::bop_memory_exceeded`],
//!   and invokes [`BopHost::on_tick`]. `max_memory` is shared with the
//!   tree-walker via the per-value allocation tracking in
//!   [`bop::memory`], so no VM-specific bookkeeping is needed.
//! - The VM scales `max_steps` internally by [`STEP_SCALE`] so a single
//!   source-level step, which typically maps to several bytecode ops,
//!   still fits under the tree-walker's calibration of
//!   `BopLimits::standard()` / `BopLimits::demo()`.

#[cfg(feature = "no_std")]
use alloc::{
    format,
    rc::Rc,
    string::{String, ToString},
    vec,
    vec::Vec,
};

#[cfg(not(feature = "no_std"))]
use std::rc::Rc;

use core::cell::{Cell, RefCell};

#[cfg(not(feature = "no_std"))]
use std::collections::{BTreeMap, BTreeSet};
#[cfg(feature = "no_std")]
use alloc::collections::{BTreeMap, BTreeSet};

use bop::builtins::{self, error, error_fatal_with_hint, error_with_hint};
use bop::error::{BopError, BopWarning};
use bop::methods;
use bop::ops;
use bop::parser::Visibility;
use bop::value::{BopFn, BopFnOrigin, FnBody, Value};
use bop::{BopHost, BopLimits, EntryPoint};

use crate::chunk::{
    CaptureSource, Chunk, CodeOffset, Constant, EnumConstructShape, EnumIdx, EnumVariantShape,
    FnDef, FnIdx, Instr, InterpPart, LoopStateKind, NameIdx, PatternIdx, StructIdx,
};

/// Hard cap on call depth; matches the tree-walker.
const MAX_CALL_DEPTH: usize = 64;

/// `max_steps` is a source-level budget. One source-level statement
/// typically maps to several bytecode instructions, so the dispatch
/// loop gets a proportionally larger internal budget. Calibrated so
/// that `while true { }` still halts under `BopLimits::demo()` and
/// small programs like fizzbuzz don't exhaust `standard()`.
const STEP_SCALE: u64 = 8;

/// Memory-limit check cadence. Checked every `1 << N` ticks; a
/// power-of-two window lets us AND with a mask instead of
/// dividing. 256 is a sweet spot — detection still lands within
/// microseconds of the limit being breached, and a tight
/// arithmetic loop pays the TLS load once per 256 instructions
/// instead of every instruction.
const TICK_MEMCHECK_MASK: u64 = 0xFF;

/// Maximum slot vecs kept on the freelist. Deep recursion with
/// varying slot counts could otherwise grow this unboundedly.
/// `MAX_CALL_DEPTH` (64) plus a little headroom is plenty — any
/// extra allocations just go back through the global allocator
/// like before.
const SLOTS_FREELIST_CAP: usize = 128;

// ─── Stack slot ────────────────────────────────────────────────────

enum Slot {
    Value(Value),
    /// Remaining items in reverse order — `pop()` yields the next one.
    /// Eager fast path for `for x in <array | string | dict>`.
    Iter(Vec<Value>),
    /// Iterator protocol state: holds a value whose `.next()`
    /// method returns `Iter::Next(v)` / `Iter::Done`. Produced
    /// when the for-loop's iterable is a `Value::Iter` or a
    /// user type; the `IterNext` opcode dispatches `.next()`
    /// through the regular method path.
    IterObject(Value),
    /// Remaining iterations for a `repeat` loop.
    Repeat(i64),
}

// ─── Frame ─────────────────────────────────────────────────────────

struct FrameScopeBases {
    types: usize,
    aliases: usize,
}

struct FunctionFrameContext {
    scope_bases: FrameScopeBases,
    function_module: Option<String>,
    lexical_context: Rc<ModuleLexicalContext>,
}

struct Frame {
    chunk: Rc<Chunk>,
    ip: usize,
    /// Flat slot array for this function's compile-time-resolved
    /// locals (params + every `let` / `for-in` variable assigned
    /// a slot). `LoadLocal(slot)` / `StoreLocal(slot)` read and
    /// write directly into this vec — the VM's hot path for
    /// variable access. Empty for module-top-level frames and
    /// match-body sub-scopes where slot resolution doesn't apply.
    slots: Vec<Value>,
    /// Slow-path BTreeMap scope stack. Used for module-top-level
    /// bindings, match-pattern bindings, captures snapshotted
    /// into lambdas, and anything else that reaches `LoadVar` /
    /// `DefineLocal` / `StoreVar`. Function frames that stay on
    /// the fast path never push into this at runtime.
    scopes: Vec<BTreeMap<String, Value>>,
    /// Number of value scopes owned by the frame at entry. `PopScope`
    /// may only remove scopes above this floor: the top-level frame and
    /// closures retain their initial scope, while named-function and method
    /// frames start at zero.
    scope_base: usize,
    /// Lazily inserted protected value scope for assignments that shadow an
    /// inherited declaration alias. It lives for the whole call even when
    /// the assignment occurs inside a nested runtime scope.
    has_declaration_overlay: bool,
    /// Protected depth of the VM-wide `type_bindings` stack at frame entry.
    /// Runtime scopes live above this depth and are paired with `scopes`.
    /// Function exit truncates back to the caller depth so an early return
    /// cannot leak type scopes whose `PopScope` was skipped.
    type_scope_base: usize,
    /// Protected depth of the VM-wide module-alias/imported-callable stacks at
    /// frame entry. Function lookup sees frame zero plus frames at/above it.
    alias_scope_base: usize,
    stack_base: usize,
    is_function: bool,
    /// Defining module for named-function and module-exported closure bodies.
    /// Used to resolve private sibling function bindings from the module cache.
    function_module: Option<String>,
    /// Immutable defining-module namespace shared by every call to this
    /// function. Mutable block/function-local declarations remain in the VM
    /// stacks above `alias_scope_base`.
    lexical_context: Rc<ModuleLexicalContext>,
    /// How this frame's return value should be transformed
    /// before being pushed for the caller. See [`FrameWrap`].
    wrap: FrameWrap,
    defining_environment_module: Option<String>,
}

impl Frame {
    fn top(chunk: Chunk, lexical_context: Rc<ModuleLexicalContext>) -> Self {
        let scopes = vec![BTreeMap::new()];
        Self {
            chunk: Rc::new(chunk),
            ip: 0,
            slots: Vec::new(),
            scope_base: scopes.len(),
            has_declaration_overlay: false,
            scopes,
            // The builtin type-binding map is the top frame's protected base.
            type_scope_base: 1,
            alias_scope_base: 1,
            stack_base: 0,
            is_function: false,
            function_module: None,
            lexical_context,
            wrap: FrameWrap::None,
            defining_environment_module: None,
        }
    }

    fn function(
        chunk: Rc<Chunk>,
        slots: Vec<Value>,
        scopes: Vec<BTreeMap<String, Value>>,
        stack_base: usize,
        context: FunctionFrameContext,
        wrap: FrameWrap,
    ) -> Self {
        Self {
            chunk,
            ip: 0,
            slots,
            scope_base: scopes.len(),
            has_declaration_overlay: false,
            scopes,
            type_scope_base: context.scope_bases.types,
            alias_scope_base: context.scope_bases.aliases,
            stack_base,
            is_function: true,
            function_module: context.function_module,
            lexical_context: context.lexical_context,
            wrap,
            defining_environment_module: None,
        }
    }

    fn caller_type_scope_depth(&self) -> usize {
        if self.is_function {
            self.type_scope_base.saturating_sub(1)
        } else {
            self.type_scope_base
        }
    }
}

/// Post-processing applied to a frame's return value / error at
/// the moment it resumes the caller. Most frames don't need
/// any — only callable-dispatch helpers (`try_call`, Result's
/// `map` / `map_err`) set this so the engine can wrap the
/// closure's bare return value without the user having to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameWrap {
    /// Plain return — push the value as-is.
    None,
    /// `try_call(f)` landing pad: wrap a clean return in
    /// `Result::Ok(v)`; a non-fatal error unwinds to this frame
    /// and pushes `Result::Err(RuntimeError { … })` instead.
    /// Fatal errors still bypass the trap — see
    /// `BopError::is_fatal`.
    TryCall { line: u32 },
    /// `r.map(f)` landing pad: wrap the clean return in
    /// `Result::Ok(v)`. Errors in `f` propagate unchanged.
    ResultOk { line: u32 },
    /// `r.map_err(f)` landing pad: wrap the clean return in
    /// `Result::Err(v)`. Errors in `f` propagate unchanged.
    ResultErr { line: u32 },
    /// `MakeIter` landing pad for user-typed iterables: the
    /// frame ran the user's `.iter()` method; its return value
    /// should become a `Slot::IterObject` on the caller's
    /// stack instead of the usual `Slot::Value`.
    IterStart,
    /// `IterNext` landing pad for user-typed iterators: the
    /// frame ran the user's `.next()` method. Inspect the
    /// return (expected `Iter::Next(x)` / `Iter::Done`) and
    /// either push `x` (so the subsequent slot store picks it
    /// up) or pop the iterator and jump to the exit target.
    IterAdvance(crate::chunk::CodeOffset),
}

/// Destructured view of a value returned from an iterator's
/// `.next()` method — either `Iter::Next(v)`, `Iter::Done`, or
/// anything else (user bug, surfaced as a runtime error).
enum IterStep {
    Next(Value),
    Done,
    Malformed,
}

fn unwrap_iter_step(v: &Value) -> IterStep {
    let e = match v {
        Value::EnumVariant(e) => e,
        _ => return IterStep::Malformed,
    };
    if e.type_name() != "Iter" {
        return IterStep::Malformed;
    }
    match (e.variant(), e.payload()) {
        ("Next", bop::value::EnumPayload::Tuple(items)) if items.len() == 1 => {
            IterStep::Next(items[0].clone())
        }
        ("Done", bop::value::EnumPayload::Unit) => IterStep::Done,
        _ => IterStep::Malformed,
    }
}

struct FnEntry {
    params: Vec<String>,
    chunk: Rc<Chunk>,
    module_path: String,
}

/// Structural equality check for two enum-variant lists.
/// Used when the same module is resolved via two paths so a
/// re-import is idempotent rather than an error.
///
/// Matches the walker's `variants_equivalent` rule: variant
/// order + names must match, tuple variants compare by arity
/// only (payload names are positional stubs with no runtime
/// meaning), struct variants still require matching field
/// names.
fn shapes_match(
    a: &[(String, EnumVariantShape)],
    b: &[(String, EnumVariantShape)],
) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|((an, av), (bn, bv))| {
        if an != bn {
            return false;
        }
        match (av, bv) {
            (EnumVariantShape::Unit, EnumVariantShape::Unit) => true,
            (EnumVariantShape::Tuple(fa), EnumVariantShape::Tuple(fb)) => fa.len() == fb.len(),
            (EnumVariantShape::Struct(fa), EnumVariantShape::Struct(fb)) => fa == fb,
            _ => false,
        }
    })
}

type TypeKey = (String, String);
type RuntimeEnumVariants = Vec<(String, EnumVariantShape)>;
type StructTypeTable = BTreeMap<TypeKey, Vec<String>>;
type EnumTypeTable = BTreeMap<TypeKey, RuntimeEnumVariants>;
type BuiltinTypeTables = (StructTypeTable, EnumTypeTable, BTreeMap<String, String>);
type ModuleStructDef = (TypeKey, Vec<String>);
type ModuleEnumDef = (TypeKey, RuntimeEnumVariants);
type ModuleMethodDef = (TypeKey, String, Rc<FnEntry>);

/// Seed a VM's type tables with the engine-wide builtin shapes
/// (`Result`, `RuntimeError`). Same source of truth as the walker
/// — both engines read `bop::builtins::builtin_*` so they can
/// never drift out of sync.
///
/// Returns:
/// - `struct_defs` keyed by `(module_path, type_name)`, with the
///   builtin entry under `<builtin>`;
/// - `enum_defs` keyed the same way;
/// - an outer-scope type_bindings map pointing each builtin's
///   bare name at `<builtin>` so user code resolves `Result::Ok`
///   without an explicit `use`.
fn seed_builtin_types() -> BuiltinTypeTables {
    use bop::parser::VariantKind;
    let builtin_mp = bop::value::BUILTIN_MODULE_PATH.to_string();
    let mut struct_defs: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut enum_defs: BTreeMap<(String, String), Vec<(String, EnumVariantShape)>> = BTreeMap::new();
    let mut bindings: BTreeMap<String, String> = BTreeMap::new();
    struct_defs.insert(
        (builtin_mp.clone(), String::from("RuntimeError")),
        bop::builtins::builtin_runtime_error_fields(),
    );
    bindings.insert(String::from("RuntimeError"), builtin_mp.clone());
    let variants: Vec<(String, EnumVariantShape)> = bop::builtins::builtin_result_variants()
        .into_iter()
        .map(|v| {
            let shape = match v.kind {
                VariantKind::Unit => EnumVariantShape::Unit,
                VariantKind::Tuple(fs) => EnumVariantShape::Tuple(fs),
                VariantKind::Struct(fs) => EnumVariantShape::Struct(fs),
            };
            (v.name, shape)
        })
        .collect();
    enum_defs.insert((builtin_mp.clone(), String::from("Result")), variants);
    bindings.insert(String::from("Result"), builtin_mp.clone());
    let iter_variants: Vec<(String, EnumVariantShape)> = bop::builtins::builtin_iter_variants()
        .into_iter()
        .map(|v| {
            let shape = match v.kind {
                VariantKind::Unit => EnumVariantShape::Unit,
                VariantKind::Tuple(fs) => EnumVariantShape::Tuple(fs),
                VariantKind::Struct(fs) => EnumVariantShape::Struct(fs),
            };
            (v.name, shape)
        })
        .collect();
    enum_defs.insert((builtin_mp.clone(), String::from("Iter")), iter_variants);
    bindings.insert(String::from("Iter"), builtin_mp);
    (struct_defs, enum_defs, bindings)
}

/// Cached result of loading a module. `Loading` is the
/// in-progress sentinel for circular-import detection; `Loaded`
/// carries every top-level export — bindings, struct/enum
/// decls, and methods — so the importer can rebuild the
/// module's visible surface without re-evaluating it.
#[allow(clippy::large_enum_variant)]
enum ImportSlot {
    Loading,
    Loaded(ModuleArtifacts),
}

#[derive(Clone)]
struct ModuleArtifacts {
    /// Top-level `let` bindings, reified as `Value`s. Fns live
    /// in `fn_decls` instead so the importer can register them
    /// in `self.functions` for cross-fn call resolution.
    bindings: Vec<(String, Value)>,
    /// Declaring module and binding for every exported value. Facades retain
    /// this provenance so their live execution environment can borrow the
    /// authoritative handle rather than cloning a stale snapshot.
    binding_origins: BTreeMap<String, BindingOrigin>,
    /// Top-level `fn` declarations. The importer copies each
    /// into its own `self.functions` table AND pushes a
    /// `Value::Fn` into the current scope so first-class use
    /// (`let g = some_imported_fn`) still works.
    fn_decls: Vec<(String, Rc<FnEntry>)>,
    /// Struct-type declarations introduced by the module,
    /// keyed by their full identity `(module_path, type_name)`.
    struct_defs: Vec<ModuleStructDef>,
    /// Enum-type declarations introduced by the module, same
    /// keying as `struct_defs`.
    enum_defs: Vec<ModuleEnumDef>,
    /// `((module_path, type_name), method_name, FnEntry)` for
    /// every method the module declared on its own types.
    methods: Vec<ModuleMethodDef>,
    /// Public type names exposed by this module and their declaration origins.
    type_exports: BTreeMap<String, String>,
    /// Exported values that remain module namespaces after value-level winner
    /// resolution. Values retain their original path and selected surface.
    module_exports: BTreeMap<String, Rc<bop::value::BopModule>>,
    /// Private module-owned context restored for calls declared in this
    /// module. This is intentionally separate from the exported surface.
    lexical_context: Rc<ModuleLexicalContext>,
}

#[derive(Clone, Default)]
struct ModuleLexicalContext {
    type_bindings: BTreeMap<String, String>,
    module_aliases: BTreeMap<String, Rc<bop::value::BopModule>>,
    imported_functions: BTreeMap<String, Rc<FnEntry>>,
}

/// Import cache shared across nested VMs so recursive imports
/// resolve exactly once per top-level run.
type ImportCache = Rc<RefCell<BTreeMap<String, ImportSlot>>>;
type LiveValueEnvironments = Rc<RefCell<BTreeMap<String, BTreeMap<String, Value>>>>;
type BindingOrigin = (String, String);

fn snapshot_module_bindings(
    top_scope: &BTreeMap<String, Value>,
    binding_origins: &BTreeMap<String, BindingOrigin>,
    module_path: &str,
    imports: &ImportCache,
    line: u32,
) -> Result<Vec<(String, Value)>, BopError> {
    // Re-exported value bindings can reuse their origin's already-external
    // snapshot. Named functions are intentionally absent from `bindings`
    // (their callable metadata lives in `fn_decls`), so those are snapshot
    // locally with the module's own values instead.
    let forwarded: BTreeMap<String, Value> = {
        let imports = imports.borrow();
        top_scope
            .keys()
            .filter_map(|name| {
                let origin = binding_origins.get(name)?;
                if origin.0 == module_path && origin.1 == *name {
                    return None;
                }
                let ImportSlot::Loaded(origin_module) = imports.get(&origin.0)? else {
                    return None;
                };
                origin_module
                    .bindings
                    .iter()
                    .find(|(origin_name, _)| origin_name == &origin.1)
                    .map(|(_, value)| (name.clone(), value.clone()))
            })
            .collect()
    };
    let local: BTreeMap<String, Value> = top_scope
        .iter()
        .filter(|(name, _)| !forwarded.contains_key(*name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let mut local_snapshots: BTreeMap<String, Value> =
        Value::__compatibility_snapshot_bindings(&local, line)?
            .into_iter()
            .collect();

    top_scope
        .keys()
        .map(|name| {
            let value = forwarded
                .get(name)
                .cloned()
                .or_else(|| local_snapshots.remove(name))
                .ok_or_else(|| error(line, format!("Missing compatibility snapshot for `{name}`")))?;
            Ok((name.clone(), value))
        })
        .collect()
}
type LiveBindingOrigins = Rc<RefCell<BTreeMap<String, BTreeMap<String, BindingOrigin>>>>;

#[derive(Clone)]
struct ModuleRuntime {
    imports: ImportCache,
    environments: LiveValueEnvironments,
    origins: LiveBindingOrigins,
}

impl ModuleRuntime {
    fn empty() -> Self {
        Self {
            imports: Rc::new(RefCell::new(BTreeMap::new())),
            environments: Rc::new(RefCell::new(BTreeMap::new())),
            origins: Rc::new(RefCell::new(BTreeMap::new())),
        }
    }
}

fn take_live_environment(
    environments: &LiveValueEnvironments,
    module_path: &str,
    origins: &BTreeMap<String, BindingOrigin>,
) -> BTreeMap<String, Value> {
    let mut environments = environments.borrow_mut();
    let mut environment = environments.remove(module_path).unwrap_or_default();
    for (binding, (origin_module, origin_binding)) in origins {
        if origin_module == module_path && origin_binding == binding {
            continue;
        }
        if let Some(value) = environments
            .get_mut(origin_module)
            .and_then(|origin| origin.remove(origin_binding))
        {
            environment.insert(binding.clone(), value);
        }
    }
    environment
}

fn put_live_environment(
    environments: &LiveValueEnvironments,
    module_path: &str,
    mut environment: BTreeMap<String, Value>,
    origins: &BTreeMap<String, BindingOrigin>,
) {
    let mut environments = environments.borrow_mut();
    for (binding, (origin_module, origin_binding)) in origins {
        if origin_module == module_path && origin_binding == binding {
            continue;
        }
        if let Some(value) = environment.remove(binding) {
            environments
                .entry(origin_module.clone())
                .or_default()
                .insert(origin_binding.clone(), value);
        }
    }
    environments.insert(module_path.to_string(), environment);
}

// ─── Next action ───────────────────────────────────────────────────

enum Next {
    /// Keep fetching the next instruction.
    Continue,
    /// End the program (top-level `Halt`).
    Halt,
}

// ─── VM ────────────────────────────────────────────────────────────

/// Stack machine that executes a compiled [`Chunk`].
pub struct Vm<'h, H: BopHost + ?Sized> {
    frames: Vec<Frame>,
    stack: Vec<Slot>,
    /// User-declared functions, keyed by name. Wrapped in `Rc`
    /// so the call path can take a cheap handle (one refcount
    /// bump) instead of cloning the params `Vec<String>` and
    /// re-taking the chunk `Rc` on every invocation — a hot
    /// path that ran ~500 000 times in `fib(28)`.
    functions: BTreeMap<String, Rc<FnEntry>>,
    host: &'h mut H,
    steps: u64,
    step_budget: u64,
    rand_state: u64,
    imports: ImportCache,
    imported_here: Vec<BTreeSet<String>>,
    limits: BopLimits,
    /// Module this VM is running — tags newly declared types
    /// with their full identity so two modules declaring the
    /// same name remain distinct. `<root>` at the top level,
    /// the dot-joined module path inside a `use`'d module's
    /// sub-VM.
    current_module: String,
    /// Declared struct types, keyed by full identity
    /// `(module_path, type_name)`. Populated by
    /// `DefineStruct` (which tags with `current_module`) and
    /// merged from imported modules via `exec_use`.
    struct_defs: BTreeMap<(String, String), Vec<String>>,
    /// Declared enum types, same `(module_path, type_name)`
    /// keying as `struct_defs`.
    enum_defs: BTreeMap<(String, String), Vec<(String, EnumVariantShape)>>,
    /// User-defined methods. Outer key is the receiver type's
    /// full identity `(module_path, type_name)`; inner is the
    /// method name. A method declared for `paint.Color` doesn't
    /// fire on `other.Color` — identity is strict.
    user_methods: BTreeMap<(String, String), BTreeMap<String, Rc<FnEntry>>>,
    /// Public type projection for the module currently executing.
    type_exports: BTreeMap<String, String>,
    /// Parallel scope-stack for bare-name type and module-alias
    /// resolution. Pushed / popped in lockstep with frame scopes,
    /// plus a fresh frame at fn-call entry so a fn's own type
    /// decls are isolated from the caller's scope. `<builtin>`
    /// types seed scope 0.
    type_bindings: Vec<BTreeMap<String, String>>,
    /// Module-level aliases. Unlike value scope, these persist
    /// across function boundaries so namespaced references
    /// inside fn bodies (`p.Color::Red` in a pattern) can still
    /// resolve to the aliased module.
    module_aliases: Vec<BTreeMap<String, Rc<bop::value::BopModule>>>,
    /// Non-aliased imported callables, paired with lexical scope frames.
    imported_functions: Vec<BTreeMap<String, Rc<FnEntry>>>,
    /// Current module namespace. Rebuilt only when top-level declarations or
    /// imports change; calls clone this single Rc handle.
    root_lexical_context: Rc<ModuleLexicalContext>,
    /// Freelist of cleared slot vecs from popped frames. Every
    /// fn call needs a fresh `Vec<Value>` sized to `slot_count`
    /// — allocating a new one per call was ~500k small heap
    /// allocations under `fib(28)`. On return we truncate + park;
    /// next call grabs one, resizes in place.
    slots_freelist: Vec<Vec<Value>>,
    root_function_visibility: BTreeMap<u32, Visibility>,
    abi_declarations: Vec<(String, Rc<FnEntry>)>,
    function_origin: BopFnOrigin,
    operation_memory: Option<Rc<bop::memory::MemoryAccount>>,
    live_value_environments: LiveValueEnvironments,
    binding_origins: BTreeMap<String, BindingOrigin>,
    live_binding_origins: LiveBindingOrigins,
    /// Non-fatal runtime diagnostics accumulated across the root VM and any
    /// nested module VMs. Only a public operation boundary writes them.
    runtime_warnings: Vec<BopWarning>,
}

struct VmState {
    frames: Vec<Frame>,
    stack: Vec<Slot>,
    functions: BTreeMap<String, Rc<FnEntry>>,
    rand_state: u64,
    imports: ImportCache,
    imported_here: Vec<BTreeSet<String>>,
    current_module: String,
    struct_defs: BTreeMap<(String, String), Vec<String>>,
    enum_defs: BTreeMap<(String, String), Vec<(String, EnumVariantShape)>>,
    user_methods: BTreeMap<(String, String), BTreeMap<String, Rc<FnEntry>>>,
    type_exports: BTreeMap<String, String>,
    type_bindings: Vec<BTreeMap<String, String>>,
    module_aliases: Vec<BTreeMap<String, Rc<bop::value::BopModule>>>,
    imported_functions: Vec<BTreeMap<String, Rc<FnEntry>>>,
    root_lexical_context: Rc<ModuleLexicalContext>,
    slots_freelist: Vec<Vec<Value>>,
    root_function_visibility: BTreeMap<u32, Visibility>,
    abi_declarations: Vec<(String, Rc<FnEntry>)>,
    function_origin: BopFnOrigin,
    live_value_environments: LiveValueEnvironments,
    binding_origins: BTreeMap<String, BindingOrigin>,
    live_binding_origins: LiveBindingOrigins,
    runtime_warnings: Vec<BopWarning>,
}

/// A loaded bytecode program whose globals and module state survive calls.
pub struct BopInstance {
    state: Option<VmState>,
    entries: Vec<EntryPoint>,
    limits: BopLimits,
    in_operation: Cell<bool>,
    memory: Rc<bop::memory::MemoryAccount>,
}

impl<'h, H: BopHost + ?Sized> Vm<'h, H> {
    /// Construct a VM from trusted bytecode.
    ///
    /// Embedders executing a hand-built or deserialized [`Chunk`] should use
    /// [`execute`], which validates structural bytecode invariants first.
    pub fn new(chunk: Chunk, host: &'h mut H, limits: BopLimits) -> Self {
        let memory = bop::memory::MemoryAccount::__new(limits.max_memory);
        let mut vm = Self::new_internal(
            chunk,
            host,
            limits,
            ModuleRuntime::empty(),
            String::from(bop::value::ROOT_MODULE_PATH),
            BTreeMap::new(),
            BopFnOrigin::__instance("vm"),
        );
        vm.operation_memory = Some(memory);
        vm
    }

    fn new_internal(
        chunk: Chunk,
        host: &'h mut H,
        limits: BopLimits,
        module_runtime: ModuleRuntime,
        current_module: String,
        root_function_visibility: BTreeMap<u32, Visibility>,
        function_origin: BopFnOrigin,
    ) -> Self {
        let step_budget = limits.max_steps.saturating_mul(STEP_SCALE);
        let (struct_defs, enum_defs, builtin_bindings) = seed_builtin_types();
        let root_lexical_context = Rc::new(ModuleLexicalContext {
            type_bindings: builtin_bindings.clone(),
            module_aliases: BTreeMap::new(),
            imported_functions: BTreeMap::new(),
        });
        let top = Frame::top(chunk, Rc::clone(&root_lexical_context));
        Self {
            frames: vec![top],
            stack: Vec::new(),
            functions: BTreeMap::new(),
            host,
            steps: 0,
            step_budget,
            rand_state: 0,
            imports: module_runtime.imports,
            imported_here: vec![BTreeSet::new()],
            limits,
            current_module,
            struct_defs,
            enum_defs,
            user_methods: BTreeMap::new(),
            type_exports: BTreeMap::new(),
            type_bindings: vec![builtin_bindings],
            module_aliases: vec![BTreeMap::new()],
            imported_functions: vec![BTreeMap::new()],
            root_lexical_context,
            slots_freelist: Vec::new(),
            root_function_visibility,
            abi_declarations: Vec::new(),
            function_origin,
            operation_memory: None,
            live_value_environments: module_runtime.environments,
            binding_origins: BTreeMap::new(),
            live_binding_origins: module_runtime.origins,
            runtime_warnings: Vec::new(),
        }
    }

    fn into_state(self) -> VmState {
        VmState {
            frames: self.frames,
            stack: self.stack,
            functions: self.functions,
            rand_state: self.rand_state,
            imports: self.imports,
            imported_here: self.imported_here,
            current_module: self.current_module,
            struct_defs: self.struct_defs,
            enum_defs: self.enum_defs,
            user_methods: self.user_methods,
            type_exports: self.type_exports,
            type_bindings: self.type_bindings,
            module_aliases: self.module_aliases,
            imported_functions: self.imported_functions,
            root_lexical_context: self.root_lexical_context,
            slots_freelist: self.slots_freelist,
            root_function_visibility: self.root_function_visibility,
            abi_declarations: self.abi_declarations,
            function_origin: self.function_origin,
            live_value_environments: self.live_value_environments,
            binding_origins: self.binding_origins,
            live_binding_origins: self.live_binding_origins,
            runtime_warnings: self.runtime_warnings,
        }
    }

    fn from_state(state: VmState, host: &'h mut H, limits: BopLimits) -> Self {
        Self {
            frames: state.frames,
            stack: state.stack,
            functions: state.functions,
            host,
            steps: 0,
            step_budget: limits.max_steps.saturating_mul(STEP_SCALE),
            rand_state: state.rand_state,
            imports: state.imports,
            imported_here: state.imported_here,
            limits,
            current_module: state.current_module,
            struct_defs: state.struct_defs,
            enum_defs: state.enum_defs,
            user_methods: state.user_methods,
            type_exports: state.type_exports,
            type_bindings: state.type_bindings,
            module_aliases: state.module_aliases,
            imported_functions: state.imported_functions,
            root_lexical_context: state.root_lexical_context,
            slots_freelist: state.slots_freelist,
            root_function_visibility: state.root_function_visibility,
            abi_declarations: state.abi_declarations,
            function_origin: state.function_origin,
            operation_memory: None,
            live_value_environments: state.live_value_environments,
            binding_origins: state.binding_origins,
            live_binding_origins: state.live_binding_origins,
            runtime_warnings: state.runtime_warnings,
        }
    }

    fn restore_instance_baseline(&mut self) {
        while self.frames.len() > 1 {
            let mut frame = self.frames.pop().expect("frame present");
            self.store_frame_defining_environment(&mut frame);
            if !frame.slots.is_empty() {
                self.return_slots(frame.slots);
            }
        }
        self.restore_active_defining_environment();
        self.stack.clear();
        if let Some(root) = self.frames.first_mut() {
            root.scopes.truncate(root.scope_base);
        }
        self.type_bindings.truncate(1);
        self.module_aliases.truncate(1);
        self.imported_functions.truncate(1);
        self.imported_here.truncate(1);
    }

    /// Grab a slot vec from the freelist or allocate a fresh one,
    /// then size it to `len` with `Value::None` placeholders.
    /// Callers write their args into the first few entries after
    /// calling this.
    fn take_slots(&mut self, len: usize) -> Vec<Value> {
        let mut v = self.slots_freelist.pop().unwrap_or_default();
        if v.capacity() < len {
            v.reserve(len - v.capacity());
        }
        v.resize(len, Value::None);
        v
    }

    /// Return a slot vec to the freelist after a frame pops. We
    /// clear (not deallocate) so the next call reuses the backing
    /// allocation. Capped so a pathological call graph doesn't
    /// grow unbounded memory.
    fn return_slots(&mut self, mut slots: Vec<Value>) {
        if self.slots_freelist.len() >= SLOTS_FREELIST_CAP {
            return;
        }
        slots.clear();
        self.slots_freelist.push(slots);
    }

    pub fn run(mut self) -> Result<(), BopError> {
        let _memory = match &self.operation_memory {
            Some(account) => bop::memory::ActiveMemoryGuard::__activate(account),
            None => bop::memory::ActiveMemoryGuard::__activate_new_if_none(
                self.limits.max_memory,
            ),
        };
        let result = (|| {
            let mut last_line: u32 = 0;
            while let Some((instr, line)) = self.fetch() {
                last_line = line;
                // Tick errors (resource-limit violations) are always
                // fatal, so `unwind_to_try_call` will short-circuit
                // them. The path still goes through the helper so
                // the two error paths behave identically.
                if let Err(err) = self.tick(line) {
                    self.unwind_to_try_call(err)?;
                    continue;
                }
                match self.dispatch(instr, line) {
                    Ok(Next::Continue) => {}
                    Ok(Next::Halt) => break,
                    Err(err) => {
                        self.unwind_to_try_call(err)?;
                    }
                }
            }
            // Programs that allocate past the memory limit and then
            // finish in fewer ticks than the periodic memory-check
            // cadence (see `TICK_MEMCHECK_MASK`) would otherwise slip
            // through silently. Catch them here — cheap, and a
            // program ending always runs this exactly once.
            if bop::memory::bop_memory_exceeded() {
                return Err(error_fatal_with_hint(
                    last_line,
                    "Memory limit exceeded",
                    "Your code is using too much memory. Check for large strings or arrays growing in loops.",
                ));
            }
            Ok(())
        })();
        self.write_runtime_warnings();
        result
    }

    /// Deliver accumulated warnings once at an externally visible execution
    /// boundary. The Vec exists in no_std builds too; only stderr delivery is
    /// unavailable there.
    fn write_runtime_warnings(&mut self) {
        #[cfg(not(feature = "no_std"))]
        for warning in self.runtime_warnings.drain(..) {
            eprintln!("warning: {}", warning.message);
        }
        #[cfg(feature = "no_std")]
        self.runtime_warnings.clear();
    }

    // ─── Fetch / ip ──────────────────────────────────────────────

    #[inline]
    fn fetch(&mut self) -> Option<(Instr, u32)> {
        let frame = self.frames.last_mut()?;
        if frame.ip >= frame.chunk.code.len() {
            return None;
        }
        let instr = frame.chunk.code[frame.ip];
        let line = frame.chunk.lines[frame.ip];
        frame.ip += 1;
        Some((instr, line))
    }

    fn jump(&mut self, target: CodeOffset) {
        if let Some(frame) = self.frames.last_mut() {
            frame.ip = target.0 as usize;
        }
    }

    // ─── Tick ────────────────────────────────────────────────────

    #[inline]
    fn tick(&mut self, line: u32) -> Result<(), BopError> {
        self.steps += 1;
        if self.steps > self.step_budget {
            // Fatal — `try_call` can't catch this or the
            // step-limit sandbox invariant would break.
            //
            // Which instruction the budget trips on is an accident
            // of step-count phase (and differs from the walker's
            // per-statement accounting), so report the outermost
            // active source loop's header instead — the walker does
            // the same, keeping the two engines' error lines
            // aligned.
            return Err(error_fatal_with_hint(
                self.active_loop_line().unwrap_or(line),
                "Your code took too many steps (possible infinite loop)",
                "Check your loops — make sure they have a condition that eventually stops them.",
            ));
        }
        // Memory-limit check is a thread-local (or global Cell)
        // lookup — cheap in absolute terms, but done every
        // instruction it dominates a tight arithmetic loop.
        // Allocations enter through `Value::Clone` /
        // constructors, so we only need to notice when the
        // counter crosses the limit. Checking every 256 ticks
        // caps detection latency at ~256 instructions worth of
        // allocations — negligible for a hard cap that's already
        // elastic by a few MB.
        if self.steps & TICK_MEMCHECK_MASK == 0
            && bop::memory::bop_memory_exceeded()
        {
            return Err(error_fatal_with_hint(
                line,
                "Memory limit exceeded",
                "Your code is using too much memory. Check for large strings or arrays growing in loops.",
            ));
        }
        let _suspended = bop::memory::ActiveMemoryGuard::__suspend();
        self.host.on_tick()?;
        Ok(())
    }

    /// Header line of the outermost source loop the VM is currently
    /// executing, scanning call frames outermost-first. For the
    /// active frame `ip - 1` is the instruction being ticked; for a
    /// caller frame it is the `Call` that entered the callee, so a
    /// budget trip inside a loop-free callee still resolves to the
    /// calling loop. `None` when no frame is inside a loop (e.g.
    /// branching recursion exhausting the budget). See
    /// [`Chunk::outermost_loop_line`] for why outermost.
    fn active_loop_line(&self) -> Option<u32> {
        self.frames.iter().find_map(|frame| {
            let offset = CodeOffset(frame.ip.saturating_sub(1) as u32);
            frame.chunk.outermost_loop_line(offset)
        })
    }

    // ─── Stack helpers ───────────────────────────────────────────

    #[inline]
    fn push_value(&mut self, v: Value) {
        self.stack.push(Slot::Value(v));
    }

    #[inline]
    fn pop_value(&mut self, line: u32) -> Result<Value, BopError> {
        match self.stack.pop() {
            Some(Slot::Value(v)) => Ok(v),
            Some(_) => Err(error(line, "VM: expected value on stack")),
            None => Err(error(line, "VM: stack underflow")),
        }
    }

    #[inline]
    fn peek_value(&self, line: u32) -> Result<&Value, BopError> {
        match self.stack.last() {
            Some(Slot::Value(v)) => Ok(v),
            Some(_) => Err(error(line, "VM: expected value on stack")),
            None => Err(error(line, "VM: stack underflow")),
        }
    }

    fn pop_n_values(&mut self, n: usize, line: u32) -> Result<Vec<Value>, BopError> {
        if self.stack.len() < n {
            return Err(error(line, "VM: stack underflow"));
        }
        let start = self.stack.len() - n;
        let mut out = Vec::with_capacity(n);
        for slot in self.stack.drain(start..) {
            match slot {
                Slot::Value(v) => out.push(v),
                _ => return Err(error(line, "VM: expected value on stack")),
            }
        }
        Ok(out)
    }

    // ─── Scope ───────────────────────────────────────────────────

    fn current_scopes_mut(&mut self) -> &mut Vec<BTreeMap<String, Value>> {
        &mut self.frames.last_mut().expect("frame present").scopes
    }

    fn current_scopes(&self) -> &Vec<BTreeMap<String, Value>> {
        &self.frames.last().expect("frame present").scopes
    }

    fn push_scope(&mut self) {
        self.current_scopes_mut().push(BTreeMap::new());
        // Type bindings parallel value scopes so inline type
        // decls inside a block vanish on block exit, same rule
        // the walker applies.
        self.type_bindings.push(BTreeMap::new());
        self.module_aliases.push(BTreeMap::new());
        self.imported_functions.push(BTreeMap::new());
        self.imported_here.push(BTreeSet::new());
    }

    fn pop_scope(&mut self) {
        let popped = {
            let frame = self.frames.last_mut().expect("frame present");
            if frame.scopes.len() > frame.scope_base {
                frame.scopes.pop();
                true
            } else {
                false
            }
        };
        if popped {
            let type_scope_base = self
                .frames
                .last()
                .expect("frame present")
                .type_scope_base;
            debug_assert!(
                self.type_bindings.len() > type_scope_base,
                "value and type scope stacks must stay paired"
            );
            if self.type_bindings.len() > type_scope_base {
                self.type_bindings.pop();
                self.module_aliases.pop();
                self.imported_functions.pop();
                self.imported_here.pop();
            }
        }
    }

    /// Push a function-like frame together with its protected type-binding
    /// scope. Keeping this transition in one helper prevents constructors for
    /// named functions, closures, methods, and iterator methods from drifting.
    fn push_function_frame(
        &mut self,
        chunk: Rc<Chunk>,
        slots: Vec<Value>,
        scopes: Vec<BTreeMap<String, Value>>,
        stack_base: usize,
        function_module: Option<String>,
        wrap: FrameWrap,
    ) {
        self.park_active_defining_environment();
        let defining_environment = function_module.as_ref().map(|module| {
            let origins = self.binding_origins_for(module);
            take_live_environment(&self.live_value_environments, module, &origins)
        });
        let mut scopes = scopes;
        if let Some(environment) = defining_environment {
            scopes.insert(0, environment);
            // Runtime declarations/imports belong to the call, never to the
            // defining module environment parked below it.
            scopes.push(BTreeMap::new());
        }
        let lexical_context = function_module
            .as_deref()
            .map(|module| self.module_lexical_context(module))
            .unwrap_or_else(|| Rc::clone(&self.root_lexical_context));
        let type_scope_base = self.type_bindings.len() + 1;
        let alias_scope_base = self.module_aliases.len();
        self.module_aliases.push(BTreeMap::new());
        self.imported_functions.push(BTreeMap::new());
        self.imported_here.push(BTreeSet::new());
        let mut frame = Frame::function(
            chunk,
            slots,
            scopes,
            stack_base,
            FunctionFrameContext {
                scope_bases: FrameScopeBases {
                    types: type_scope_base,
                    aliases: alias_scope_base,
                },
                function_module,
                lexical_context,
            },
            wrap,
        );
        frame.defining_environment_module = frame.function_module.clone();
        self.frames.push(frame);
        self.type_bindings.push(BTreeMap::new());
    }

    fn park_active_defining_environment(&mut self) {
        let active_module = self.frames.last().and_then(|frame| {
            frame
                .defining_environment_module
                .clone()
                .or_else(|| (!frame.is_function).then(|| self.current_module.clone()))
        });
        let Some(module) = active_module else {
            return;
        };
        let environment = self
            .frames
            .last_mut()
            .and_then(|frame| frame.scopes.first_mut())
            .map(core::mem::take)
            .unwrap_or_default();
        let origins = self.binding_origins_for(&module);
        put_live_environment(
            &self.live_value_environments,
            &module,
            environment,
            &origins,
        );
        self.live_binding_origins
            .borrow_mut()
            .insert(module, origins);
    }

    fn store_frame_defining_environment(&mut self, frame: &mut Frame) {
        let Some(module) = frame.defining_environment_module.as_ref() else {
            return;
        };
        let origins = self.binding_origins_for(module);
        if !self.live_value_environments.borrow().contains_key(module) {
            let environment = frame
                .scopes
                .first_mut()
                .map(core::mem::take)
                .unwrap_or_default();
            put_live_environment(
                &self.live_value_environments,
                module,
                environment,
                &origins,
            );
        }
        self.live_binding_origins
            .borrow_mut()
            .insert(module.clone(), origins);
    }

    fn restore_active_defining_environment(&mut self) {
        let active_module = self.frames.last().and_then(|frame| {
            frame
                .defining_environment_module
                .clone()
                .or_else(|| (!frame.is_function).then(|| self.current_module.clone()))
        });
        let Some(module) = active_module else {
            return;
        };
        if !self.live_value_environments.borrow().contains_key(&module) {
            return;
        }
        let origins = self.binding_origins_for(&module);
        let environment =
            take_live_environment(&self.live_value_environments, &module, &origins);
        if let Some(scope) = self
            .frames
            .last_mut()
            .and_then(|frame| frame.scopes.first_mut())
        {
            *scope = environment;
        }
    }

    fn module_alias(&self, name: &str) -> Option<&Rc<bop::value::BopModule>> {
        let floor = self.frames.last().filter(|frame| frame.is_function).map(|frame| frame.alias_scope_base);
        self.module_aliases
            .iter()
            .enumerate()
            .rev()
            .filter(|(index, _)| floor.is_none_or(|floor| *index >= floor))
            .find_map(|(_, frame)| frame.get(name))
            .or_else(|| {
                self.frames
                    .last()
                    .and_then(|frame| frame.lexical_context.module_aliases.get(name))
            })
    }

    fn module_binding(&self, module: &bop::value::BopModule, name: &str) -> Option<Value> {
        if let Some((active_module, environment)) = self.frames.last().and_then(|frame| {
            let active_module = frame
                .defining_environment_module
                .as_deref()
                .or_else(|| (!frame.is_function).then_some(self.current_module.as_str()))?;
            Some((active_module, frame.scopes.first()?))
        }) {
            if module.path == active_module {
                if let Some(value) = environment.get(name) {
                    return Some(value.clone());
                }
            }
            let (origin_module, origin_binding) = module.__binding_origin(name);
            if origin_module == active_module {
                if let Some(value) = environment.get(&origin_binding) {
                    return Some(value.clone());
                }
            }
            let active_origins = self.binding_origins_for(active_module);
            if let Some(active_binding) = active_origins.iter().find_map(
                |(active_binding, active_origin)| {
                    (active_origin.0 == origin_module && active_origin.1 == origin_binding)
                        .then_some(active_binding)
                },
            ) {
                if let Some(value) = environment.get(active_binding) {
                    return Some(value.clone());
                }
            }
        }
        module.__binding(name)
    }

    fn binding_origins_for(&self, module: &str) -> BTreeMap<String, BindingOrigin> {
        if module == self.current_module {
            return self.binding_origins.clone();
        }
        self.live_binding_origins
            .borrow()
            .get(module)
            .cloned()
            .unwrap_or_default()
    }

    fn live_origin_value(&self, origin: &BindingOrigin) -> Option<Value> {
        if let Some((active_module, environment)) = self.frames.last().and_then(|frame| {
            let active_module = frame
                .defining_environment_module
                .as_deref()
                .or_else(|| (!frame.is_function).then_some(self.current_module.as_str()))?;
            Some((active_module, frame.scopes.first()?))
        }) {
            if active_module == origin.0 {
                if let Some(value) = environment.get(&origin.1) {
                    return Some(value.clone());
                }
            }
            let active_origins = self.binding_origins_for(active_module);
            if let Some(binding) = active_origins.iter().find_map(|(binding, candidate)| {
                (candidate == origin).then_some(binding)
            }) {
                if let Some(value) = environment.get(binding) {
                    return Some(value.clone());
                }
            }
        }
        self.live_value_environments
            .borrow()
            .get(&origin.0)
            .and_then(|environment| environment.get(&origin.1))
            .cloned()
    }

    fn imported_function(&self, name: &str) -> Option<&Rc<FnEntry>> {
        let floor = self.frames.last().filter(|frame| frame.is_function).map(|frame| frame.alias_scope_base);
        self.imported_functions
            .iter()
            .enumerate()
            .rev()
            .filter(|(index, _)| floor.is_none_or(|floor| *index >= floor))
            .find_map(|(_, frame)| frame.get(name))
            .or_else(|| {
                self.frames
                    .last()
                    .and_then(|frame| frame.lexical_context.imported_functions.get(name))
            })
    }

    fn is_module_top_scope(&self) -> bool {
        self.frames.last().is_some_and(|frame| !frame.is_function && frame.scopes.len() == frame.scope_base)
    }

    fn define_local(&mut self, name: String, value: Value) {
        if self.current_scopes().is_empty() {
            self.current_scopes_mut().push(BTreeMap::new());
        }
        if let Some(scope) = self.current_scopes_mut().last_mut() {
            scope.insert(name, value);
        }
    }

    fn sync_top_level_module_alias(
        &mut self,
        name: String,
        module: Option<Rc<bop::value::BopModule>>,
    ) {
        if !self.is_module_top_scope() {
            return;
        }
        let aliases = self.module_aliases.last_mut().expect("module alias scope");
        if let Some(module) = module {
            aliases.insert(name, module);
        } else {
            aliases.remove(&name);
        }
        self.refresh_root_lexical_context();
    }

    /// `lookup_var`, but pull the name from the current chunk by
    /// index. Avoids an `Rc::clone` + separate borrow at the call
    /// site — the dispatch loop's hottest read path.
    fn lookup_var_by_idx(&self, idx: NameIdx) -> Option<&Value> {
        let frame = self.frames.last()?;
        let name = frame.chunk.name(idx);
        for scope in frame.scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v);
            }
        }
        if frame.is_function
            && frame.function_module.as_deref() == Some(bop::value::ROOT_MODULE_PATH)
        {
            return self
                .frames
                .first()
                .and_then(|root| root.scopes.iter().rev().find_map(|scope| scope.get(name)));
        }
        None
    }

    fn lookup_var_mut_by_idx(&mut self, idx: NameIdx) -> Option<&mut Value> {
        let (name, uses_root) = {
            let frame = self.frames.last()?;
            (
                frame.chunk.name(idx).to_string(),
                frame.is_function
                    && frame.function_module.as_deref()
                        == Some(bop::value::ROOT_MODULE_PATH),
            )
        };
        let current_index = self.frames.len().checked_sub(1)?;
        if current_index == 0 {
            return self.frames[0]
                .scopes
                .iter_mut()
                .rev()
                .find_map(|scope| scope.get_mut(&name));
        }
        let (earlier, current) = self.frames.split_at_mut(current_index);
        for scope in current[0].scopes.iter_mut().rev() {
            if let Some(value) = scope.get_mut(&name) {
                return Some(value);
            }
        }
        if uses_root {
            return earlier.first_mut().and_then(|root| {
                root.scopes
                    .iter_mut()
                    .rev()
                    .find_map(|scope| scope.get_mut(&name))
            });
        }
        None
    }

    fn set_existing(&mut self, name: &str, value: Value) -> bool {
        for scope in self.current_scopes_mut().iter_mut().rev() {
            // Writing through `get_mut` keeps the existing key
            // allocation in place — we'd otherwise pay a fresh
            // `String` alloc per loop iteration for every
            // `StoreVar` in a tight loop (e.g. `i = i + 1`).
            if let Some(slot) = scope.get_mut(name) {
                *slot = value;
                return true;
            }
        }
        if self.frames.last().is_some_and(|frame| {
            frame.is_function
                && frame.function_module.as_deref() == Some(bop::value::ROOT_MODULE_PATH)
        }) {
            if let Some(slot) = self
                .frames
                .first_mut()
                .and_then(|root| root.scopes.iter_mut().rev().find_map(|scope| scope.get_mut(name)))
            {
                *slot = value;
                return true;
            }
        }
        false
    }

    /// `set_existing`, but pull the name from the current chunk
    /// by index. Splits the frame's `&mut` into a `&Rc<Chunk>`
    /// (for the name slice) and `&mut scopes` (for the walk)
    /// using field-level borrow splitting — no `Rc::clone`.
    fn set_existing_by_idx(&mut self, idx: NameIdx, value: Value) -> bool {
        let current_index = self.frames.len() - 1;
        let name = self.frames[current_index].chunk.name(idx).to_string();
        for scope in self.frames[current_index].scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(&name) {
                *slot = value;
                return true;
            }
        }
        let uses_root = self.frames[current_index].is_function
            && self.frames[current_index].function_module.as_deref()
                == Some(bop::value::ROOT_MODULE_PATH);
        if self.frames[current_index].is_function
            && self.frames[current_index]
                .lexical_context
                .module_aliases
                .contains_key(&name)
        {
            let frame = &mut self.frames[current_index];
            let overlay_scope = if frame.has_declaration_overlay {
                frame.scope_base - 1
            } else {
                let scope = frame.scope_base;
                frame.scopes.insert(scope, BTreeMap::new());
                frame.scope_base += 1;
                frame.has_declaration_overlay = true;
                scope
            };
            frame
                .scopes
                .get_mut(overlay_scope)
                .expect("alias overlay scope")
                .insert(name, value);
            return true;
        }
        if uses_root {
            if let Some(slot) = self.frames[0]
                .scopes
                .iter_mut()
                .rev()
                .find_map(|scope| scope.get_mut(&name))
            {
                *slot = value;
                return true;
            }
        }
        false
    }

    // ─── "Did you mean?" candidate collectors ─────────────────
    //
    // Same pattern as the walker: gather every name the user
    // could plausibly have meant so `bop::suggest::did_you_mean`
    // picks the closest one. VM-specific quirk: the scope stack
    // is per-frame, so we only scan the current frame's scopes
    // rather than the whole interpreter's.

    /// Names reachable when an identifier is used as a value —
    /// locals in the enclosing scopes plus user fn declarations.
    fn value_candidates_hint(&self, target: &str) -> Option<String> {
        let mut candidates: Vec<String> = Vec::new();
        for scope in self.current_scopes() {
            for k in scope.keys() {
                candidates.push(k.clone());
            }
        }
        for name in self.functions.keys() {
            candidates.push(name.clone());
        }
        bop::suggest::did_you_mean(target, candidates)
    }

    /// Names reachable in a call position: user fns plus core
    /// builtins. Host builtins stay with the host's own
    /// `function_hint()` (the call site falls back to that path
    /// when no user-level suggestion fits).
    fn callable_candidates_hint(&self, target: &str) -> Option<String> {
        let mut candidates: Vec<String> =
            self.functions.keys().cloned().collect();
        for b in bop::suggest::CORE_CALLABLE_BUILTINS {
            candidates.push((*b).to_string());
        }
        bop::suggest::did_you_mean(target, candidates)
    }

    fn active_module_path(&self) -> &str {
        self.frames
            .last()
            .and_then(|frame| frame.function_module.as_deref())
            .unwrap_or(&self.current_module)
    }

    fn module_lexical_context(&self, module_path: &str) -> Rc<ModuleLexicalContext> {
        if module_path == self.current_module {
            return Rc::clone(&self.root_lexical_context);
        }
        let cache = self.imports.borrow();
        match cache.get(module_path) {
            Some(ImportSlot::Loaded(artifacts)) => Rc::clone(&artifacts.lexical_context),
            _ => Rc::new(ModuleLexicalContext::default()),
        }
    }

    fn refresh_root_lexical_context(&mut self) {
        self.root_lexical_context = Rc::new(ModuleLexicalContext {
            type_bindings: self.type_bindings.first().cloned().unwrap_or_default(),
            module_aliases: self.module_aliases.first().cloned().unwrap_or_default(),
            imported_functions: self.imported_functions.first().cloned().unwrap_or_default(),
        });
        if let Some(frame) = self.frames.first_mut() {
            frame.lexical_context = Rc::clone(&self.root_lexical_context);
        }
    }

    /// Resolve module-owned sibling functions from cached module artifacts
    /// before falling back to caller-visible functions. This preserves module
    /// function environments without making alias imports publish bare names.
    #[inline]
    fn lookup_function_entry(&self, name: &str) -> Option<Rc<FnEntry>> {
        let frame = self.frames.last()?;
        let defining_module = frame.function_module.as_deref();
        if let Some(module_path) = defining_module {
            if module_path != bop::value::ROOT_MODULE_PATH {
                let cache = self.imports.borrow();
                if let Some(ImportSlot::Loaded(artifacts)) = cache.get(module_path) {
                    if let Some((_, entry)) =
                        artifacts.fn_decls.iter().find(|(candidate, _)| candidate == name)
                    {
                        return Some(entry.clone());
                    }
                }
            }
        }

        // Alias-free named calls dominate real programs. Avoid probing every
        // empty runtime import scope and then the empty defining-module map
        // before reaching `self.functions`. The moment any active import map
        // contains a callable, keep the full lookup below so a block/function
        // import can still shadow a same-named declared function.
        let import_floor = if frame.is_function {
            frame.alias_scope_base
        } else {
            0
        };
        let runtime_imports_empty = self.imported_functions[import_floor..]
            .iter()
            .all(BTreeMap::is_empty);
        if runtime_imports_empty && frame.lexical_context.imported_functions.is_empty() {
            if let Some(module_path) = defining_module {
                return self
                    .functions
                    .get(name)
                    .filter(|entry| entry.module_path == module_path)
                    .cloned();
            }
            return self.functions.get(name).cloned();
        }

        if let Some(entry) = self.imported_function(name) {
            return Some(entry.clone());
        }
        if let Some(module_path) = defining_module {
            if let Some(entry) = self
                .functions
                .get(name)
                .filter(|entry| entry.module_path == module_path)
            {
                return Some(entry.clone());
            }
        }
        if defining_module.is_none_or(|path| path == self.current_module) {
            self.functions.get(name).cloned()
        } else {
            None
        }
    }

    // ─── Dispatch ────────────────────────────────────────────────

    #[inline]
    fn dispatch(&mut self, instr: Instr, line: u32) -> Result<Next, BopError> {
        match instr {
            // ─── Literals ─────────────────────────────────────────
            Instr::LoadConst(idx) => {
                let value = match self.current_chunk().constant(idx) {
                    Constant::Int(n) => Value::Int(*n),
                    Constant::Number(n) => Value::Number(*n),
                    Constant::Str(s) => Value::new_str(s.clone()),
                };
                self.push_value(value);
            }
            Instr::LoadNone => self.push_value(Value::None),
            Instr::LoadTrue => self.push_value(Value::Bool(true)),
            Instr::LoadFalse => self.push_value(Value::Bool(false)),

            // ─── Variables ────────────────────────────────────────
            Instr::LoadVar(n) => {
                // Fast path: look the name up by index so both
                // the chunk borrow and the scope walk happen in
                // one helper — no `Rc::clone` and no intermediate
                // owned `String`. A tight loop with 3 `LoadVar`s
                // per iteration previously paid one `Rc::clone`
                // per access here.
                if let Some(v) = self.lookup_var_by_idx(n).cloned() {
                    self.push_value(v);
                } else {
                    // Slow path: fall back to the named-fn
                    // registry so `fn fib(...) {...}; let g = fib`
                    // yields a real `Value::Fn`. Only then do we
                    // need the name as a `&str` / `String`.
                    let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
                    let name = chunk.name(n);
                    if let Some(module) = self.module_alias(name).cloned() {
                        self.push_value(Value::Module(module));
                        return Ok(Next::Continue);
                    }
                    let fn_entry = self.lookup_function_entry(name);
                    if let Some(entry) = fn_entry {
                        let params = entry.params.clone();
                        let chunk_rc = entry.chunk.clone();
                        let body: Rc<dyn core::any::Any + 'static> = chunk_rc;
                        let v = BopFn::try_new_compiled_in_module_with_origin(
                            params,
                            Vec::new(),
                            body,
                            Some(name.to_string()),
                            Some(entry.module_path.clone()),
                            self.function_origin.clone(),
                            0,
                            line,
                        ).map(Value::Fn)?;
                        self.push_value(v);
                    } else {
                        // "did you mean?" first, else the generic
                        // "use `let`" nudge. Mirrors the walker's
                        // behaviour for consistency across engines.
                        let hint = self
                            .value_candidates_hint(name)
                            .unwrap_or_else(|| {
                                "Did you forget to create it with `let`?".to_string()
                            });
                        return Err(error_with_hint(
                            line,
                            bop::error_messages::variable_not_found(name),
                            hint,
                        ));
                    }
                }
            }
            Instr::DefineLocal(n) => {
                // `define_local` takes an owned `String` because
                // it inserts into the scope map — can't skip the
                // allocation here.
                let name = self.current_chunk().name(n).to_string();
                let v = self.pop_value(line)?;
                let module = match &v {
                    Value::Module(module) => Some(Rc::clone(module)),
                    _ => None,
                };
                if self.is_module_top_scope() {
                    if let Some(origin) = self.binding_origins.get(&name).cloned() {
                        if origin.0 != self.current_module || origin.1 != name {
                            if let Some(previous) = self
                                .current_scopes_mut()
                                .last_mut()
                                .and_then(|scope| scope.remove(&name))
                            {
                                self.live_value_environments
                                    .borrow_mut()
                                    .entry(origin.0)
                                    .or_default()
                                    .insert(origin.1, previous);
                            }
                        }
                    }
                }
                self.define_local(name.clone(), v);
                if self.is_module_top_scope() {
                    self.binding_origins.insert(
                        name.clone(),
                        (self.current_module.clone(), name.clone()),
                    );
                }
                self.sync_top_level_module_alias(name, module);
            }
            Instr::StoreVar(n) => {
                // Fast path: look the target up by index so we
                // neither `Rc::clone` nor allocate the name.
                let v = self.pop_value(line)?;
                let module = match &v {
                    Value::Module(module) => Some(Rc::clone(module)),
                    _ => None,
                };
                let top_level_name = self
                    .is_module_top_scope()
                    .then(|| self.current_chunk().name(n).to_string());
                let runtime_alias_without_value_binding = {
                    let chunk = Rc::clone(
                        &self.frames.last().expect("frame present").chunk,
                    );
                    let name = chunk.name(n);
                    self.lookup_var_by_idx(n).is_none() && self.module_alias(name).is_some()
                };
                if runtime_alias_without_value_binding {
                    let name = self.current_chunk().name(n).to_string();
                    self.define_local(name, v);
                    return Ok(Next::Continue);
                }
                if !self.set_existing_by_idx(n, v) {
                    // Cold path: synthesise the error with the
                    // name — allocation is fine here.
                    let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
                    let name = chunk.name(n);
                    return Err(error_with_hint(
                        line,
                        format!("Variable `{}` doesn't exist yet", name),
                        format!("Use `let` to create a new variable: let {} = ...", name),
                    ));
                }
                if let Some(name) = top_level_name {
                    self.sync_top_level_module_alias(name, module);
                }
            }
            Instr::CompoundAssign { target, op } => {
                let rhs = self.pop_value(line)?;
                let runtime_alias_without_value_binding = match target {
                    crate::chunk::AssignBack::Name(name_idx) => {
                        let chunk = Rc::clone(
                            &self.frames.last().expect("frame present").chunk,
                        );
                        self.lookup_var_by_idx(name_idx).is_none()
                            && self.module_alias(chunk.name(name_idx)).is_some()
                    }
                    crate::chunk::AssignBack::Slot(_) => false,
                };
                let current = match target {
                    crate::chunk::AssignBack::Slot(slot) => self
                        .frames
                        .last()
                        .and_then(|frame| frame.slots.get(slot.0 as usize))
                        .cloned()
                        .ok_or_else(|| error(line, "VM: local slot out of range"))?,
                    crate::chunk::AssignBack::Name(name_idx) => {
                        if let Some(value) = self.lookup_var_by_idx(name_idx).cloned() {
                            value
                        } else {
                            let chunk = Rc::clone(
                                &self.frames.last().expect("frame present").chunk,
                            );
                            let name = chunk.name(name_idx);
                            if let Some(module) = self.module_alias(name).cloned() {
                                Value::Module(module)
                            } else {
                                return Err(error(
                                    line,
                                    bop::error_messages::variable_not_found(name),
                                ));
                            }
                        }
                    }
                };
                let value = apply_in_place_assign(op, rhs, line, || Ok(current))?;
                match target {
                    crate::chunk::AssignBack::Slot(slot) => {
                        let target = self
                            .frames
                            .last_mut()
                            .expect("frame present")
                            .slots
                            .get_mut(slot.0 as usize)
                            .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                        *target = value;
                    }
                    crate::chunk::AssignBack::Name(name) => {
                        if runtime_alias_without_value_binding {
                            let name = self.current_chunk().name(name).to_string();
                            self.define_local(name, value);
                            return Ok(Next::Continue);
                        }
                        if !self.set_existing_by_idx(name, value) {
                            let chunk = Rc::clone(
                                &self.frames.last().expect("frame present").chunk,
                            );
                            return Err(error(
                                line,
                                bop::error_messages::variable_not_found(chunk.name(name)),
                            ));
                        }
                    }
                }
            }

            // ─── Slot-based locals (fast path) ───────────────────
            Instr::LoadLocal(slot) => {
                // Direct vec index — no hashing, no string compare.
                // Slots are pre-sized at call time from the
                // enclosing `FnDef::slot_count`, so an out-of-range
                // read can only happen on miscompiled bytecode.
                let frame = self.frames.last().expect("frame present");
                let v = frame
                    .slots
                    .get(slot.0 as usize)
                    .cloned()
                    .ok_or_else(|| {
                        error(line, "VM: local slot out of range")
                    })?;
                self.push_value(v);
            }
            Instr::StoreLocal(slot) => {
                let v = self.pop_value(line)?;
                let frame = self.frames.last_mut().expect("frame present");
                let i = slot.0 as usize;
                if i < frame.slots.len() {
                    frame.slots[i] = v;
                } else {
                    return Err(error(line, "VM: local slot out of range"));
                }
            }

            // ─── Superinstructions ───────────────────────────────
            Instr::AddLocals(a, b) => {
                // Typed fast path: Int + Int → Int, no Value
                // variant match and no stack push/pop for the
                // operands. `fib`'s recursive body and every
                // `total = total + i` loop go through here.
                let frame = self.frames.last().expect("frame present");
                let av = frame
                    .slots
                    .get(a.0 as usize)
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                let bv = frame
                    .slots
                    .get(b.0 as usize)
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                let result = match (av, bv) {
                    (Value::Int(x), Value::Int(y)) => x
                        .checked_add(*y)
                        .map(Value::Int)
                        .map_or_else(|| ops::add(av, bv, line), Ok)?,
                    // Cold path: delegate to the generic Value
                    // adder. Covers Number, String concat, array
                    // concat, etc.
                    _ => ops::add(av, bv, line)?,
                };
                self.push_value(result);
            }
            Instr::LtLocals(a, b) => {
                let frame = self.frames.last().expect("frame present");
                let av = frame
                    .slots
                    .get(a.0 as usize)
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                let bv = frame
                    .slots
                    .get(b.0 as usize)
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                let result = match (av, bv) {
                    (Value::Int(x), Value::Int(y)) => Value::Bool(x < y),
                    _ => ops::lt(av, bv, line)?,
                };
                self.push_value(result);
            }
            Instr::IncLocalInt(slot, k) => {
                // `slot += k` for small-int `k`, the `i = i + 1`
                // idiom. Fast path: Int → Int with overflow
                // check. On non-Int, build an Int value and
                // dispatch through generic add so `x = x + 1`
                // still works when `x` is a Number.
                let frame = self.frames.last_mut().expect("frame present");
                let i = slot.0 as usize;
                let current = frame
                    .slots
                    .get(i)
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                let new = match current {
                    Value::Int(x) => x
                        .checked_add(k as i64)
                        .map(Value::Int)
                        .map_or_else(
                            || ops::add(current, &Value::Int(k as i64), line),
                            Ok,
                        )?,
                    _ => ops::add(current, &Value::Int(k as i64), line)?,
                };
                frame.slots[i] = new;
            }
            Instr::LoadLocalAddInt(slot, k) => {
                // Push `slots[slot] + k`. Covers `array[i + 1]` after the
                // compiler folds a small-int literal into the op.
                let frame = self.frames.last().expect("frame present");
                let v = frame
                    .slots
                    .get(slot.0 as usize)
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                let result = match v {
                    Value::Int(x) => x
                        .checked_add(k as i64)
                        .map(Value::Int)
                        .map_or_else(|| ops::add(v, &Value::Int(k as i64), line), Ok)?,
                    _ => ops::add(v, &Value::Int(k as i64), line)?,
                };
                self.push_value(result);
            }
            Instr::LtLocalInt(slot, k) => {
                // Push `slots[slot] < k` — the `n < 2` base-case
                // test in `fib` and every bounded `while i < K`
                // loop.
                let frame = self.frames.last().expect("frame present");
                let v = frame
                    .slots
                    .get(slot.0 as usize)
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                let result = match v {
                    Value::Int(x) => Value::Bool(*x < k as i64),
                    _ => ops::lt(v, &Value::Int(k as i64), line)?,
                };
                self.push_value(result);
            }

            // ─── Scope ────────────────────────────────────────────
            Instr::PushScope => self.push_scope(),
            Instr::PopScope => self.pop_scope(),

            // ─── Stack ────────────────────────────────────────────
            Instr::Pop => {
                if self.stack.pop().is_none() {
                    return Err(error(line, "VM: stack underflow"));
                }
            }
            Instr::Dup => {
                let v = self.peek_value(line)?.clone();
                self.push_value(v);
            }
            Instr::Dup2 => {
                let len = self.stack.len();
                if len < 2 {
                    return Err(error(line, "VM: stack underflow"));
                }
                let b = match &self.stack[len - 1] {
                    Slot::Value(v) => v.clone(),
                    _ => return Err(error(line, "VM: expected value on stack")),
                };
                let a = match &self.stack[len - 2] {
                    Slot::Value(v) => v.clone(),
                    _ => return Err(error(line, "VM: expected value on stack")),
                };
                self.push_value(a);
                self.push_value(b);
            }

            // ─── Binary ops ───────────────────────────────────────
            Instr::Add => self.binary(line, ops::add)?,
            Instr::Sub => self.binary(line, ops::sub)?,
            Instr::Mul => self.binary(line, ops::mul)?,
            Instr::Div => self.binary(line, ops::div)?,
            Instr::Rem => self.binary(line, ops::rem)?,
            Instr::Eq => self.binary_infallible(line, |a, b, _| Ok(ops::eq(a, b)))?,
            Instr::NotEq => self.binary_infallible(line, |a, b, _| Ok(ops::not_eq(a, b)))?,
            Instr::Lt => self.binary(line, ops::lt)?,
            Instr::Gt => self.binary(line, ops::gt)?,
            Instr::LtEq => self.binary(line, ops::lt_eq)?,
            Instr::GtEq => self.binary(line, ops::gt_eq)?,

            // ─── Unary ops ────────────────────────────────────────
            Instr::Neg => {
                let v = self.pop_value(line)?;
                self.push_value(ops::neg(&v, line)?);
            }
            Instr::Not => {
                let v = self.pop_value(line)?;
                self.push_value(ops::not(&v));
            }

            Instr::TruthyToBool => {
                let v = self.pop_value(line)?;
                self.push_value(Value::Bool(v.is_truthy()));
            }

            // ─── Indexing ─────────────────────────────────────────
            Instr::GetIndex => {
                let idx = self.pop_value(line)?;
                let obj = self.pop_value(line)?;
                self.push_value(ops::index_get(&obj, &idx, line)?);
            }
            Instr::SetIndex => {
                let val = self.pop_value(line)?;
                let idx = self.pop_value(line)?;
                let mut obj = self.pop_value(line)?;
                ops::index_set(&mut obj, &idx, val, line)?;
                self.push_value(obj);
            }
            Instr::SetIndexInPlace { target, op } => {
                let idx = self.pop_value(line)?;
                let rhs = self.pop_value(line)?;
                match target {
                    crate::chunk::AssignBack::Slot(slot) => {
                        let value = self
                            .frames
                            .last_mut()
                            .expect("frame present")
                            .slots
                            .get_mut(slot.0 as usize)
                            .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                        let val = apply_in_place_assign(
                            op,
                            rhs,
                            line,
                            || ops::index_get(value, &idx, line),
                        )?;
                        ops::index_set(value, &idx, val, line)?;
                    }
                    crate::chunk::AssignBack::Name(name_idx) => {
                        let name = self.current_chunk().name(name_idx).to_string();
                        if self.lookup_var_mut_by_idx(name_idx).is_none() {
                            if let Some(module) = self.module_alias(&name).cloned() {
                                let mut value = Value::Module(module);
                                let val = apply_in_place_assign(
                                    op,
                                    rhs,
                                    line,
                                    || ops::index_get(&value, &idx, line),
                                )?;
                                ops::index_set(&mut value, &idx, val, line)?;
                                return Ok(Next::Continue);
                            }
                            let hint = self.value_candidates_hint(&name).unwrap_or_else(|| {
                                "Did you forget to create it with `let`?".to_string()
                            });
                            return Err(error_with_hint(
                                line,
                                bop::error_messages::variable_not_found(&name),
                                hint,
                            ));
                        }
                        let value = self
                            .lookup_var_mut_by_idx(name_idx)
                            .expect("binding checked above");
                        let val = apply_in_place_assign(
                            op,
                            rhs,
                            line,
                            || ops::index_get(value, &idx, line),
                        )?;
                        ops::index_set(value, &idx, val, line)?;
                    }
                }
            }

            // ─── String interpolation ────────────────────────────
            Instr::StringInterp(idx) => {
                let recipe_parts = Rc::clone(&self.current_chunk().interp(idx).parts);
                self.push_value(self.build_interp(&recipe_parts, line)?);
            }

            // ─── Collections ──────────────────────────────────────
            Instr::MakeArray(n) => {
                let items = self.pop_n_values(n as usize, line)?;
                self.push_value(Value::try_new_array(items, line)?);
            }
            Instr::MakeDict(n) => {
                let flat = self.pop_n_values((n as usize) * 2, line)?;
                let mut entries: Vec<(String, Value)> = Vec::with_capacity(n as usize);
                let mut iter = flat.into_iter();
                while let (Some(key), Some(val)) = (iter.next(), iter.next()) {
                    let key_str = match &key {
                        Value::Str(s) => s.as_str().to_string(),
                        other => {
                            return Err(error(
                                line,
                                format!("Dict keys must be strings, got {}", other.type_name()),
                            ));
                        }
                    };
                    drop(key);
                    entries.push((key_str, val));
                }
                self.push_value(Value::try_new_dict(entries, line)?);
            }

            // ─── Calls ────────────────────────────────────────────
            Instr::Call { name, argc } => {
                return self.call(name, argc as usize, line);
            }
            Instr::CallValue { argc } => {
                return self.call_value(argc as usize, line);
            }
            Instr::CallMethod {
                method,
                argc,
                assign_back_to,
                nested_place,
            } => {
                return self.call_method(
                    method,
                    argc as usize,
                    assign_back_to,
                    nested_place,
                    line,
                );
            }
            Instr::CallMethodInPlace {
                target,
                method,
                argc,
            } => {
                return self.call_method_in_place(target, method, argc as usize, line);
            }

            // ─── Functions ────────────────────────────────────────
            Instr::DefineFn(idx) => {
                self.define_fn(idx);
            }
            Instr::MakeLambda(idx) => {
                self.make_lambda(idx, line)?;
            }
            Instr::Return => {
                let v = self.pop_value(line)?;
                return self.do_return(v);
            }
            Instr::ReturnNone => {
                return self.do_return(Value::None);
            }

            // ─── Iteration / repeat ──────────────────────────────
            Instr::MakeIter => {
                let mut v = self.pop_value(line)?;
                // Fast path: Array / Str / Dict materialise eagerly
                // into a `Slot::Iter(Vec<_>)`. Every other iterable
                // goes through the protocol — either wraps a
                // `Value::Iter` directly, or dispatches the user's
                // `.iter()` method and lands the result via
                // `FrameWrap::IterStart`.
                match &mut v {
                    Value::Array(arr) => {
                        let mut items = arr.take();
                        drop(v);
                        items.reverse();
                        self.stack.push(Slot::Iter(items));
                    }
                    Value::Str(s) => {
                        let mut items: Vec<Value> = s
                            .chars()
                            .map(|c| Value::new_str(c.to_string()))
                            .collect();
                        drop(v);
                        items.reverse();
                        self.stack.push(Slot::Iter(items));
                    }
                    Value::Dict(d) => {
                        let mut items: Vec<Value> = d
                            .iter()
                            .map(|(k, _)| Value::new_str(k.clone()))
                            .collect();
                        drop(v);
                        items.reverse();
                        self.stack.push(Slot::Iter(items));
                    }
                    Value::Iter(_) => {
                        // Already an iterator — use as-is.
                        self.stack.push(Slot::IterObject(v));
                    }
                    Value::Struct(_) | Value::EnumVariant(_) => {
                        // Dispatch user `.iter()`. The result
                        // lands on the stack as a Slot::IterObject
                        // via `FrameWrap::IterStart`.
                        let iterable = v;
                        self.dispatch_iter_method(
                            iterable,
                            "iter",
                            Vec::new(),
                            FrameWrap::IterStart,
                            line,
                        )?;
                    }
                    _ => {
                        return Err(error(
                            line,
                            bop::error_messages::cant_iterate_over(v.type_name()),
                        ));
                    }
                }
            }
            Instr::IterNext { target } => {
                match self.stack.last_mut() {
                    Some(Slot::Iter(items)) => {
                        let next = items.pop();
                        match next {
                            Some(item) => self.push_value(item),
                            None => {
                                self.stack.pop();
                                self.jump(target);
                            }
                        }
                    }
                    Some(Slot::IterObject(iter_val)) => {
                        // Synchronous fast-path for the built-in
                        // iterator: `iter_method::next` runs in
                        // Rust, no bytecode frame push required.
                        if matches!(iter_val, Value::Iter(_)) {
                            let iter_clone = iter_val.clone();
                            let (result, _) = methods::iter_method(
                                &iter_clone,
                                "next",
                                &[],
                                line,
                            )?;
                            match unwrap_iter_step(&result) {
                                IterStep::Next(v) => self.push_value(v),
                                IterStep::Done => {
                                    self.stack.pop();
                                    self.jump(target);
                                }
                                IterStep::Malformed => {
                                    return Err(error(
                                        line,
                                        format!(
                                            "`.next()` on a `for` iterator must return `Iter::Next(v)` or `Iter::Done`, got {}",
                                            result.inspect()
                                        ),
                                    ));
                                }
                            }
                        } else {
                            // User-typed iterator — dispatch
                            // `.next()` through the normal method
                            // path. The return value is handled
                            // in `do_return` under `IterAdvance`.
                            let iter_clone = iter_val.clone();
                            self.dispatch_iter_method(
                                iter_clone,
                                "next",
                                Vec::new(),
                                FrameWrap::IterAdvance(target),
                                line,
                            )?;
                        }
                    }
                    _ => return Err(error(line, "VM: expected iterator on stack")),
                }
            }
            Instr::MakeRepeatCount => {
                let v = self.pop_value(line)?;
                let n = match v {
                    Value::Int(n) => n,
                    Value::Number(n) => n as i64,
                    other => {
                        return Err(error(
                            line,
                            format!("repeat needs a number, but got {}", other.type_name()),
                        ));
                    }
                };
                self.stack.push(Slot::Repeat(n.max(0)));
            }
            Instr::RepeatNext { target } => {
                let done = match self.stack.last_mut() {
                    Some(Slot::Repeat(n)) => {
                        if *n > 0 {
                            *n -= 1;
                            false
                        } else {
                            true
                        }
                    }
                    _ => return Err(error(line, "VM: expected repeat counter on stack")),
                };
                if done {
                    self.stack.pop();
                    self.jump(target);
                }
            }
            Instr::PopLoopState(kind) => {
                let matches_kind = matches!(
                    (kind, self.stack.last()),
                    (
                        LoopStateKind::Iterator,
                        Some(Slot::Iter(_) | Slot::IterObject(_)),
                    ) | (LoopStateKind::Repeat, Some(Slot::Repeat(_)))
                );
                if !matches_kind {
                    let expected = match kind {
                        LoopStateKind::Iterator => "iterator",
                        LoopStateKind::Repeat => "repeat counter",
                    };
                    return Err(error(
                        line,
                        format!("VM: expected {expected} loop state on stack"),
                    ));
                }
                self.stack.pop();
            }

            // ─── Control flow ─────────────────────────────────────
            Instr::Jump(t) => self.jump(t),
            Instr::JumpIfFalse(t) => {
                let v = self.pop_value(line)?;
                if !v.is_truthy() {
                    self.jump(t);
                }
            }
            Instr::JumpIfFalsePeek(t) => {
                let truthy = self.peek_value(line)?.is_truthy();
                if !truthy {
                    self.jump(t);
                }
            }
            Instr::JumpIfTruePeek(t) => {
                let truthy = self.peek_value(line)?.is_truthy();
                if truthy {
                    self.jump(t);
                }
            }

            // ─── Modules ─────────────────────────────────────────
            Instr::Use(idx) => {
                let spec = self.current_chunk().use_spec(idx).clone();
                self.exec_use(&spec, line)?;
            }

            // ─── User-defined types ──────────────────────────────
            Instr::DefineStruct(idx) => self.define_struct(idx, line)?,
            Instr::DefineEnum(idx) => self.define_enum(idx, line)?,
            Instr::DefineMethod {
                type_name,
                method_name,
                fn_idx,
            } => self.define_method(type_name, method_name, fn_idx),
            Instr::ValidateStructConstruct {
                namespace,
                type_name,
                fields,
            } => {
                let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
                self.validate_struct_construct(
                    namespace.map(|namespace| chunk.namespace_ref(namespace)),
                    chunk.name(type_name),
                    chunk.construct_fields(fields),
                    line,
                )?;
            }
            Instr::ValidateEnumConstruct {
                namespace,
                type_name,
                variant,
                shape,
                fields,
            } => {
                let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
                self.validate_enum_construct(
                    namespace.map(|namespace| chunk.namespace_ref(namespace)),
                    chunk.name(type_name),
                    chunk.name(variant),
                    shape,
                    chunk.construct_fields(fields),
                    line,
                )?;
            }
            Instr::ConstructStruct {
                namespace,
                type_name,
                count,
            } => {
                let namespace =
                    namespace.map(|namespace| self.current_chunk().namespace_ref(namespace));
                self.construct_struct(namespace, type_name, count as usize, line)?;
            }
            Instr::ConstructEnum {
                namespace,
                type_name,
                variant,
                shape,
            } => {
                let namespace =
                    namespace.map(|namespace| self.current_chunk().namespace_ref(namespace));
                self.construct_enum(namespace, type_name, variant, shape, line)?;
            }
            Instr::FieldGet(n) => {
                let field = self.current_chunk().name(n).to_string();
                let obj = self.pop_value(line)?;
                self.push_value(self.field_get(&obj, &field, line)?);
            }
            Instr::FieldSet(n) => {
                let field = self.current_chunk().name(n).to_string();
                let val = self.pop_value(line)?;
                let obj = self.pop_value(line)?;
                self.push_value(self.field_set(obj, &field, val, line)?);
            }
            Instr::FieldSetInPlace { target, field, op } => {
                let field = self.current_chunk().name(field).to_string();
                let rhs = self.pop_value(line)?;

                let value = match target {
                    crate::chunk::AssignBack::Slot(slot) => self
                        .frames
                        .last_mut()
                        .expect("frame present")
                        .slots
                        .get_mut(slot.0 as usize)
                        .ok_or_else(|| error(line, "VM: local slot out of range"))?,
                    crate::chunk::AssignBack::Name(name_idx) => {
                        let name = self.current_chunk().name(name_idx).to_string();
                        if self.lookup_var_mut_by_idx(name_idx).is_none() {
                            if let Some(module) = self.module_alias(&name).cloned() {
                                let value = Value::Module(module);
                                let val = apply_in_place_assign(
                                    op,
                                    rhs,
                                    line,
                                    || self.field_get(&value, &field, line),
                                )?;
                                self.field_set(value, &field, val, line)?;
                                return Ok(Next::Continue);
                            }
                            let hint = self.value_candidates_hint(&name).unwrap_or_else(|| {
                                "Did you forget to create it with `let`?".to_string()
                            });
                            return Err(error_with_hint(
                                line,
                                bop::error_messages::variable_not_found(&name),
                                hint,
                            ));
                        }
                        self.lookup_var_mut_by_idx(name_idx)
                            .expect("binding checked above")
                    }
                };
                match value {
                    Value::Struct(structure) => {
                        let type_name = structure.type_name().to_string();
                        let val = apply_in_place_assign(
                            op,
                            rhs,
                            line,
                            || {
                                structure.field(&field).cloned().ok_or_else(|| {
                                    error(
                                        line,
                                        bop::error_messages::struct_has_no_field(
                                            &type_name,
                                            &field,
                                        ),
                                    )
                                })
                            },
                        )?;
                        if !structure.try_set_field(&field, val, line)? {
                            return Err(error(
                                line,
                                bop::error_messages::struct_has_no_field(&type_name, &field),
                            ));
                        }
                    }
                    other => {
                        return Err(error(
                            line,
                            bop::error_messages::cant_assign_field(&field, other.type_name()),
                        ));
                    }
                }
            }

            // ─── Pattern matching ───────────────────────────────
            Instr::MatchFail { pattern, on_fail } => {
                self.match_fail(pattern, on_fail, line)?;
            }
            Instr::MatchExhausted => {
                return Err(error(line, "No match arm matched the scrutinee"));
            }

            // ─── try ─────────────────────────────────────────────
            Instr::TryUnwrap => {
                return self.try_unwrap(line);
            }

            // ─── Termination ──────────────────────────────────────
            Instr::Halt => return Ok(Next::Halt),
        }

        Ok(Next::Continue)
    }

    // ─── Binary helpers ──────────────────────────────────────────

    fn binary(
        &mut self,
        line: u32,
        op: fn(&Value, &Value, u32) -> Result<Value, BopError>,
    ) -> Result<(), BopError> {
        let b = self.pop_value(line)?;
        let a = self.pop_value(line)?;
        self.push_value(op(&a, &b, line)?);
        Ok(())
    }

    fn binary_infallible(
        &mut self,
        line: u32,
        op: fn(&Value, &Value, u32) -> Result<Value, BopError>,
    ) -> Result<(), BopError> {
        let b = self.pop_value(line)?;
        let a = self.pop_value(line)?;
        self.push_value(op(&a, &b, line)?);
        Ok(())
    }

    // ─── String interpolation ────────────────────────────────────

    fn build_interp(&self, parts: &[InterpPart], line: u32) -> Result<Value, BopError> {
        let mut result = String::new();
        for part in parts {
            match part {
                InterpPart::Literal(s) => result.push_str(s),
                InterpPart::Local(slot) => {
                    let v = self
                        .frames
                        .last()
                        .and_then(|frame| frame.slots.get(slot.0 as usize))
                        .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                    result.push_str(&format!("{}", v));
                }
                InterpPart::Name(name_idx) => {
                    let name = self.current_chunk().name(*name_idx);
                    if let Some(value) = self.lookup_var_by_idx(*name_idx) {
                        result.push_str(&format!("{}", value));
                    } else if let Some(module) = self.module_alias(name) {
                        result.push_str(&format!("{}", Value::Module(Rc::clone(module))));
                    } else {
                        return Err(error(
                            line,
                            bop::error_messages::variable_not_found(name),
                        ));
                    }
                }
            }
        }
        Ok(Value::new_str(result))
    }

    // ─── Chunk accessor ──────────────────────────────────────────

    fn current_chunk(&self) -> &Chunk {
        &self.frames.last().expect("frame present").chunk
    }

    // ─── Calls ───────────────────────────────────────────────────

    fn call(&mut self, name_idx: NameIdx, argc: usize, line: u32) -> Result<Next, BopError> {
        // Borrow the name out of the chunk without allocating a
        // fresh `String` per call — `fib(25)` dispatches this
        // path ~75 000 times and even one small allocation per
        // call shows up in profiles. Holding a local `Rc<Chunk>`
        // keeps the name slice valid for the whole body.
        let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
        let name: &str = chunk.name(name_idx);

        // Check lexical shadowing first (a `let f = fn() {...}`
        // must win over a same-named builtin / user fn). We peek
        // rather than clone so the common case — no shadow —
        // pays nothing.
        let current_value = self
            .current_scopes()
            .iter()
            .rev()
            .find_map(|scope| scope.get(name))
            .cloned();
        if let Some(value) = current_value {
            let args = self.pop_n_values(argc, line)?;
            return match &value {
                Value::Fn(f) => {
                    let f = Rc::clone(f);
                    drop(value);
                    self.call_closure(&f, args, line)
                }
                other => Err(error(
                    line,
                    format!(
                        "`{}` is a {}, not a function",
                        name,
                        other.type_name()
                    ),
                )),
            };
        }

        // A defining-module declaration alias is a protected value binding.
        // Keep the borrowed local peek above as the hot path, then consult the
        // lexical context only when no local/parameter shadows the name. Clone
        // the module handle solely on this rare alias-call error path.
        let declaration_alias = self.frames.last().and_then(|frame| {
            if frame.lexical_context.module_aliases.is_empty() {
                None
            } else {
                frame.lexical_context.module_aliases.get(name).cloned()
            }
        });
        if let Some(module) = declaration_alias {
            let _args = self.pop_n_values(argc, line)?;
            return Err(error(
                line,
                format!(
                    "`{}` is a {}, not a function",
                    name,
                    Value::Module(module).type_name()
                ),
            ));
        }

        if let Some(entry) = self.imported_function(name).cloned() {
            if argc != entry.params.len() {
                return Err(error(
                    line,
                    format!(
                        "`{}` expects {} argument{}, but got {}",
                        name,
                        entry.params.len(),
                        if entry.params.len() == 1 { "" } else { "s" },
                        argc
                    ),
                ));
            }
            drop(chunk);
            return self.enter_user_fn(entry, argc, line);
        }

        if self.frames.last().is_some_and(|frame| {
            frame.is_function
                && frame.function_module.as_deref() == Some(bop::value::ROOT_MODULE_PATH)
        }) {
            let root_value = self
                .frames
                .first()
                .and_then(|root| root.scopes.iter().rev().find_map(|scope| scope.get(name)))
                .cloned();
            if let Some(value) = root_value {
                let args = self.pop_n_values(argc, line)?;
                return match &value {
                    Value::Fn(function) => {
                        let function = Rc::clone(function);
                        self.call_closure(&function, args, line)
                    }
                    other => Err(error(
                        line,
                        format!("`{}` is a {}, not a function", name, other.type_name()),
                    )),
                };
            }
        }

        // User-fn hot path: if `name` is a declared fn and no
        // local shadow exists, pop args straight into the new
        // frame's scope — no intermediate `Vec<Value>` allocation
        // per call. Callsite profile on `fib(28)` showed this
        // path taking ~90 % of all dispatched calls; eliminating
        // the per-call `Vec` cost takes ~500 000 small heap
        // allocations off the table.
        //
        // SAFETY-of-reorder: we've already ruled out lexical
        // shadowing above, so moving the user-fn check in front
        // of builtins / host matches the "let wins over fn; fn
        // wins over builtin" ordering the walker uses. (Walker:
        // locals checked at Call site, then call_function
        // matches builtins — a user-defined `fn print` never
        // reaches this path because `print` isn't in the
        // walker's `functions` table before `fn print` runs.
        // Same here: DefineFn populates `self.functions` only
        // for user-declared names, so builtins like `range`,
        // `print` don't collide.)
        if let Some(entry) = self.lookup_function_entry(name) {
            // Argc check before we touch the stack so error
            // messages match the walker's `name` wording.
            if argc != entry.params.len() {
                return Err(error(
                    line,
                    format!(
                        "`{}` expects {} argument{}, but got {}",
                        name,
                        entry.params.len(),
                        if entry.params.len() == 1 { "" } else { "s" },
                        argc
                    ),
                ));
            }
            drop(chunk);
            return self.enter_user_fn(entry, argc, line);
        }

        // Everything from here down needs the args as a
        // contiguous slice, so pay the `Vec` cost once.
        let args = self.pop_n_values(argc, line)?;

        // 1. Global builtins — the narrow set that can't be
        // expressed as methods on a receiver (variadic,
        // constructor-shape, session-stateful, or takes a
        // callable). Everything that used to live here is now
        // a method; see `methods::common_method` and
        // `methods::numeric_method`.
        match name {
            "range" => {
                let v = builtins::builtin_range(&args, line, &mut self.rand_state)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            "rand" => {
                let v = builtins::builtin_rand(&args, line, &mut self.rand_state)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            "print" => {
                let message = args
                    .iter()
                    .map(|a| format!("{}", a))
                    .collect::<Vec<_>>()
                    .join(" ");
                let _suspended = bop::memory::ActiveMemoryGuard::__suspend();
                self.host.on_print(&message);
                self.push_value(Value::None);
                return Ok(Next::Continue);
            }
            "try_call" => return self.builtin_try_call(args, line),
            "panic" => {
                // `builtin_panic` always returns `Err`; the `Ok`
                // arm is unreachable, but matching keeps the
                // compiler happy and keeps this branch symmetric
                // with the other builtins.
                let v = builtins::builtin_panic(&args, line)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            _ => {}
        }

        // 2. Host-provided builtins.
        let host_result = {
            let _suspended = bop::memory::ActiveMemoryGuard::__suspend();
            self.host.call(name, &args, line)
        };
        if let Some(result) = host_result {
            let v = result?;
            self.push_value(v);
            return Ok(Next::Continue);
        }

        // Nothing left to try — the name is unresolved. The
        // user-fn fast path above already handled the success
        // case; we only reach here when no declared fn matches
        // either, so we can go straight to the "did you mean?"
        // / host-hint path.
        if let Some(hint) = self.callable_candidates_hint(name) {
            return Err(error_with_hint(
                line,
                bop::error_messages::function_not_found(name),
                hint,
            ));
        }
        let host_hint = {
            let _suspended = bop::memory::ActiveMemoryGuard::__suspend();
            self.host.function_hint().to_string()
        };
        Err(if host_hint.is_empty() {
            error(line, bop::error_messages::function_not_found(name))
        } else {
            error_with_hint(
                line,
                bop::error_messages::function_not_found(name),
                host_hint,
            )
        })
    }

    /// Fast path for user-defined function calls: pop `argc`
    /// args straight off the value stack into the new frame's
    /// parameter scope, and push the frame. No intermediate
    /// `Vec<Value>`. Assumes the caller has already validated
    /// `argc == entry.params.len()` so the per-call error
    /// message can include the target name.
    fn enter_user_fn(
        &mut self,
        entry: Rc<FnEntry>,
        argc: usize,
        line: u32,
    ) -> Result<Next, BopError> {
        if self.frames.len().saturating_sub(1) >= MAX_CALL_DEPTH {
            return Err(error_with_hint(
                line,
                "Too many nested function calls (possible infinite recursion)",
                "Check that your recursive function has a base case that stops calling itself.",
            ));
        }

        if self.stack.len() < argc {
            return Err(error(line, "VM: stack underflow"));
        }

        // Build the frame's flat slot array: args go into the
        // first `argc` slots (by compile-time-assigned order),
        // the rest of `slot_count` are pre-seeded with `None` so
        // later `StoreLocal`s land in-bounds. Backing allocation
        // comes from the freelist when possible — ~500k per-call
        // heap allocs under `fib(28)` fold into a steady-state
        // set of ~MAX_CALL_DEPTH reused vecs.
        let slot_count = (entry.chunk.slot_count as usize).max(argc);
        let mut slots = self.take_slots(slot_count);
        let start = self.stack.len() - argc;
        for i in 0..argc {
            let slot = core::mem::replace(
                &mut self.stack[start + i],
                Slot::Value(Value::None),
            );
            match slot {
                Slot::Value(v) => slots[i] = v,
                _ => {
                    self.stack.truncate(start);
                    self.return_slots(slots);
                    return Err(error(line, "VM: expected value on stack"));
                }
            }
        }
        self.stack.truncate(start);

        self.push_function_frame(
            entry.chunk.clone(),
            slots,
            // Function frames don't use the BTreeMap scope stack
            // on the fast path — captures are on the `Value::Fn`
            // itself (handled in `call_closure`), and all locals
            // live in `slots`. An empty `scopes` vec is cheap to
            // allocate and keeps `LoadVar` / `DefineLocal` from
            // panicking if they still get emitted (they shouldn't
            // inside a fn body, but the fallback is safe).
            Vec::new(),
            self.stack.len(),
            Some(entry.module_path.clone()),
            FrameWrap::None,
        );
        // A fresh type_bindings frame scopes any type decl
        // inside this fn to the fn body itself — same rule
        // as push_scope / pop_scope for block scoping. On
        // `do_return` we pop this frame.
        Ok(Next::Continue)
    }

    /// Dispatch a value-based call: the callee sits on top of the
    /// `argc` args. Pops all `argc + 1` slots,
    /// expects the callee to be a `Value::Fn`, and delegates to
    /// `call_closure`.
    fn call_value(&mut self, argc: usize, line: u32) -> Result<Next, BopError> {
        let callee = self.pop_value(line)?;
        let args = self.pop_n_values(argc, line)?;
        self.invoke_value(callee, args, line)
    }

    /// Invoke an already-evaluated callable with already-popped
    /// args. Shared between `Instr::CallValue` and the
    /// `Value::Module` fast path in `call_method` (where
    /// `m.foo(...)` resolves the fn through a module field
    /// lookup rather than going through the stack callee slot).
    fn invoke_value(
        &mut self,
        callee: Value,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Next, BopError> {
        match &callee {
            Value::Fn(f) => {
                let f = Rc::clone(f);
                drop(callee);
                self.call_closure(&f, args, line)
            }
            other => Err(error(
                line,
                bop::error_messages::cant_call_a(other.type_name()),
            )),
        }
    }

    fn call_method(
        &mut self,
        method_idx: NameIdx,
        argc: usize,
        assign_back_to: Option<crate::chunk::AssignBack>,
        nested_place: bool,
        line: u32,
    ) -> Result<Next, BopError> {
        let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
        let method = chunk.name(method_idx);

        let obj = self.pop_value(line)?;
        let args = self.pop_n_values(argc, line)?;

        self.dispatch_method(obj, method, args, assign_back_to, nested_place, line)
    }

    fn call_method_in_place(
        &mut self,
        target: crate::chunk::NamespaceRef,
        method_idx: NameIdx,
        argc: usize,
        line: u32,
    ) -> Result<Next, BopError> {
        let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
        let method = chunk.name(method_idx);
        let receiver_idx = target.name_idx();
        let receiver_name = chunk.name(receiver_idx);
        let args = self.pop_n_values(argc, line)?;

        // The compiler only emits this instruction for a bare identifier and
        // a built-in mutating method name. Resolve the binding after argument
        // evaluation, matching the walker/AOT order. Arrays take the direct
        // path; every other value is cloned once for ordinary dispatch so a
        // user type is still free to define a method named `push`, `pop`, etc.
        let (fallback, assign_back) = match target.slot_idx() {
            Some(slot) => {
                let value = self
                    .frames
                    .last_mut()
                    .expect("frame present")
                    .slots
                    .get_mut(slot.0 as usize)
                    .ok_or_else(|| error(line, "VM: local slot out of range"))?;
                if let Value::Array(array) = value {
                    methods::reject_constant_array_mutation(receiver_name, method, line)?;
                    let result = methods::array_method_mut(array, method, args, line)?;
                    self.push_value(result);
                    return Ok(Next::Continue);
                }
                (
                    value.clone(),
                    crate::chunk::AssignBack::Slot(slot),
                )
            }
            None => {
                if self.lookup_var_mut_by_idx(receiver_idx).is_none() {
                    if let Some(module) = self.module_alias(receiver_name).cloned() {
                        return self.dispatch_method(
                            Value::Module(module),
                            method,
                            args,
                            None,
                            false,
                            line,
                        );
                    }
                    let hint = self.value_candidates_hint(receiver_name).unwrap_or_else(|| {
                        "Did you forget to create it with `let`?".to_string()
                    });
                    return Err(error_with_hint(
                        line,
                        bop::error_messages::variable_not_found(receiver_name),
                        hint,
                    ));
                }
                let value = self
                    .lookup_var_mut_by_idx(receiver_idx)
                    .expect("binding checked above");
                if let Value::Array(array) = value {
                    methods::reject_constant_array_mutation(receiver_name, method, line)?;
                    let result = methods::array_method_mut(array, method, args, line)?;
                    self.push_value(result);
                    return Ok(Next::Continue);
                }
                (
                    value.clone(),
                    crate::chunk::AssignBack::Name(receiver_idx),
                )
            }
        };

        self.dispatch_method(fallback, method, args, Some(assign_back), false, line)
    }

    fn dispatch_method(
        &mut self,
        obj: Value,
        method: &str,
        args: Vec<Value>,
        assign_back_to: Option<crate::chunk::AssignBack>,
        nested_place: bool,
        line: u32,
    ) -> Result<Next, BopError> {
        // `m.foo(args)` on a module alias: there's no struct /
        // enum receiver — `m` is a `Value::Module` whose `foo`
        // export is a callable. Look it up and dispatch through
        // the regular call-by-value path.
        //
        // Before falling through to the binding lookup, check
        // whether the method name is one of the common methods
        // (`type`, `to_str`, `inspect`). Those work on every
        // value, including `Value::Module`, and shouldn't be
        // gated by whether the module happens to export that
        // name.
        if let Value::Module(ref m) = obj {
            if let Some(result) = methods::common_method(&obj, method, &args, line)? {
                self.push_value(result.0);
                return Ok(Next::Continue);
            }
            if let Some(callee) = self.module_binding(m, method) {
                drop(obj);
                return self.invoke_value(callee, args, line);
            }
            return Err(error(
                line,
                format!("`{}` isn't exported from `{}`", method, m.path),
            ));
        }

        // User-method dispatch comes first — any method declared
        // on the receiver's full type identity wins over the
        // built-in method of the same name, matching the walker.
        // Dispatch is strict: `fn paint.Color.shade(self)` never
        // fires on `other.Color` even though the bare name is
        // the same.
        let user_type_key: Option<(String, String)> = match &obj {
            Value::Struct(s) => Some((
                s.module_path().to_string(),
                s.type_name().to_string(),
            )),
            Value::EnumVariant(e) => Some((
                e.module_path().to_string(),
                e.type_name().to_string(),
            )),
            _ => None,
        };
        if let Some(type_key) = user_type_key {
            let entry = self
                .user_methods
                .get(&type_key)
                .and_then(|m| m.get(method))
                .cloned();
            if let Some(entry) = entry {
                if entry.params.len() != args.len() + 1 {
                    return Err(error(
                        line,
                        format!(
                            "`{}.{}` expects {} argument{} (including `self`), but got {}",
                            type_key.1,
                            method,
                            entry.params.len(),
                            if entry.params.len() == 1 { "" } else { "s" },
                            args.len() + 1
                        ),
                    ));
                }
                if self.frames.len().saturating_sub(1) >= MAX_CALL_DEPTH {
                    return Err(error_with_hint(
                        line,
                        "Too many nested function calls (possible infinite recursion)",
                        "Check that your recursive function has a base case that stops calling itself.",
                    ));
                }
                // User methods use the same slot-based frame
                // layout as regular fn calls. `self` takes slot 0,
                // remaining params take 1..entry.params.len().
                let slot_count = (entry.chunk.slot_count as usize)
                    .max(entry.params.len());
                let mut slots = self.take_slots(slot_count);
                slots[0] = obj;
                for (i, a) in args.into_iter().enumerate() {
                    slots[i + 1] = a;
                }
                self.push_function_frame(
                    entry.chunk.clone(),
                    slots,
                    Vec::new(),
                    self.stack.len(),
                    Some(entry.module_path.clone()),
                    FrameWrap::None,
                );
                // User methods don't do mutation back-assign
                // — the receiver is passed by value, and the
                // method returns a fresh instance if it wants to
                // "mutate". Matches the walker's convention.
                let _ = assign_back_to;
                return Ok(Next::Continue);
            }
        }

        if nested_place {
            methods::reject_nested_array_mutation(&obj, method, line)?;
        }

        // `type` / `to_str` / `inspect` work on every value —
        // dispatch them ahead of the type-specific tables so
        // walker / VM / AOT agree on the common method surface.
        if let Some((ret, _)) = methods::common_method(&obj, method, &args, line)? {
            self.push_value(ret);
            return Ok(Next::Continue);
        }

        // Built-in `Result` combinators. Pure methods (`is_ok`,
        // `unwrap`, …) return through the regular path; callable-
        // taking ones (`map`, `map_err`, `and_then`) push a
        // closure frame with a `FrameWrap` that wraps the return
        // in `Ok`/`Err` or passes it through.
        if methods::is_builtin_result(&obj) {
            if let Some(v) = methods::result_method(&obj, method, &args, line)? {
                self.push_value(v);
                return Ok(Next::Continue);
            }
            if let Some(kind) = methods::is_result_callable_method(method) {
                return self.call_result_callable_method(obj, kind, method, args, line);
            }
        }

        let (ret, mutated) = match &obj {
            Value::Array(arr) => {
                methods::array_method(arr, method, &args, line)?
            }
            Value::Str(s) => {
                methods::string_method(s.as_str(), method, &args, line)?
            }
            Value::Dict(entries) => {
                methods::dict_method(entries, method, &args, line)?
            }
            Value::Int(_) | Value::Number(_) => {
                methods::numeric_method(&obj, method, &args, line)?
            }
            Value::Bool(_) => {
                methods::bool_method(&obj, method, &args, line)?
            }
            Value::Iter(_) => {
                methods::iter_method(&obj, method, &args, line)?
            }
            _ => {
                return Err(error(
                    line,
                    bop::error_messages::no_such_method(obj.type_name(), method),
                ));
            }
        };

        if methods::is_mutating_method(method) {
            if let (Some(target), Some(new_obj)) = (assign_back_to, mutated) {
                match target {
                    crate::chunk::AssignBack::Slot(slot) => {
                        let frame = self.frames.last_mut().expect("frame present");
                        let i = slot.0 as usize;
                        if i < frame.slots.len() {
                            frame.slots[i] = new_obj;
                        }
                        // Out-of-range slot: silently drop the
                        // mutation. The compiler only emits
                        // `Slot(i)` for a slot it knows exists,
                        // so this branch is effectively a
                        // miscompile guard.
                    }
                    crate::chunk::AssignBack::Name(var_idx) => {
                        let var_name = self.current_chunk().name(var_idx).to_string();
                        self.set_existing(&var_name, new_obj);
                    }
                }
            }
        }
        self.push_value(ret);
        Ok(Next::Continue)
    }

    fn define_struct(&mut self, idx: StructIdx, line: u32) -> Result<(), BopError> {
        let def = self.current_chunk().struct_def(idx).clone();
        let mut seen = BTreeSet::new();
        for field in &def.fields {
            if !seen.insert(field.clone()) {
                return Err(error(
                    line,
                    format!(
                        "Struct `{}` has duplicate field `{}`",
                        def.name, field
                    ),
                ));
            }
        }
        // Type identity is `(current_module, def.name)`. Two
        // different modules declaring the same name coexist at
        // distinct registry keys; same-module redecl is a no-op
        // on matching shape and an error otherwise.
        let key = (self.current_module.clone(), def.name.clone());
        if let Some(existing) = self.struct_defs.get(&key) {
            if existing == &def.fields {
                self.bind_local_type(&def.name);
                return Ok(());
            }
            return Err(error(
                line,
                format!("Struct `{}` is already declared", def.name),
            ));
        }
        self.struct_defs.insert(key, def.fields);
        self.bind_local_type(&def.name);
        Ok(())
    }

    fn define_enum(&mut self, idx: EnumIdx, line: u32) -> Result<(), BopError> {
        let def = self.current_chunk().enum_def(idx).clone();
        let variants: Vec<(String, EnumVariantShape)> = def
            .variants
            .into_iter()
            .map(|v| (v.name, v.shape))
            .collect();
        let key = (self.current_module.clone(), def.name.clone());
        if let Some(existing) = self.enum_defs.get(&key) {
            if shapes_match(existing, &variants) {
                self.bind_local_type(&def.name);
                return Ok(());
            }
            return Err(error(
                line,
                format!("Enum `{}` is already declared", def.name),
            ));
        }
        let mut seen_variants = BTreeSet::new();
        for (variant, shape) in &variants {
            if !seen_variants.insert(variant.clone()) {
                return Err(error(
                    line,
                    format!(
                        "Enum `{}` has duplicate variant `{}`",
                        def.name, variant
                    ),
                ));
            }
            let fields = match shape {
                EnumVariantShape::Unit => continue,
                EnumVariantShape::Tuple(fields)
                | EnumVariantShape::Struct(fields) => fields,
            };
            let mut seen_fields = BTreeSet::new();
            for field in fields {
                if !seen_fields.insert(field.clone()) {
                    return Err(error(
                        line,
                        format!(
                            "Enum variant `{}::{}` has duplicate field `{}`",
                            def.name, variant, field
                        ),
                    ));
                }
            }
        }
        self.enum_defs.insert(key, variants);
        self.bind_local_type(&def.name);
        Ok(())
    }

    /// Bind a declared type's bare name in the current
    /// `type_bindings` frame so subsequent references resolve
    /// to *this* module's version.
    fn bind_local_type(&mut self, name: &str) {
        if let Some(scope) = self.type_bindings.last_mut() {
            scope.insert(name.to_string(), self.current_module.clone());
        }
        if self.is_module_top_scope() {
            self.type_exports
                .insert(name.to_string(), self.current_module.clone());
            self.refresh_root_lexical_context();
        }
    }

    fn define_method(
        &mut self,
        type_name: NameIdx,
        method_name: NameIdx,
        fn_idx: FnIdx,
    ) {
        let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
        let type_name_s = chunk.name(type_name).to_string();
        let method_name_s = chunk.name(method_name).to_string();
        let fn_def = chunk.function(fn_idx);
        let entry = Rc::new(FnEntry {
            params: fn_def.params.clone(),
            chunk: Rc::clone(&fn_def.chunk),
            module_path: self.current_module.clone(),
        });
        // Methods attach to the *full* receiver-type identity.
        // A method declared inside `paint` for `Color` only
        // fires on `paint.Color` values, not on same-named
        // types from other modules.
        let key = (self.current_module.clone(), type_name_s);
        self.user_methods
            .entry(key)
            .or_default()
            .insert(method_name_s, entry);
    }

    fn construct_struct(
        &mut self,
        namespace: Option<crate::chunk::NamespaceRef>,
        type_name: NameIdx,
        count: usize,
        line: u32,
    ) -> Result<(), BopError> {
        let type_name_s = self.current_chunk().name(type_name).to_string();
        // Resolve the source-level reference to its full type
        // identity before validating the shape. `namespace`
        // means `ns.Type { ... }`; bare means the scope walker
        // looks up `Type` in `type_bindings`.
        let module_path = match namespace {
            Some(namespace) => self.resolve_namespaced_type(namespace, &type_name_s, line)?,
            None => self
                .resolve_type_ref(None, &type_name_s)
                .ok_or_else(|| {
                    error(
                        line,
                        bop::error_messages::struct_not_declared(&type_name_s),
                    )
                })?,
        };
        let key = (module_path.clone(), type_name_s.clone());
        let decl = self
            .struct_defs
            .get(&key)
            .ok_or_else(|| {
                error(
                    line,
                    bop::error_messages::struct_not_declared(&type_name_s),
                )
            })?
            .clone();
        let flat = self.pop_n_values(count * 2, line)?;
        let mut provided: BTreeMap<String, Value> = BTreeMap::new();
        let mut iter = flat.into_iter();
        while let (Some(key), Some(val)) = (iter.next(), iter.next()) {
            let key_str = match &key {
                Value::Str(s) => s.as_str().to_string(),
                other => {
                    return Err(error(
                        line,
                        format!(
                            "Struct field names must be strings, got {}",
                            other.type_name()
                        ),
                    ));
                }
            };
            drop(key);
            if provided.contains_key(&key_str) {
                return Err(error(
                    line,
                    format!(
                        "Field `{}` specified twice in `{}` construction",
                        key_str, type_name_s
                    ),
                ));
            }
            if !decl.iter().any(|d| d == &key_str) {
                let msg = bop::error_messages::struct_has_no_field(
                    &type_name_s,
                    &key_str,
                );
                let err = match bop::suggest::did_you_mean(
                    &key_str,
                    decl.iter().map(|s| s.as_str()),
                ) {
                    Some(hint) => error_with_hint(line, msg, hint),
                    None => error(line, msg),
                };
                return Err(err);
            }
            provided.insert(key_str, val);
        }
        let mut fields: Vec<(String, Value)> = Vec::with_capacity(decl.len());
        for d in &decl {
            match provided.remove(d) {
                Some(v) => fields.push((d.clone(), v)),
                None => {
                    return Err(error(
                        line,
                        format!(
                            "Missing field `{}` in `{}` construction",
                            d, type_name_s
                        ),
                    ));
                }
            }
        }
        self.push_value(Value::try_new_struct(module_path, type_name_s, fields, line)?);
        Ok(())
    }

    fn validate_struct_construct(
        &self,
        namespace: Option<crate::chunk::NamespaceRef>,
        type_name: &str,
        provided: &[String],
        line: u32,
    ) -> Result<(), BopError> {
        let module_path = match namespace {
            Some(namespace) => self.resolve_namespaced_type(namespace, type_name, line)?,
            None => self.resolve_type_ref(None, type_name).ok_or_else(|| {
                error(line, bop::error_messages::struct_not_declared(type_name))
            })?,
        };
        let declared = self
            .struct_defs
            .get(&(module_path, type_name.to_string()))
            .ok_or_else(|| {
                error(line, bop::error_messages::struct_not_declared(type_name))
            })?;
        for (index, field) in provided.iter().enumerate() {
            if provided[..index].contains(field) {
                return Err(error(
                    line,
                    format!(
                        "Field `{}` specified twice in `{}` construction",
                        field, type_name
                    ),
                ));
            }
            if !declared.contains(field) {
                let message = bop::error_messages::struct_has_no_field(type_name, field);
                return match bop::suggest::did_you_mean(
                    field,
                    declared.iter().map(|name| name.as_str()),
                ) {
                    Some(hint) => Err(error_with_hint(line, message, hint)),
                    None => Err(error(line, message)),
                };
            }
        }
        for field in declared {
            if !provided.contains(field) {
                return Err(error(
                    line,
                    format!(
                        "Missing field `{}` in `{}` construction",
                        field, type_name
                    ),
                ));
            }
        }
        Ok(())
    }

    fn validate_enum_construct(
        &self,
        namespace: Option<crate::chunk::NamespaceRef>,
        type_name: &str,
        variant: &str,
        shape: EnumConstructShape,
        provided: &[String],
        line: u32,
    ) -> Result<(), BopError> {
        let module_path = match namespace {
            Some(namespace) => self.resolve_namespaced_type(namespace, type_name, line)?,
            None => self.resolve_type_ref(None, type_name).ok_or_else(|| {
                error(line, bop::error_messages::enum_not_declared(type_name))
            })?,
        };
        let variants = self
            .enum_defs
            .get(&(module_path, type_name.to_string()))
            .ok_or_else(|| error(line, bop::error_messages::enum_not_declared(type_name)))?;
        let declared = variants
            .iter()
            .find(|(name, _)| name == variant)
            .map(|(_, shape)| shape)
            .ok_or_else(|| {
                let message = bop::error_messages::enum_has_no_variant(type_name, variant);
                match bop::suggest::did_you_mean(
                    variant,
                    variants.iter().map(|(name, _)| name.as_str()),
                ) {
                    Some(hint) => error_with_hint(line, message, hint),
                    None => error(line, message),
                }
            })?;
        match (declared, shape) {
            (EnumVariantShape::Unit, EnumConstructShape::Unit) => Ok(()),
            (EnumVariantShape::Tuple(fields), EnumConstructShape::Tuple(argc)) => {
                if fields.len() == argc as usize {
                    Ok(())
                } else {
                    Err(error(
                        line,
                        format!(
                            "`{}::{}` expects {} argument{}, but got {}",
                            type_name,
                            variant,
                            fields.len(),
                            if fields.len() == 1 { "" } else { "s" },
                            argc
                        ),
                    ))
                }
            }
            (EnumVariantShape::Struct(fields), EnumConstructShape::Struct(_)) => {
                for (index, field) in provided.iter().enumerate() {
                    if provided[..index].contains(field) {
                        return Err(error(
                            line,
                            format!(
                                "Field `{}` specified twice in `{}::{}`",
                                field, type_name, variant
                            ),
                        ));
                    }
                    if !fields.contains(field) {
                        return Err(error(
                            line,
                            bop::error_messages::variant_has_no_field(
                                type_name, variant, field,
                            ),
                        ));
                    }
                }
                for field in fields {
                    if !provided.contains(field) {
                        return Err(error(
                            line,
                            format!(
                                "Missing field `{}` in `{}::{}` construction",
                                field, type_name, variant
                            ),
                        ));
                    }
                }
                Ok(())
            }
            (EnumVariantShape::Unit, _) => Err(error(
                line,
                format!("Variant `{}::{}` takes no payload", type_name, variant),
            )),
            (EnumVariantShape::Tuple(_), _) => Err(error(
                line,
                format!(
                    "Variant `{}::{}` expects positional arguments `(…)`",
                    type_name, variant
                ),
            )),
            (EnumVariantShape::Struct(_), _) => Err(error(
                line,
                format!(
                    "Variant `{}::{}` expects named fields `{{ … }}`",
                    type_name, variant
                ),
            )),
        }
    }

    fn construct_enum(
        &mut self,
        namespace: Option<crate::chunk::NamespaceRef>,
        type_name: NameIdx,
        variant: NameIdx,
        shape: EnumConstructShape,
        line: u32,
    ) -> Result<(), BopError> {
        let type_name_s = self.current_chunk().name(type_name).to_string();
        let variant_s = self.current_chunk().name(variant).to_string();
        let module_path = match namespace {
            Some(namespace) => self.resolve_namespaced_type(namespace, &type_name_s, line)?,
            None => self
                .resolve_type_ref(None, &type_name_s)
                .ok_or_else(|| {
                    error(line, bop::error_messages::enum_not_declared(&type_name_s))
                })?,
        };
        let key = (module_path.clone(), type_name_s.clone());
        let decl = self
            .enum_defs
            .get(&key)
            .ok_or_else(|| {
                error(line, bop::error_messages::enum_not_declared(&type_name_s))
            })?
            .clone();
        let variant_decl = decl
            .iter()
            .find(|(n, _)| n == &variant_s)
            .cloned()
            .ok_or_else(|| {
                let msg = bop::error_messages::enum_has_no_variant(
                    &type_name_s,
                    &variant_s,
                );
                match bop::suggest::did_you_mean(
                    &variant_s,
                    decl.iter().map(|(n, _)| n.as_str()),
                ) {
                    Some(hint) => error_with_hint(line, msg, hint),
                    None => error(line, msg),
                }
            })?;
        match (&variant_decl.1, shape) {
            (EnumVariantShape::Unit, EnumConstructShape::Unit) => {
                self.push_value(Value::new_enum_unit(module_path, type_name_s, variant_s));
            }
            (EnumVariantShape::Tuple(fields), EnumConstructShape::Tuple(argc)) => {
                if fields.len() as u32 != argc {
                    return Err(error(
                        line,
                        format!(
                            "`{}::{}` expects {} argument{}, but got {}",
                            type_name_s,
                            variant_s,
                            fields.len(),
                            if fields.len() == 1 { "" } else { "s" },
                            argc
                        ),
                    ));
                }
                let items = self.pop_n_values(argc as usize, line)?;
                self.push_value(Value::try_new_enum_tuple(
                    module_path,
                    type_name_s,
                    variant_s,
                    items,
                    line,
                )?);
            }
            (EnumVariantShape::Struct(decl_fields), EnumConstructShape::Struct(count)) => {
                let flat = self.pop_n_values(count as usize * 2, line)?;
                let mut provided: BTreeMap<String, Value> = BTreeMap::new();
                let mut iter = flat.into_iter();
                while let (Some(key), Some(val)) = (iter.next(), iter.next()) {
                    let key_str = match &key {
                        Value::Str(s) => s.as_str().to_string(),
                        _ => {
                            return Err(error(
                                line,
                                "Enum struct-variant field names must be strings",
                            ));
                        }
                    };
                    drop(key);
                    if provided.contains_key(&key_str) {
                        return Err(error(
                            line,
                            format!(
                                "Field `{}` specified twice in `{}::{}`",
                                key_str, type_name_s, variant_s
                            ),
                        ));
                    }
                    if !decl_fields.iter().any(|d| d == &key_str) {
                        return Err(error(
                            line,
                            bop::error_messages::variant_has_no_field(
                                &type_name_s,
                                &variant_s,
                                &key_str,
                            ),
                        ));
                    }
                    provided.insert(key_str, val);
                }
                let mut fields: Vec<(String, Value)> =
                    Vec::with_capacity(decl_fields.len());
                for d in decl_fields {
                    match provided.remove(d) {
                        Some(v) => fields.push((d.clone(), v)),
                        None => {
                            return Err(error(
                                line,
                                format!(
                                    "Missing field `{}` in `{}::{}` construction",
                                    d, type_name_s, variant_s
                                ),
                            ));
                        }
                    }
                }
                self.push_value(Value::try_new_enum_struct(
                    module_path,
                    type_name_s,
                    variant_s,
                    fields,
                    line,
                )?);
            }
            (EnumVariantShape::Unit, _) => {
                return Err(error(
                    line,
                    format!("Variant `{}::{}` takes no payload", type_name_s, variant_s),
                ));
            }
            (EnumVariantShape::Tuple(_), _) => {
                return Err(error(
                    line,
                    format!(
                        "Variant `{}::{}` expects positional arguments `(…)`",
                        type_name_s, variant_s
                    ),
                ));
            }
            (EnumVariantShape::Struct(_), _) => {
                return Err(error(
                    line,
                    format!(
                        "Variant `{}::{}` expects named fields `{{ … }}`",
                        type_name_s, variant_s
                    ),
                ));
            }
        }
        Ok(())
    }

    fn field_get(&self, obj: &Value, field: &str, line: u32) -> Result<Value, BopError> {
        match obj {
            Value::Struct(s) => s.field(field).cloned().ok_or_else(|| {
                let msg = bop::error_messages::struct_has_no_field(
                    s.type_name(),
                    field,
                );
                let names = s.fields().iter().map(|(k, _)| k.as_str());
                match bop::suggest::did_you_mean(field, names) {
                    Some(hint) => error_with_hint(line, msg, hint),
                    None => error(line, msg),
                }
            }),
            Value::EnumVariant(e) => e.field(field).cloned().ok_or_else(|| {
                error(
                    line,
                    bop::error_messages::variant_has_no_field(
                        e.type_name(),
                        e.variant(),
                        field,
                    ),
                )
            }),
            Value::Module(m) => {
                if let Some(v) = self.module_binding(m, field) {
                    return Ok(v);
                }
                if m.has_type(field) {
                    return Err(error(
                        line,
                        format!("`{}` in `{}` is a type, not a value", field, m.path),
                    ));
                }
                Err(error(
                    line,
                    format!("`{}` isn't exported from `{}`", field, m.path),
                ))
            }
            other => Err(error(
                line,
                bop::error_messages::cant_read_field(field, other.type_name()),
            )),
        }
    }

    fn field_set(
        &self,
        mut obj: Value,
        field: &str,
        value: Value,
        line: u32,
    ) -> Result<Value, BopError> {
        // Mutate the owned value and return it to the generic opcode caller.
        // Named-field assignment uses `FieldSetInPlace` instead so its
        // receiver stays off the stack and cannot spuriously trigger a detach.
        match &mut obj {
            Value::Struct(boxed) => {
                let type_name = boxed.type_name().to_string();
                if !boxed.try_set_field(field, value, line)? {
                    return Err(error(
                        line,
                        bop::error_messages::struct_has_no_field(&type_name, field),
                    ));
                }
                Ok(obj)
            }
            other => Err(error(
                line,
                bop::error_messages::cant_assign_field(field, other.type_name()),
            )),
        }
    }

    /// Handle a `MatchFail` instruction: pop the scrutinee and
    /// attempt to match it against the pattern at `pattern`. On
    /// success, install the captured bindings into the current
    /// scope and fall through. On failure, jump to `on_fail`.
    ///
    /// Delegates to `bop::pattern_matches` so the VM behaves
    /// exactly like the tree-walker on every pattern shape.
    fn match_fail(
        &mut self,
        pattern: PatternIdx,
        on_fail: CodeOffset,
        line: u32,
    ) -> Result<(), BopError> {
        let value = self.pop_value(line)?;
        // `pattern` refers to a slot in the *currently executing*
        // chunk's pattern pool; clone the shared handle rather than hold a
        // frame borrow while mutating `self` to install bindings.
        let recipe = self.current_chunk().pattern(pattern).clone();
        let mut bindings: Vec<(String, Value)> = Vec::new();
        // Build a resolver snapshot from the current frame's
        // value scopes plus the VM-level type_bindings and
        // alias map. Patterns referring to `Color::Red` or
        // `m.Color::Red` thread through this so the matcher can
        // compare the value's full identity.
        let frame = self.frames.last().expect("frame present");
        let frame_scopes = &frame.scopes;
        let type_bindings = &self.type_bindings;
        let module_aliases = &self.module_aliases;
        let resolver = |ns: Option<&str>, tn: &str| -> Option<String> {
            if let Some(ns) = ns {
                if let Some(slot) = recipe
                    .namespaces
                    .iter()
                    .find(|(name, _)| name == ns)
                    .map(|(_, namespace)| namespace)
                    .and_then(|namespace| namespace.slot_idx())
                {
                    return match frame.slots.get(slot.0 as usize) {
                        Some(Value::Module(module)) => {
                            module.type_origin(tn).map(str::to_string)
                        }
                        _ => None,
                    };
                }
            }
            resolve_type_in_frame(
                frame,
                frame_scopes,
                type_bindings,
                module_aliases,
                ns,
                tn,
            )
        };
        let matched = bop::pattern_matches(&recipe.pattern, &value, &mut bindings, &resolver);
        if matched {
            for (name, v) in bindings {
                self.define_local(name, v);
            }
        } else {
            self.jump(on_fail);
        }
        Ok(())
    }

    /// Implement `try`: pop the top value and inspect the
    /// `Ok` / `Err` shape.
    ///
    /// - `Ok(v)` (single tuple payload) / `Ok` (unit) → push the
    ///   unwrapped value (`v` or `Value::None`) and continue.
    /// - `Err(...)` → act like `Return` from the current frame,
    ///   carrying the whole `Err` variant as the returned value.
    ///   If the current frame is the top-level program, raise a
    ///   runtime error instead (there's no fn to return from).
    /// - Anything else → runtime error.
    ///
    /// Mirrors the walker's `eval_try` so all three engines agree
    /// on the same shape recognition rules.
    fn try_unwrap(&mut self, line: u32) -> Result<Next, BopError> {
        let value = self.pop_value(line)?;
        match &value {
            Value::EnumVariant(ev) if ev.variant() == "Ok" => {
                use bop::value::EnumPayload;
                let payload = match ev.payload() {
                    EnumPayload::Tuple(items) if items.len() == 1 => items[0].clone(),
                    EnumPayload::Unit => Value::None,
                    EnumPayload::Tuple(items) => {
                        return Err(error(
                            line,
                            format!(
                                "try: Ok variant must carry exactly one value, got {}",
                                items.len()
                            ),
                        ));
                    }
                    EnumPayload::Struct(_) => {
                        return Err(error(
                            line,
                            "try: Ok variant must carry a single positional value, not named fields",
                        ));
                    }
                };
                self.push_value(payload);
                Ok(Next::Continue)
            }
            Value::EnumVariant(ev) if ev.variant() == "Err" => {
                let current_is_fn = self
                    .frames
                    .last()
                    .map(|f| f.is_function)
                    .unwrap_or(false);
                if !current_is_fn {
                    return Err(bop::error_messages::top_level_try_error(line));
                }
                // Fast-return with the Err value: identical path
                // to an ordinary `return err`.
                self.do_return(value)
            }
            other => Err(error(
                line,
                format!(
                    "try expected a Result-shaped value (Ok/Err variant), got {}",
                    other.type_name()
                ),
            )),
        }
    }

    fn define_fn(&mut self, idx: FnIdx) {
        let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
        let fn_def = chunk.function(idx);
        let name = fn_def.name.clone();
        let entry = Rc::new(FnEntry {
            params: fn_def.params.clone(),
            chunk: Rc::clone(&fn_def.chunk),
            module_path: self.active_module_path().to_string(),
        });
        self.functions.insert(name.clone(), Rc::clone(&entry));
        if self.is_module_top_scope()
            && self.current_module == bop::value::ROOT_MODULE_PATH
        {
            if let Some(visibility) = self.root_function_visibility.get(&idx.0).copied() {
                self.abi_declarations
                    .retain(|(existing, _)| existing != &name);
                if visibility == Visibility::Public {
                    self.abi_declarations.push((name, entry));
                }
            }
        }
    }

    /// Materialise a lambda expression as a `Value::Fn`. Each
    /// `CaptureSource` in the lambda's `FnDef` tells us exactly
    /// where to read the captured value from the enclosing frame
    /// — no "flatten every binding in sight" pass, and no
    /// over-capture of out-of-scope slots.
    fn make_lambda(&mut self, idx: FnIdx, line: u32) -> Result<(), BopError> {
        let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
        let fn_def = chunk.function(idx);
        let captures = self.snapshot_captures_for(fn_def);
        let compiled_chunk = Rc::clone(&fn_def.chunk);
        let body: Rc<dyn core::any::Any + 'static> = compiled_chunk;
        let value = BopFn::try_new_compiled_in_module_with_origin(
            fn_def.params.clone(),
            captures,
            body,
            None,
            Some(self.active_module_path().to_string()),
            self.function_origin.clone(),
            0,
            line,
        ).map(Value::Fn)?;
        self.push_value(value);
        Ok(())
    }

    /// Package the captures for a lambda according to its
    /// compile-time `capture_sources`. `ParentSlot(n)` reads slot
    /// `n` from the enclosing frame; `ParentScope(name)` walks
    /// the enclosing frame's BTreeMap scope stack to find the
    /// binding.
    ///
    /// A missing `ParentScope` binding is skipped rather than
    /// represented by `Value::None`. Absence and a binding whose
    /// value is genuinely `none` are observably different: leaving
    /// an absent name uncaptured lets the lambda body's normal
    /// `LoadVar` / call path resolve a named function or report
    /// `Variable not found`.
    fn snapshot_captures_for(&self, fn_def: &FnDef) -> Vec<(String, Value)> {
        let frame = self.frames.last().expect("frame present");
        let mut out = Vec::with_capacity(fn_def.capture_names.len());
        for (name, source) in fn_def
            .capture_names
            .iter()
            .zip(fn_def.capture_sources.iter())
        {
            match source {
                CaptureSource::ParentSlot(slot) => {
                    let v = frame
                        .slots
                        .get(slot.0 as usize)
                        .cloned()
                        .unwrap_or(Value::None);
                    out.push((name.clone(), v));
                }
                CaptureSource::ParentScope(look_name) => {
                    let mut found = None;
                    let defining_environment_floor =
                        usize::from(frame.defining_environment_module.is_some());
                    for scope in frame
                        .scopes
                        .iter()
                        .skip(defining_environment_floor)
                        .rev()
                    {
                        if let Some(v) = scope.get(look_name.as_str()) {
                            found = Some(v.clone());
                            break;
                        }
                    }
                    if let Some(v) = found {
                        out.push((name.clone(), v));
                    }
                }
            };
        }
        out
    }

    /// Call a `Value::Fn` by pushing a new frame whose scope holds
    /// the closure's captures plus its parameters (plus the
    /// closure itself under `self_name`, when present, so
    /// self-reference works without a separate pathway).
    fn call_closure(
        &mut self,
        func: &Rc<BopFn>,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Next, BopError> {
        if !func.__is_allowed_by(&self.function_origin, "vm") {
            return Err(error(
                line,
                "This function belongs to a different Bop engine instance",
            ));
        }
        // The body must be a VM-compiled chunk. Walker-created
        // `Value::Fn`s would carry `FnBody::Ast` and don't belong
        // in the VM.
        let chunk: Rc<Chunk> = match &func.body {
            FnBody::Compiled(any) => match Rc::clone(any).downcast::<Chunk>() {
                Ok(c) => c,
                Err(_) => {
                    return Err(error(
                        line,
                        "Closure body wasn't compiled by the bytecode VM",
                    ));
                }
            },
            FnBody::Ast(_) => {
                return Err(error(
                    line,
                    "Closure body wasn't compiled for the VM — use `bop::run` to execute tree-walker closures",
                ));
            }
        };

        if args.len() != func.params.len() {
            let display_name = func.self_name.as_deref().unwrap_or("fn");
            return Err(error(
                line,
                format!(
                    "`{}` expects {} argument{}, but got {}",
                    display_name,
                    func.params.len(),
                    if func.params.len() == 1 { "" } else { "s" },
                    args.len()
                ),
            ));
        }

        if self.frames.len().saturating_sub(1) >= MAX_CALL_DEPTH {
            return Err(error_with_hint(
                line,
                "Too many nested function calls (possible infinite recursion)",
                "Check that your recursive function has a base case that stops calling itself.",
            ));
        }

        // Lambda params + locals go into the flat `slots` array
        // (same fast path as named fns). Captures and the
        // `self_name` self-reference stay on the BTreeMap scope
        // stack — they're looked up by name at the compile-time
        // fallback path (`LoadVar`) because the compiler can't
        // resolve them to slots from inside the lambda body.
        let slot_count = (chunk.slot_count as usize).max(func.params.len());
        let mut slots = self.take_slots(slot_count);
        for (i, arg) in args.into_iter().enumerate() {
            slots[i] = arg;
        }

        let mut scope = BTreeMap::new();
        for (name, value) in &func.captures {
            scope.insert(name.clone(), value.clone());
        }
        if let Some(self_name) = &func.self_name {
            scope.insert(self_name.clone(), Value::Fn(Rc::clone(func)));
        }

        self.push_function_frame(
            chunk,
            slots,
            vec![scope],
            self.stack.len(),
            func.module_path.clone(),
            FrameWrap::None,
        );
        Ok(Next::Continue)
    }

    /// Execute a `use` statement. Dispatches the four shapes
    /// (glob / selective / aliased / selective + aliased) against
    /// the module's exported bindings. Types register globally
    /// on every form; the distinction is in how the caller sees
    /// the module's *values and fns* (flat in their scope vs.
    /// behind a namespace binding).
    fn exec_use(&mut self, spec: &crate::chunk::UseSpec, line: u32) -> Result<(), BopError> {
        let refresh_root_context = self.is_module_top_scope();
        let path = spec.path.as_str();
        let items = spec.items.as_deref();
        let alias = spec.alias.as_deref();

        let is_plain_glob = items.is_none() && alias.is_none();
        if is_plain_glob
            && self.imported_here.last().is_some_and(|imports| imports.contains(path))
        {
            return Ok(());
        }
        // A lazily evaluated dependency may import the module that is
        // currently executing. Move that active environment into the shared
        // registry for the duration of loading so the nested VM observes and
        // updates the one authoritative handle rather than a cached snapshot.
        self.park_active_defining_environment();
        let loaded = self.load_module(path, line);
        self.restore_active_defining_environment();
        let artifacts = loaded?;

        // Types always register under their *full identity*
        // `(module_path, type_name)`. Two modules declaring the
        // same name now coexist at distinct keys; same-key
        // reinsertion is idempotent on matching shape and a
        // hard error otherwise (means the module was re-loaded
        // with different source).
        for (key, fields) in &artifacts.struct_defs {
            if let Some(existing) = self.struct_defs.get(key) {
                if existing == fields {
                    continue;
                }
                return Err(error(
                    line,
                    format!(
                        "Type `{}` from `{}` reloaded with different fields",
                        key.1, key.0
                    ),
                ));
            }
            self.struct_defs.insert(key.clone(), fields.clone());
        }
        for (key, variants) in &artifacts.enum_defs {
            if let Some(existing) = self.enum_defs.get(key) {
                if shapes_match(existing, variants) {
                    continue;
                }
                return Err(error(
                    line,
                    format!(
                        "Type `{}` from `{}` reloaded with different variants",
                        key.1, key.0
                    ),
                ));
            }
            self.enum_defs.insert(key.clone(), variants.clone());
        }
        for (type_key, method_name, entry) in &artifacts.methods {
            self.user_methods
                .entry(type_key.clone())
                .or_default()
                .insert(method_name.clone(), entry.clone());
        }

        // Gather the module's value-level exports as a single
        // name-keyed list. Fn declarations produce both a
        // scope-visible `Value::Fn` and a `self.functions` entry;
        // plain let bindings contribute just the value.
        let mut exports: Vec<(String, Value)> =
            Vec::with_capacity(artifacts.fn_decls.len() + artifacts.bindings.len());
        let mut fn_entries: BTreeMap<String, Rc<FnEntry>> = BTreeMap::new();
        for (name, entry) in &artifacts.fn_decls {
            if artifacts.bindings.iter().any(|(binding, _)| binding == name) {
                continue;
            }
            let chunk_rc: Rc<Chunk> = entry.chunk.clone();
            let body: Rc<dyn core::any::Any + 'static> = chunk_rc;
            let value = BopFn::try_new_compiled_in_module_with_origin(
                entry.params.clone(),
                Vec::new(),
                body,
                Some(name.clone()),
                Some(entry.module_path.clone()),
                self.function_origin.clone(),
                0,
                line,
            ).map(Value::Fn)?;
            exports.push((name.clone(), value));
            fn_entries.insert(name.clone(), entry.clone());
        }
        for (name, value) in &artifacts.bindings {
            exports.push((name.clone(), value.clone()));
        }
        // Functions and values are stored separately inside module artifacts,
        // but a flat import exposes one namespace. Sorting the combined
        // projection makes warning order match the walker and generated AOT.
        exports.sort_by(|(left, _), (right, _)| left.cmp(right));

        // Selective filter: ensure every listed name exists, then
        // retain only the listed exports.
        if let Some(list) = items {
            let available: BTreeSet<&str> =
                exports.iter().map(|(k, _)| k.as_str()).collect();
            for wanted in list {
                if !available.contains(wanted.as_str())
                    && !artifacts.type_exports.contains_key(wanted)
                {
                    return Err(error(
                        line,
                        format!(
                            "`{}` isn't exported from `{}` (selective import)",
                            wanted, path
                        ),
                    ));
                }
            }
            let listed: BTreeSet<String> =
                list.iter().cloned().collect();
            exports.retain(|(k, _)| listed.contains(k));
            fn_entries.retain(|name, _| listed.contains(name));
        }

        // Decide which of the module's declared type names the
        // caller sees by bare name. Selective = exactly the
        // listed items that are types; glob = all public
        // (non-`_`-prefixed) types; aliased = none at bare name
        // (only reachable through the alias).
        let exposed_types: BTreeMap<String, String> = match items {
            Some(list) => artifacts
                .type_exports
                .iter()
                .filter(|(name, _)| list.iter().any(|item| item == *name))
                .map(|(name, origin)| (name.clone(), origin.clone()))
                .collect(),
            None => artifacts
                .type_exports
                .iter()
                .filter(|(name, _)| alias.is_some() || !bop::naming::is_private(name))
                .map(|(name, origin)| (name.clone(), origin.clone()))
                .collect(),
        };

        if let Some(alias_name) = alias {
            // Aliased form: pack the exports into a Value::Module
            // and bind it under the alias. The alias lives in the
            // current frame's top scope. Function values retain
            // their defining module, so sibling calls resolve
            // through cached module artifacts without publishing
            // bare names in this caller.
            let frame_has = self
                .frames
                .last()
                .and_then(|f| f.scopes.last())
                .map(|s| s.contains_key(alias_name))
                .unwrap_or(false);
            let imported_function_has = self
                .imported_functions
                .last()
                .is_some_and(|functions| functions.contains_key(alias_name));
            if frame_has || self.functions.contains_key(alias_name) || imported_function_has {
                return Err(error(
                    line,
                    format!(
                        "`{}` is already bound — can't use it as a module alias",
                        alias_name
                    ),
                ));
            }
            let live_bindings = exports
                .iter()
                .map(|(name, _)| {
                    let origin = artifacts
                        .binding_origins
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| (path.to_string(), name.clone()));
                    (name.clone(), origin)
                })
                .collect();
            let module_rc = bop::value::BopModule::__try_new_live_with_type_exports(
                path.to_string(),
                exports,
                bop::value::BopTypeExports::from_origins(exposed_types),
                live_bindings,
                Rc::clone(&self.live_value_environments),
                line,
            )?;
            // Bind the alias three ways:
            //   1. as Value::Module in the current value scope
            //      (immediate `m.helper(x)` at the use site);
            //   2. in `module_aliases` so it survives the
            //      fresh-scope reset at fn call boundaries;
            //   3. in `type_bindings` so patterns + construction
            //      resolve `m.Type` inside fn bodies.
            let module_value = Value::Module(Rc::clone(&module_rc));
            if let Some(frame) = self.frames.last_mut() {
                if let Some(scope) = frame.scopes.last_mut() {
                    scope.insert(alias_name.to_string(), module_value);
                }
            }
            self.module_aliases
                .last_mut()
                .expect("module alias scope")
                .insert(alias_name.to_string(), Rc::clone(&module_rc));
            if let Some(scope) = self.type_bindings.last_mut() {
                scope.insert(alias_name.to_string(), path.to_string());
            }
        } else {
            // Flat form (glob / selective). Glob skips
            // `_`-prefixed names (privacy); selective doesn't
            // (the user explicitly asked).
            let skip_private = items.is_none();
            let module_top_scope = self.is_module_top_scope();
            for (name, mut value) in exports {
                if skip_private && bop::naming::is_private(&name) {
                    continue;
                }
                let clashes = self
                    .frames
                    .last()
                    .and_then(|f| f.scopes.last())
                    .map(|s| s.contains_key(&name))
                    .unwrap_or(false)
                    || (module_top_scope && self.functions.contains_key(&name))
                    // Slot-allocated locals / params visible at the
                    // `use` site never appear in the dynamic scope
                    // maps above; the compiler records them in the
                    // spec (sorted) so this check sees them too.
                    || spec.shadowed_locals.binary_search(&name).is_ok();
                if clashes {
                    if is_plain_glob {
                        self.runtime_warnings.push(BopWarning::at(
                            bop::error_messages::glob_shadow_warning(&name, path),
                            line,
                        ));
                    }
                    continue;
                }
                if let Some(entry) = fn_entries.remove(&name) {
                    self.imported_functions
                        .last_mut()
                        .expect("imported function scope")
                        .insert(name.clone(), entry);
                }
                let module = artifacts.module_exports.get(&name).cloned();
                let origin = artifacts
                    .binding_origins
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| (path.to_string(), name.clone()));
                if module_top_scope {
                    self.binding_origins.insert(name.clone(), origin.clone());
                    if origin.0 != self.current_module || origin.1 != name {
                        if let Some(authoritative) = self
                            .live_value_environments
                            .borrow_mut()
                            .get_mut(&origin.0)
                            .and_then(|environment| environment.remove(&origin.1))
                        {
                            value = authoritative;
                        }
                    }
                } else if let Some(current) = self.live_origin_value(&origin) {
                    value = current;
                }
                if let Some(frame) = self.frames.last_mut() {
                    if let Some(scope) = frame.scopes.last_mut() {
                        scope.insert(name.clone(), value);
                    }
                }
                if let Some(module) = module {
                    self.module_aliases
                        .last_mut()
                        .expect("module alias scope")
                        .insert(name, module);
                }
            }
            // Bring the module's exposed types in by bare name
            // too — `Color::Red` now resolves to *this*
            // module's Color. First-win on conflict matches the
            // value-binding rule.
            for (type_name, origin) in &exposed_types {
                let already_bound = self
                    .type_bindings
                    .last()
                    .map(|s| s.contains_key(type_name))
                    .unwrap_or(false);
                if already_bound {
                    continue;
                }
                if let Some(scope) = self.type_bindings.last_mut() {
                    scope.insert(type_name.clone(), origin.clone());
                }
                if module_top_scope {
                    self.type_exports
                        .entry(type_name.clone())
                        .or_insert_with(|| origin.clone());
                }
            }
        }

        if is_plain_glob {
            self.imported_here
                .last_mut()
                .expect("import scope")
                .insert(path.to_string());
        }
        if refresh_root_context {
            self.refresh_root_lexical_context();
        }
        Ok(())
    }

    /// Validate `ns.Type` — confirm `ns` binds a `Value::Module`
    /// whose type exports include `type_name`. Used by
    /// namespaced struct-literal / variant-ctor dispatch.
    fn resolve_namespaced_type(
        &self,
        namespace: crate::chunk::NamespaceRef,
        type_name: &str,
        line: u32,
    ) -> Result<String, BopError> {
        let name = self.current_chunk().name(namespace.name_idx());
        let Some(slot) = namespace.slot_idx() else {
            self.validate_namespaced_type(name, type_name, line)?;
            return self.resolve_type_ref(Some(name), type_name).ok_or_else(|| {
                error(line, format!("`{type_name}` isn't a type exported from `{name}`"))
            });
        };
        let value = self
            .frames
            .last()
            .and_then(|frame| frame.slots.get(slot.0 as usize))
            .ok_or_else(|| error(line, "VM: local slot out of range"))?;
        let module = match value {
            Value::Module(module) => module,
            other => {
                return Err(error(
                    line,
                    format!(
                        "`{name}` is a {}, not a module alias — can't reach `{type_name}` through it",
                        other.type_name()
                    ),
                ));
            }
        };
        module.type_origin(type_name).map(str::to_string).ok_or_else(|| {
            error(
                line,
                format!("`{type_name}` isn't a type exported from `{}`", module.path),
            )
        })
    }

    fn validate_namespaced_type(
        &self,
        ns: &str,
        type_name: &str,
        line: u32,
    ) -> Result<(), BopError> {
        // Prefer a local value-scope binding (catches
        // shadowing), but fall back to the VM-level module
        // alias map so namespaced references inside fn bodies
        // still resolve — the fn frame's value scopes don't
        // carry the caller's aliases.
        let frame = self.frames.last().ok_or_else(|| {
            error(line, "VM: no frame for namespaced type access")
        })?;
        for scope in frame.scopes.iter().rev() {
            if let Some(v) = scope.get(ns) {
                let module = match v {
                    Value::Module(m) => m,
                    other => {
                        return Err(error(
                            line,
                            format!(
                                "`{}` is a {}, not a module alias — can't reach `{}` through it",
                                ns,
                                other.type_name(),
                                type_name
                            ),
                        ));
                    }
                };
                if !module.has_type(type_name) {
                    return Err(error(
                        line,
                        format!(
                            "`{}` isn't a type exported from `{}`",
                            type_name, module.path
                        ),
                    ));
                }
                return Ok(());
            }
        }
        if let Some(module) = self.module_alias(ns) {
            if !module.has_type(type_name) {
                return Err(error(
                    line,
                    format!(
                        "`{}` isn't a type exported from `{}`",
                        type_name, module.path
                    ),
                ));
            }
            return Ok(());
        }
        Err(error(
            line,
            format!("`{}` isn't a module alias in scope", ns),
        ))
    }

    /// Resolve a source-level type reference to its declaring
    /// module. Shared scope walker between construction,
    /// pattern matching, and namespace validation: keeps
    /// resolution rules centralised.
    fn resolve_type_ref(&self, namespace: Option<&str>, type_name: &str) -> Option<String> {
        let frame = self.frames.last()?;
        resolve_type_in_frame(
            frame,
            &frame.scopes,
            &self.type_bindings,
            &self.module_aliases,
            namespace,
            type_name,
        )
    }

    fn load_module(
        &mut self,
        path: &str,
        line: u32,
    ) -> Result<ModuleArtifacts, BopError> {
        {
            let cache = self.imports.borrow();
            if let Some(ImportSlot::Loaded(bindings)) = cache.get(path) {
                return Ok(bindings.clone());
            }
            if let Some(ImportSlot::Loading) = cache.get(path) {
                return Err(error(
                    line,
                    format!("Circular import: module `{}` is still loading", path),
                ));
            }
        }

        let resolved = {
            let _suspended = bop::memory::ActiveMemoryGuard::__suspend();
            self.host.resolve_module(path)
        };
        let source = match resolved {
            Some(Ok(s)) => s,
            Some(Err(e)) => return Err(e),
            None => {
                return Err(error(
                    line,
                    format!("Module `{}` not found", path),
                ));
            }
        };

        self.imports
            .borrow_mut()
            .insert(path.to_string(), ImportSlot::Loading);

        let result = self
            .evaluate_module(path, &source)
            .map_err(|error| error.with_module_source(path, source.as_str()));

        match result {
            Ok(bindings) => {
                self.imports
                    .borrow_mut()
                    .insert(path.to_string(), ImportSlot::Loaded(bindings.clone()));
                Ok(bindings)
            }
            Err(e) => {
                self.imports.borrow_mut().remove(path);
                Err(e)
            }
        }
    }

    /// Parse, compile, and execute a module in a nested VM that
    /// shares the import cache and limits. Returns the module's
    /// top-level `(name, Value)` bindings, with named fns reified
    /// as `Value::Fn` carrying VM-compiled chunks so the caller's
    /// `Call` / `CallValue` paths can dispatch them directly.
    fn evaluate_module(
        &mut self,
        module_path: &str,
        source: &str,
    ) -> Result<ModuleArtifacts, BopError> {
        let stmts = bop::parse(source)?;
        let chunk = crate::compile(&stmts)?;
        let module_runtime = ModuleRuntime {
            imports: Rc::clone(&self.imports),
            environments: Rc::clone(&self.live_value_environments),
            origins: Rc::clone(&self.live_binding_origins),
        };
        let limits = self.limits.clone();
        let mut sub = Vm::new_internal(
            chunk,
            self.host,
            limits,
            module_runtime,
            module_path.to_string(),
            BTreeMap::new(),
            self.function_origin.clone(),
        );
        let module_result = sub.run_internal();
        // Nested module VMs are an implementation detail of the root
        // operation. Preserve their diagnostics (including transitive ones)
        // and defer stderr delivery to that operation's public boundary.
        self.runtime_warnings.append(&mut sub.runtime_warnings);
        if let Err(module_error) = module_result {
            sub.restore_instance_baseline();
            let failed_environment = sub
                .frames
                .first_mut()
                .and_then(|frame| frame.scopes.first_mut())
                .map(core::mem::take)
                .unwrap_or_default();
            put_live_environment(
                &sub.live_value_environments,
                module_path,
                failed_environment,
                &sub.binding_origins,
            );
            // Forwarded bindings have been returned to their declaration
            // modules; the failed facade's partial state must not survive.
            sub.live_value_environments.borrow_mut().remove(module_path);
            sub.live_binding_origins.borrow_mut().remove(module_path);
            return Err(module_error);
        }
        // Collect top-level lets from the module frame's one
        // remaining scope…
        let bindings = match sub
            .frames
            .first()
            .and_then(|frame| frame.scopes.first())
            .map(|scope| {
                snapshot_module_bindings(
                    scope,
                    &sub.binding_origins,
                    module_path,
                    &sub.imports,
                    0,
                )
            })
            .transpose()
        {
            Ok(bindings) => bindings.unwrap_or_default(),
            Err(snapshot_error) => {
                let failed_environment = sub
                    .frames
                    .first_mut()
                    .and_then(|frame| frame.scopes.first_mut())
                    .map(core::mem::take)
                    .unwrap_or_default();
                put_live_environment(
                    &sub.live_value_environments,
                    module_path,
                    failed_environment,
                    &sub.binding_origins,
                );
                sub.live_value_environments.borrow_mut().remove(module_path);
                sub.live_binding_origins.borrow_mut().remove(module_path);
                return Err(snapshot_error);
            }
        };
        let live_environment = sub
            .frames
            .first_mut()
            .and_then(|frame| frame.scopes.first_mut())
            .map(core::mem::take)
            .unwrap_or_default();
        let binding_origins = sub.binding_origins.clone();
        put_live_environment(
            &sub.live_value_environments,
            module_path,
            live_environment,
            &binding_origins,
        );
        sub.live_binding_origins
            .borrow_mut()
            .insert(module_path.to_string(), binding_origins.clone());
        // Named fn entries go out separately so the importer
        // can register them in `self.functions` for bare-ident
        // call resolution. Reified `Value::Fn`s for the same
        // fns are synthesised at the import site (see
        // `exec_import`).
        let fn_decls: Vec<(String, Rc<FnEntry>)> =
            sub.functions.into_iter().collect();
        // Type decls & methods come along for the ride — the
        // importer needs them so pattern-matching and
        // construction against the module's types works
        // without re-declaration. Engine builtins (<builtin>
        // module path) are already seeded in the importer, so
        // we filter them out to avoid duplicating the entries.
        let builtin_mp = bop::value::BUILTIN_MODULE_PATH;
        let struct_defs: Vec<ModuleStructDef> = sub
            .struct_defs
            .into_iter()
            .filter(|((mp, _), _)| mp != builtin_mp)
            .collect();
        let enum_defs: Vec<ModuleEnumDef> = sub
            .enum_defs
            .into_iter()
            .filter(|((mp, _), _)| mp != builtin_mp)
            .collect();
        let mut methods: Vec<ModuleMethodDef> = Vec::new();
        for (type_key, by_method) in sub.user_methods {
            for (method_name, entry) in by_method {
                methods.push((type_key.clone(), method_name, entry));
            }
        }
        let module_exports: BTreeMap<String, Rc<bop::value::BopModule>> = bindings
            .iter()
            .filter_map(|(name, value)| match value {
                Value::Module(module) => Some((name.clone(), Rc::clone(module))),
                _ => None,
            })
            .collect();
        let lexical_context = Rc::new(ModuleLexicalContext {
            type_bindings: sub.type_bindings.into_iter().next().unwrap_or_default(),
            module_aliases: module_exports.clone(),
            imported_functions: sub.imported_functions.into_iter().next().unwrap_or_default(),
        });
        Ok(ModuleArtifacts {
            bindings,
            binding_origins,
            fn_decls,
            struct_defs,
            enum_defs,
            methods,
            type_exports: sub.type_exports,
            module_exports,
            lexical_context,
        })
    }

    /// Like `run` but keeps `self` around afterwards so the
    /// caller can inspect the module's final state. Used by
    /// `evaluate_module`.
    fn run_internal(&mut self) -> Result<(), BopError> {
        while let Some((instr, line)) = self.fetch() {
            if let Err(err) = self.tick(line) {
                self.unwind_to_try_call(err)?;
                continue;
            }
            match self.dispatch(instr, line) {
                Ok(Next::Continue) => {}
                Ok(Next::Halt) => break,
                Err(err) => {
                    self.unwind_to_try_call(err)?;
                }
            }
        }
        Ok(())
    }

    fn do_return(&mut self, value: Value) -> Result<Next, BopError> {
        if self.frames.last().is_some_and(|frame| !frame.is_function) {
            drop(value);
            let frame = self.frames.last_mut().expect("frame present");
            frame.ip = frame.chunk.code.len();
            frame.scopes.truncate(frame.scope_base);
            self.stack.clear();
            self.type_bindings.truncate(frame.type_scope_base);
            self.module_aliases.truncate(frame.alias_scope_base);
            self.imported_functions.truncate(frame.alias_scope_base);
            self.imported_here.truncate(frame.alias_scope_base);
            return Ok(Next::Halt);
        }
        // Pop the current frame, truncate any frame-local stack
        // residue, and push the return value for the caller.
        let mut frame = self.frames.pop().expect("frame present");
        self.store_frame_defining_environment(&mut frame);
        self.restore_active_defining_environment();
        self.stack.truncate(frame.stack_base);
        // Drop the function's protected type-binding scope plus any runtime
        // scopes skipped by an early return. Top-level return preserves the
        // builtin map while discarding any open runtime scopes.
        self.type_bindings
            .truncate(frame.caller_type_scope_depth());
        self.module_aliases.truncate(frame.alias_scope_base);
        self.imported_functions.truncate(frame.alias_scope_base);
        self.imported_here.truncate(frame.alias_scope_base);
        // Recycle the slot vec so the next call can reuse its
        // allocation. Dropping it here would drop every `Value`
        // slot in place, which is still cheap for `Int`/`Bool`
        // but releases the vec's backing buffer — the exact
        // alloc/dealloc churn we're here to avoid.
        if !frame.slots.is_empty() {
            self.return_slots(frame.slots);
        }
        // Iterator-protocol landing pads get their own return
        // handling — they don't just push the value; they
        // manipulate the caller's stack (and maybe jump) to
        // continue the for-loop.
        match frame.wrap {
            FrameWrap::IterStart => {
                // User `.iter()` returned `value`. Stash it on
                // the stack as the iterator object for the
                // matching `IterNext` instruction to consume.
                self.stack.push(Slot::IterObject(value));
                return Ok(Next::Continue);
            }
            FrameWrap::IterAdvance(target) => {
                // User `.next()` returned `value`; dispatch:
                //   Iter::Next(x) → push x so the following
                //     StoreLocal/DefineLocal binds the loop var.
                //     Iterator object stays on the stack under x.
                //   Iter::Done    → pop the iterator, jump exit.
                //   malformed     → raise.
                match unwrap_iter_step(&value) {
                    IterStep::Next(x) => {
                        drop(value);
                        self.push_value(x);
                        return Ok(Next::Continue);
                    }
                    IterStep::Done => {
                        drop(value);
                        self.stack.pop();
                        self.jump(target);
                        return Ok(Next::Continue);
                    }
                    IterStep::Malformed => {
                        let msg = format!(
                            "`.next()` on a `for` iterator must return `Iter::Next(v)` or `Iter::Done`, got {}",
                            value.inspect()
                        );
                        // The returning frame no longer has a
                        // meaningful "current line" — use the
                        // top frame's instruction line as the
                        // best approximation. 0 is fine if we're
                        // returning to the top-level frame.
                        let caller_line = self
                            .frames
                            .last()
                            .and_then(|f| {
                                let idx = f.ip.saturating_sub(1);
                                f.chunk.lines.get(idx).copied()
                            })
                            .unwrap_or(0);
                        return Err(error(caller_line, msg));
                    }
                }
            }
            _ => {}
        }
        // Apply the frame-level return wrapper: `try_call` wraps
        // in `Result::Ok`, `r.map` / `r.map_err` wrap the closure
        // result in Ok / Err respectively. Plain frames pass the
        // value through untouched.
        let final_value = match frame.wrap {
            FrameWrap::None => value,
            FrameWrap::TryCall { line } => builtins::make_try_call_ok(value, line)?,
            FrameWrap::ResultOk { line } => methods::make_result_ok(value, line)?,
            FrameWrap::ResultErr { line } => methods::make_result_err(value, line)?,
            FrameWrap::IterStart | FrameWrap::IterAdvance(_) => {
                unreachable!("handled above")
            }
        };
        if frame.is_function {
            self.push_value(final_value);
            Ok(Next::Continue)
        } else {
            // Return at top level: behave like Halt (matches tree-walker,
            // which silently accepts Signal::Return at program scope).
            drop(final_value);
            Ok(Next::Halt)
        }
    }

    /// Implement the `try_call(f)` builtin. Validates the arg
    /// shape, dispatches to `call_closure` to push a frame for
    /// `f`, and marks that frame with `FrameWrap::TryCall` so
    /// the outcome (whether a normal return or a non-fatal
    /// error) gets wrapped in a `Result::Ok`/`Result::Err`
    /// before returning to the original `try_call` caller.
    fn builtin_try_call(
        &mut self,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Next, BopError> {
        if args.len() != 1 {
            return Err(error(
                line,
                format!(
                    "`try_call` expects 1 argument, but got {}",
                    args.len()
                ),
            ));
        }
        let mut iter = args.into_iter();
        let callable = iter.next().unwrap();
        let func = match &callable {
            Value::Fn(f) => Rc::clone(f),
            other => {
                return Err(error(
                    line,
                    format!(
                        "`try_call` expects a function, got {}",
                        other.type_name()
                    ),
                ));
            }
        };
        drop(callable);
        if let Err(err) = self.call_closure(&func, Vec::new(), line) {
            if err.is_fatal {
                return Err(err);
            }
            self.push_value(builtins::make_try_call_err(&err));
            return Ok(Next::Continue);
        }
        // The frame we just pushed is the one that should
        // participate in the try_call wrap/catch dance.
        if let Some(frame) = self.frames.last_mut() {
            frame.wrap = FrameWrap::TryCall { line };
        }
        Ok(Next::Continue)
    }

    /// Dispatch `receiver.method(args)` as a user-method call
    /// for the iterator protocol. Pushes a bytecode frame with
    /// `wrap` attached so the return value lands on the caller's
    /// stack with the right post-processing. Returns a clean
    /// "iterable doesn't have a .iter() method" error when the
    /// user type doesn't implement the protocol.
    fn dispatch_iter_method(
        &mut self,
        receiver: Value,
        method: &str,
        extra_args: Vec<Value>,
        wrap: FrameWrap,
        line: u32,
    ) -> Result<(), BopError> {
        let type_key: Option<(String, String)> = match &receiver {
            Value::Struct(s) => Some((
                s.module_path().to_string(),
                s.type_name().to_string(),
            )),
            Value::EnumVariant(e) => Some((
                e.module_path().to_string(),
                e.type_name().to_string(),
            )),
            _ => None,
        };
        let key = type_key.ok_or_else(|| {
            error(
                line,
                bop::error_messages::cant_iterate_over(receiver.type_name()),
            )
        })?;
        let entry = self
            .user_methods
            .get(&key)
            .and_then(|m| m.get(method))
            .cloned()
            .ok_or_else(|| {
                error(
                    line,
                    format!(
                        "`{}` doesn't have a `.{}()` method — can't iterate",
                        key.1, method
                    ),
                )
            })?;
        if entry.params.len() != extra_args.len() + 1 {
            return Err(error(
                line,
                format!(
                    "`{}.{}` expects {} argument{} (including `self`), but got {}",
                    key.1,
                    method,
                    entry.params.len(),
                    if entry.params.len() == 1 { "" } else { "s" },
                    extra_args.len() + 1
                ),
            ));
        }
        if self.frames.len().saturating_sub(1) >= MAX_CALL_DEPTH {
            return Err(error_with_hint(
                line,
                "Too many nested function calls (possible infinite recursion)",
                "Check that your recursive function has a base case that stops calling itself.",
            ));
        }
        let slot_count =
            (entry.chunk.slot_count as usize).max(entry.params.len());
        let mut slots = self.take_slots(slot_count);
        slots[0] = receiver;
        for (i, a) in extra_args.into_iter().enumerate() {
            slots[i + 1] = a;
        }
        self.push_function_frame(
            entry.chunk.clone(),
            slots,
            Vec::new(),
            self.stack.len(),
            Some(entry.module_path.clone()),
            wrap,
        );
        Ok(())
    }

    /// Handle `r.map(f)`, `r.map_err(f)`, `r.and_then(f)` for a
    /// built-in `Result`. `map` / `map_err` push a closure frame
    /// marked with a `FrameWrap` that wraps the closure's return
    /// value in `Ok` / `Err`; the short-circuit branch (map on
    /// Err, map_err on Ok) skips the call and pushes the passed-
    /// through Result directly. `and_then` trusts the closure to
    /// return a Result and pushes the call with no wrap.
    fn call_result_callable_method(
        &mut self,
        obj: Value,
        kind: methods::ResultCallableKind,
        method: &str,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Next, BopError> {
        use methods::{make_result_err, make_result_ok, ResultCallableKind};
        if args.len() != 1 {
            return Err(error(
                line,
                format!(
                    "`{}` expects 1 argument, but got {}",
                    method,
                    args.len()
                ),
            ));
        }
        let mut args_iter = args.into_iter();
        let callable = args_iter.next().unwrap();

        let (is_ok, payload) = match &obj {
            Value::EnumVariant(e) => {
                let payload = match e.payload() {
                    bop::value::EnumPayload::Tuple(items) if items.len() == 1 => {
                        items[0].clone()
                    }
                    _ => {
                        return Err(error(
                            line,
                            format!("malformed Result::{} payload", e.variant()),
                        ));
                    }
                };
                (e.variant() == "Ok", payload)
            }
            _ => return Err(error(line, "Result method called on non-Result")),
        };
        drop(obj);

        match kind {
            ResultCallableKind::Map => {
                if !is_ok {
                    // Err passes through unchanged — no closure call.
                    self.push_value(make_result_err(payload, line)?);
                    return Ok(Next::Continue);
                }
                let func = match &callable {
                    Value::Fn(f) => Rc::clone(f),
                    other => {
                        return Err(error(
                            line,
                            format!("`map` expects a function, got {}", other.type_name()),
                        ));
                    }
                };
                drop(callable);
                self.call_closure(&func, vec![payload], line)?;
                if let Some(frame) = self.frames.last_mut() {
                    frame.wrap = FrameWrap::ResultOk { line };
                }
                Ok(Next::Continue)
            }
            ResultCallableKind::MapErr => {
                if is_ok {
                    self.push_value(make_result_ok(payload, line)?);
                    return Ok(Next::Continue);
                }
                let func = match &callable {
                    Value::Fn(f) => Rc::clone(f),
                    other => {
                        return Err(error(
                            line,
                            format!(
                                "`map_err` expects a function, got {}",
                                other.type_name()
                            ),
                        ));
                    }
                };
                drop(callable);
                self.call_closure(&func, vec![payload], line)?;
                if let Some(frame) = self.frames.last_mut() {
                    frame.wrap = FrameWrap::ResultErr { line };
                }
                Ok(Next::Continue)
            }
            ResultCallableKind::AndThen => {
                if !is_ok {
                    self.push_value(make_result_err(payload, line)?);
                    return Ok(Next::Continue);
                }
                let func = match &callable {
                    Value::Fn(f) => Rc::clone(f),
                    other => {
                        return Err(error(
                            line,
                            format!(
                                "`and_then` expects a function, got {}",
                                other.type_name()
                            ),
                        ));
                    }
                };
                drop(callable);
                // Closure is expected to return a Result — no
                // wrapping. The result flows back via the normal
                // `do_return` path (`FrameWrap::None`).
                self.call_closure(&func, vec![payload], line)
            }
        }
    }

    /// Propagate a non-fatal error up through any number of fn
    /// frames until we find a `FrameWrap::TryCall` landing pad.
    /// On success, truncates the frame stack and value stack
    /// back to the wrapper's base, pushes a
    /// `Result::Err(RuntimeError { … })` for the outer caller,
    /// and returns `Ok(())` so the dispatch loop keeps going.
    ///
    /// Returns `Err(err)` (untouched) when:
    /// - the error is fatal (resource-limit violation), or
    /// - no enclosing `try_call` frame exists.
    fn unwind_to_try_call(&mut self, err: BopError) -> Result<(), BopError> {
        if err.is_fatal {
            return Err(err);
        }
        let wrap_idx = match self
            .frames
            .iter()
            .rposition(|f| matches!(f.wrap, FrameWrap::TryCall { .. }))
        {
            Some(i) => i,
            None => return Err(err),
        };
        let wrapper_stack_base = self.frames[wrap_idx].stack_base;
        // Drain the unwound frames through the freelist instead
        // of `truncate`, so their slot vecs get recycled.
        while self.frames.len() > wrap_idx {
            let mut frame = self.frames.pop().expect("frame present");
            self.store_frame_defining_environment(&mut frame);
            self.type_bindings
                .truncate(frame.caller_type_scope_depth());
            self.module_aliases.truncate(frame.alias_scope_base);
            self.imported_functions.truncate(frame.alias_scope_base);
            self.imported_here.truncate(frame.alias_scope_base);
            if !frame.slots.is_empty() {
                self.return_slots(frame.slots);
            }
        }
        self.restore_active_defining_environment();
        self.stack.truncate(wrapper_stack_base);
        self.push_value(builtins::make_try_call_err(&err));
        Ok(())
    }
}

fn resolve_type_in_frame(
    frame: &Frame,
    value_scopes: &[BTreeMap<String, Value>],
    type_scopes: &[BTreeMap<String, String>],
    module_aliases: &[BTreeMap<String, Rc<bop::value::BopModule>>],
    namespace: Option<&str>,
    type_name: &str,
) -> Option<String> {
    if let Some(namespace) = namespace {
        for scope in value_scopes.iter().rev() {
            if let Some(value) = scope.get(namespace) {
                return match value {
                    Value::Module(module) => {
                        module.type_origin(type_name).map(str::to_string)
                    }
                    _ => None,
                };
            }
        }
        let floor = frame.is_function.then_some(frame.alias_scope_base);
        for (index, aliases) in module_aliases.iter().enumerate().rev() {
            if floor.is_some_and(|floor| index < floor) {
                continue;
            }
            if let Some(module) = aliases.get(namespace) {
                return module.type_origin(type_name).map(str::to_string);
            }
        }
        return frame
            .lexical_context
            .module_aliases
            .get(namespace)
            .and_then(|module| module.type_origin(type_name).map(str::to_string));
    }

    let floor = frame.is_function.then_some(frame.type_scope_base.saturating_sub(1));
    for (index, bindings) in type_scopes.iter().enumerate().rev() {
        if floor.is_some_and(|floor| index < floor) {
            continue;
        }
        if let Some(module_path) = bindings.get(type_name) {
            return Some(module_path.clone());
        }
    }
    frame.lexical_context.type_bindings.get(type_name).cloned()
}

fn apply_in_place_assign<F>(
    op: crate::chunk::InPlaceAssignOp,
    rhs: Value,
    line: u32,
    current: F,
) -> Result<Value, BopError>
where
    F: FnOnce() -> Result<Value, BopError>,
{
    use crate::chunk::InPlaceAssignOp;

    if op == InPlaceAssignOp::Eq {
        return Ok(rhs);
    }
    let left = current()?;
    match op {
        InPlaceAssignOp::Eq => unreachable!(),
        InPlaceAssignOp::Add => ops::add(&left, &rhs, line),
        InPlaceAssignOp::Sub => ops::sub(&left, &rhs, line),
        InPlaceAssignOp::Mul => ops::mul(&left, &rhs, line),
        InPlaceAssignOp::Div => ops::div(&left, &rhs, line),
        InPlaceAssignOp::Rem => ops::rem(&left, &rhs, line),
    }
}

impl BopInstance {
    /// Compile and execute a program once, retaining its VM state for calls.
    pub fn load(
        source: &str,
        host: &mut dyn BopHost,
        limits: &BopLimits,
    ) -> Result<Self, BopError> {
        let statements = bop::parse(source)?;
        let compiled = crate::compiler::compile_program(&statements)?;
        crate::validate_chunk(&compiled.chunk)?;
        let memory = bop::memory::MemoryAccount::__new(limits.max_memory);
        let mut vm = Vm::new_internal(
            compiled.chunk,
            host,
            limits.clone(),
            ModuleRuntime::empty(),
            bop::value::ROOT_MODULE_PATH.to_string(),
            compiled.root_function_visibility,
            BopFnOrigin::__instance("vm"),
        );
        {
            let _active = bop::memory::ActiveMemoryGuard::__activate(&memory);
            let execution = vm.run_internal();
            vm.write_runtime_warnings();
            execution?;
            if memory.__exceeded() {
                return Err(instance_memory_error());
            }
        }
        let entries = vm
            .abi_declarations
            .iter()
            .map(|(name, target)| EntryPoint::__new(name.clone(), target.params.len()))
            .collect();
        Ok(Self {
            state: Some(vm.into_state()),
            entries,
            limits: limits.clone(),
            in_operation: Cell::new(false),
            memory,
        })
    }

    pub fn entry_points(&self) -> &[EntryPoint] {
        &self.entries
    }

    pub fn call(
        &mut self,
        name: &str,
        args: &[Value],
        host: &mut dyn BopHost,
    ) -> Result<Value, BopError> {
        let _operation = VmOperationGuard::begin(&self.in_operation)?;
        let state = self.state.as_ref().expect("instance state present");
        let target = state
            .abi_declarations
            .iter()
            .find(|(entry_name, _)| entry_name == name)
            .map(|(_, target)| Rc::clone(target))
            .ok_or_else(|| error(0, format!("Public entry point `{}` was not found", name)))?;
        if args.len() != target.params.len() {
            return Err(error(
                0,
                format!(
                    "`{}` expects {} argument{}, but got {}",
                    name,
                    target.params.len(),
                    if target.params.len() == 1 { "" } else { "s" },
                    args.len(),
                ),
            ));
        }
        if self.memory.__exceeded() {
            return Err(instance_memory_error());
        }
        let state = self.state.take().expect("instance state present");
        let mut vm = Vm::from_state(state, host, self.limits.clone());
        let result = {
            let _active = bop::memory::ActiveMemoryGuard::__activate(&self.memory);
            for argument in args {
                vm.push_value(argument.clone());
            }
            let execution = vm
                .enter_user_fn(target, args.len(), 0)
                .and_then(|_| vm.run_internal());
            let value = execution.and_then(|_| vm.pop_value(0));
            vm.restore_instance_baseline();
            if self.memory.__exceeded() {
                Err(instance_memory_error())
            } else {
                value
            }
        };
        vm.write_runtime_warnings();
        self.state = Some(vm.into_state());
        result
    }

    pub fn call_value(
        &mut self,
        callable: &Value,
        args: &[Value],
        host: &mut dyn BopHost,
    ) -> Result<Value, BopError> {
        let _operation = VmOperationGuard::begin(&self.in_operation)?;
        let function = match callable {
            Value::Fn(function) => Rc::clone(function),
            other => {
                return Err(error(
                    0,
                    format!("expected function, got {}", other.type_name()),
                ));
            }
        };
        let state = self.state.as_ref().expect("instance state present");
        if !function.__is_allowed_by(&state.function_origin, "vm") {
            return Err(error(
                0,
                "This function belongs to a different Bop engine instance",
            ));
        }
        if args.len() != function.params.len() {
            return Err(error(
                0,
                format!(
                    "Function expects {} argument{}, but got {}",
                    function.params.len(),
                    if function.params.len() == 1 { "" } else { "s" },
                    args.len(),
                ),
            ));
        }
        if self.memory.__exceeded() {
            return Err(instance_memory_error());
        }
        let state = self.state.take().expect("instance state present");
        let mut vm = Vm::from_state(state, host, self.limits.clone());
        let result = {
            let _active = bop::memory::ActiveMemoryGuard::__activate(&self.memory);
            let execution = vm
                .call_closure(&function, args.to_vec(), 0)
                .and_then(|_| vm.run_internal());
            let value = execution.and_then(|_| vm.pop_value(0));
            vm.restore_instance_baseline();
            if self.memory.__exceeded() {
                Err(instance_memory_error())
            } else {
                value
            }
        };
        vm.write_runtime_warnings();
        self.state = Some(vm.into_state());
        result
    }
}

fn instance_memory_error() -> BopError {
    BopError::fatal("Memory limit exceeded", 0)
}

struct VmOperationGuard<'a>(&'a Cell<bool>);

impl<'a> VmOperationGuard<'a> {
    fn begin(flag: &'a Cell<bool>) -> Result<Self, BopError> {
        if flag.replace(true) {
            return Err(error(0, "A Bop instance cannot be re-entered"));
        }
        Ok(Self(flag))
    }
}

impl Drop for VmOperationGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

// ─── Public entry points ──────────────────────────────────────────

/// Execute a pre-compiled [`Chunk`] against the supplied host.
///
/// The chunk is structurally validated before the VM starts, so malformed
/// hand-built or deserialized bytecode is reported as a [`BopError`] rather
/// than panicking or silently jumping beyond the instruction stream.
pub fn execute<H: BopHost>(
    chunk: Chunk,
    host: &mut H,
    limits: &BopLimits,
) -> Result<(), BopError> {
    crate::validate_chunk(&chunk)?;
    let vm = Vm::new(chunk, host, limits.clone());
    vm.run()
}

/// Parse, compile, and run Bop source.
///
/// This mirrors [`bop::run`] but routes through the bytecode VM.
pub fn run<H: BopHost>(
    source: &str,
    host: &mut H,
    limits: &BopLimits,
) -> Result<(), BopError> {
    let stmts = bop::parse(source)?;
    let chunk = crate::compile(&stmts)?;
    execute(chunk, host, limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct SilentHost;

    impl BopHost for SilentHost {
        fn call(
            &mut self,
            _name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, BopError>> {
            None
        }
    }

    #[test]
    fn sequential_broken_loops_leave_the_value_stack_balanced() {
        let mut source = String::new();
        for _ in 0..128 {
            source.push_str("for item in [1, 2, 3] { break }\n");
            source.push_str("repeat 3 { break }\n");
        }
        let program = bop::parse(&source).expect("parse");
        let chunk = crate::compile(&program).expect("compile");
        let mut host = SilentHost;
        let mut vm = Vm::new(chunk, &mut host, BopLimits::standard());

        vm.run_internal().expect("execute");

        assert!(
            vm.stack.is_empty(),
            "clean completion left {} loop sidecars on the value stack",
            vm.stack.len()
        );
    }

    #[test]
    fn loop_control_leaves_value_and_type_scope_stacks_balanced() {
        let source = r#"repeat 2048 {
    if true { continue }
}
for outer in [1, 2] {
    repeat 3 {
        while true {
            if true { break }
        }
        let label = match outer {
            n if n < 0 => "negative",
            _ => "positive",
        }
        if outer == 1 { continue }
        break
    }
}"#;
        let program = bop::parse(source).expect("parse");
        let chunk = crate::compile(&program).expect("compile");
        let mut host = SilentHost;
        let mut vm = Vm::new(chunk, &mut host, BopLimits::standard());

        vm.run_internal().expect("execute");

        let frame = vm.frames.last().expect("top frame");
        assert_eq!(frame.scopes.len(), frame.scope_base);
        assert_eq!(vm.type_bindings.len(), frame.type_scope_base);
        assert!(
            vm.stack.is_empty(),
            "clean completion left {} values or loop sidecars",
            vm.stack.len()
        );
    }

    #[test]
    fn frame_scope_floors_preserve_base_scopes_and_pair_type_scopes() {
        let mut host = SilentHost;
        let mut vm = Vm::new(Chunk::new(), &mut host, BopLimits::standard());

        // A redundant PopScope cannot remove the top-level binding map.
        vm.pop_scope();
        assert_eq!(vm.frames.last().unwrap().scopes.len(), 1);
        assert_eq!(vm.type_bindings.len(), 1);
        vm.push_scope();
        vm.pop_scope();
        assert_eq!(vm.frames.last().unwrap().scopes.len(), 1);
        assert_eq!(vm.type_bindings.len(), 1);

        // Named-function frames start with no value scope, but their first
        // pushed runtime scope is removable. Function exit also discards all
        // still-open type scopes, as an early return requires.
        vm.push_function_frame(
            Rc::new(Chunk::new()),
            Vec::new(),
            Vec::new(),
            0,
            None,
            FrameWrap::None,
        );
        vm.pop_scope();
        assert_eq!(vm.frames.last().unwrap().scopes.len(), 0);
        assert_eq!(vm.type_bindings.len(), 2);
        vm.push_scope();
        vm.pop_scope();
        assert_eq!(vm.frames.last().unwrap().scopes.len(), 0);
        assert_eq!(vm.type_bindings.len(), 2);
        vm.push_scope();
        vm.push_scope();
        vm.do_return(Value::None).expect("return");
        assert_eq!(vm.frames.len(), 1);
        assert_eq!(vm.type_bindings.len(), 1);

        // Closure frames retain their capture map while removing a match
        // scope above it.
        let mut captures = BTreeMap::new();
        captures.insert(String::from("captured"), Value::Int(10));
        vm.push_function_frame(
            Rc::new(Chunk::new()),
            Vec::new(),
            vec![captures],
            vm.stack.len(),
            None,
            FrameWrap::None,
        );
        vm.push_scope();
        vm.pop_scope();
        let frame = vm.frames.last().unwrap();
        assert_eq!(frame.scopes.len(), 1);
        assert!(matches!(
            frame.scopes[0].get("captured"),
            Some(Value::Int(10))
        ));
        assert_eq!(vm.type_bindings.len(), 2);

        // Error unwinding must remove both the try-call frame and a nested
        // function's still-open runtime scopes, restoring the top-level type
        // depth exactly.
        vm.do_return(Value::None).expect("closure return");
        vm.push_function_frame(
            Rc::new(Chunk::new()),
            Vec::new(),
            Vec::new(),
            vm.stack.len(),
            None,
            FrameWrap::TryCall { line: 1 },
        );
        vm.push_scope();
        vm.push_function_frame(
            Rc::new(Chunk::new()),
            Vec::new(),
            Vec::new(),
            vm.stack.len(),
            None,
            FrameWrap::None,
        );
        vm.push_scope();
        vm.unwind_to_try_call(error(1, "boom")).expect("unwind");
        assert_eq!(vm.frames.len(), 1);
        assert_eq!(vm.type_bindings.len(), 1);
    }

    #[test]
    fn named_call_frames_share_one_module_context() {
        let mut host = SilentHost;
        let mut vm = Vm::new(Chunk::new(), &mut host, BopLimits::standard());
        let root_context = Rc::clone(&vm.root_lexical_context);

        vm.push_function_frame(
            Rc::new(Chunk::new()),
            Vec::new(),
            Vec::new(),
            0,
            Some(bop::value::ROOT_MODULE_PATH.to_string()),
            FrameWrap::None,
        );

        assert!(Rc::ptr_eq(
            &vm.frames.last().unwrap().lexical_context,
            &root_context
        ));
        assert_eq!(vm.type_bindings.len(), 2);
        assert_eq!(vm.module_aliases.len(), 2);
        assert_eq!(vm.imported_functions.len(), 2);
        assert!(vm.type_bindings[1].is_empty());
        assert!(vm.module_aliases[1].is_empty());
        assert!(vm.imported_functions[1].is_empty());
    }

    #[test]
    fn capture_snapshot_distinguishes_missing_from_bound_none() {
        let program = bop::parse(
            r#"let present = none
let read = fn() { return [missing, present] }"#,
        )
        .expect("parse");
        let chunk = crate::compile(&program).expect("compile");
        let lambda = chunk.functions[0].clone();
        let mut host = SilentHost;
        let mut vm = Vm::new(chunk, &mut host, BopLimits::standard());
        vm.define_local("present".to_string(), Value::None);

        let captures = vm.snapshot_captures_for(&lambda);

        assert_eq!(captures.len(), 1, "missing bindings must not be invented");
        assert_eq!(captures[0].0, "present");
        assert!(matches!(captures[0].1, Value::None));
    }

    struct RetainingHost {
        retained: Option<Value>,
    }

    impl BopHost for RetainingHost {
        fn call(
            &mut self,
            name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, BopError>> {
            if name != "retain_large" {
                return None;
            }
            self.retained = Some(Value::new_str("x".repeat(16 * 1024)));
            Some(Ok(Value::None))
        }
    }

    #[test]
    fn instance_suspends_host_allocations_and_checks_final_returns() {
        let limits = BopLimits { max_steps: 100, max_memory: 32 };
        let mut host = RetainingHost { retained: None };
        let mut instance = BopInstance::load(
            "pub fn host_only() { retain_large() }\npub fn too_large() { return \"abcdefghijklmnopqrstuvwxyz0123456789\" }",
            &mut host,
            &limits,
        )
        .unwrap();
        instance.call("host_only", &[], &mut host).unwrap();
        assert!(host.retained.is_some());
        assert_eq!(instance.memory.__used(), 0);
        let error = instance.call("too_large", &[], &mut host).unwrap_err();
        assert!(error.is_fatal);
        assert!(error.message.contains("Memory limit"));
    }

    struct ExternalValueHost {
        value: Option<Value>,
    }

    impl BopHost for ExternalValueHost {
        fn call(
            &mut self,
            name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, BopError>> {
            (name == "take_external").then(|| Ok(self.value.take().unwrap_or(Value::None)))
        }
    }

    #[test]
    fn external_values_are_free_until_detach_and_memory_poison_is_fail_fast() {
        let external = {
            let _suspended = bop::memory::ActiveMemoryGuard::__suspend();
            Value::new_array((0..256).map(Value::Int).collect())
        };
        let limits = BopLimits { max_steps: 100, max_memory: 64 };
        let mut host = ExternalValueHost { value: Some(external) };
        let mut instance = BopInstance::load(
            "let stored = none\npub fn keep() { stored = take_external() }\npub fn mutate() { stored.push(256) }\npub fn harmless() { return 1 }",
            &mut host,
            &limits,
        )
        .unwrap();

        instance.call("keep", &[], &mut host).unwrap();
        assert_eq!(instance.memory.__used(), 0);
        let mutation_error = instance.call("mutate", &[], &mut host).unwrap_err();
        assert!(mutation_error.is_fatal);
        assert!(instance.memory.__used() > limits.max_memory);
        let poisoned = instance.call("harmless", &[], &mut host).unwrap_err();
        assert!(poisoned.is_fatal);
        assert!(poisoned.message.contains("Memory limit"));
    }

    #[test]
    fn returned_receipts_release_on_last_drop_and_instances_do_not_cross_charge() {
        let mut host = SilentHost;
        let source = "pub fn make(x) { return [x, x, x, x] }\npub fn harmless() { return none }";
        let limits = BopLimits::standard();
        let mut first = BopInstance::load(source, &mut host, &limits).unwrap();
        let mut second = BopInstance::load(source, &mut host, &limits).unwrap();

        let first_value = first.call("make", &[Value::Int(1)], &mut host).unwrap();
        let first_bytes = first.memory.__used();
        assert!(first_bytes > 0);
        assert_eq!(second.memory.__used(), 0);
        first.call("harmless", &[], &mut host).unwrap();
        assert_eq!(first.memory.__used(), first_bytes);

        let first_clone = first_value.clone();
        drop(first_value);
        assert_eq!(first.memory.__used(), first_bytes);
        let second_value = second.call("make", &[Value::Int(2)], &mut host).unwrap();
        let second_bytes = second.memory.__used();
        assert!(second_bytes > 0);
        assert_eq!(first.memory.__used(), first_bytes);
        drop(first_clone);
        assert_eq!(first.memory.__used(), 0);
        assert_eq!(second.memory.__used(), second_bytes);
        drop(second_value);
        assert_eq!(second.memory.__used(), 0);
    }

    struct HookAllocatingHost {
        retained: RefCell<Vec<Value>>,
    }

    impl HookAllocatingHost {
        fn retain_large(&self) {
            self.retained
                .borrow_mut()
                .push(Value::new_str("x".repeat(16 * 1024)));
        }
    }

    impl BopHost for HookAllocatingHost {
        fn call(
            &mut self,
            name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, BopError>> {
            self.retain_large();
            (name == "host_value").then_some(Ok(Value::None))
        }

        fn on_print(&mut self, _message: &str) {
            self.retain_large();
        }

        fn function_hint(&self) -> &str {
            self.retain_large();
            "host hint"
        }

        fn on_tick(&mut self) -> Result<(), BopError> {
            self.retain_large();
            Ok(())
        }

        fn resolve_module(&mut self, name: &str) -> Option<Result<String, BopError>> {
            self.retain_large();
            (name == "hook").then(|| Ok(String::new()))
        }
    }

    #[test]
    fn every_instance_host_hook_runs_with_accounting_suspended() {
        let limits = BopLimits { max_steps: 100, max_memory: 64 };
        let mut host = HookAllocatingHost { retained: RefCell::new(Vec::new()) };
        let mut instance = BopInstance::load(
            "use hook\npub fn print_it() { print(\"ok\") }\npub fn host_it() { host_value() }\npub fn hint_it() { missing() }",
            &mut host,
            &limits,
        )
        .unwrap();
        assert_eq!(instance.memory.__used(), 0);
        instance.call("print_it", &[], &mut host).unwrap();
        instance.call("host_it", &[], &mut host).unwrap();
        let error = instance.call("hint_it", &[], &mut host).unwrap_err();
        assert!(!error.is_fatal);
        assert!(error
            .friendly_hint
            .as_deref()
            .is_some_and(|hint| hint.contains("host hint")));
        assert_eq!(instance.memory.__used(), 0);
        assert!(host.retained.borrow().len() >= 8);
    }

    #[test]
    fn same_instance_reentry_rejection_precedes_target_and_arity_checks() {
        let mut host = SilentHost;
        let mut instance = BopInstance::load(
            "pub fn entry() {}",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        instance.in_operation.set(true);
        let error = instance
            .call("missing", &[Value::None], &mut host)
            .unwrap_err();
        instance.in_operation.set(false);
        assert_eq!(error.line, Some(0));
        assert!(error.message.contains("cannot be re-entered"));
    }

    struct MapModuleHost {
        modules: BTreeMap<String, String>,
    }

    impl BopHost for MapModuleHost {
        fn call(
            &mut self,
            _name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, BopError>> {
            None
        }

        fn resolve_module(&mut self, name: &str) -> Option<Result<String, BopError>> {
            self.modules.get(name).cloned().map(Ok)
        }
    }

    #[test]
    fn vm_module_compilation_error_retains_transitive_source_context() {
        let root_source = "use outer";
        let inner_source = "let okay = 1\nlet broken =";
        let mut host = MapModuleHost {
            modules: BTreeMap::from([
                ("outer".to_string(), "use inner\nlet outer = 1".to_string()),
                ("inner".to_string(), inner_source.to_string()),
            ]),
        };

        let error =
            crate::run(root_source, &mut host, &BopLimits::standard()).unwrap_err();
        let context = error.source_context.as_ref().expect("module context");

        assert_eq!(context.module_path, "inner");
        assert_eq!(context.source.as_deref(), Some(inner_source));
        let rendered = error.render(root_source);
        assert!(rendered.contains("in module `inner` at line 2"));
        assert!(rendered.contains("let broken ="));
        assert!(!rendered.contains("1 | use outer"));
    }

    #[test]
    fn module_compatibility_snapshots_do_not_force_named_cow_detaches() {
        let mut host = MapModuleHost {
            modules: BTreeMap::from([
                (
                    "leaf".to_string(),
                    "let items = [1, 2, 3]\nfn pop() { items.pop() }".to_string(),
                ),
                (
                    "facade".to_string(),
                    "use leaf\nfn pop() { items.pop() }".to_string(),
                ),
            ]),
        };
        let mut instance = BopInstance::load(
            "use leaf as leaf\nuse facade as facade\npub fn facade_pop() { facade.pop() }\npub fn direct_pop() { leaf.pop() }",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        let loaded_bytes = instance.memory.__used();
        assert!(loaded_bytes > 0);

        instance.call("facade_pop", &[], &mut host).unwrap();
        assert_eq!(instance.memory.__used(), loaded_bytes);
        instance.call("direct_pop", &[], &mut host).unwrap();
        assert_eq!(instance.memory.__used(), loaded_bytes);
    }

    #[test]
    fn facade_fanout_reuses_the_origin_compatibility_snapshot() {
        const FACADES: usize = 8;
        let items = (0..256)
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let mut modules = BTreeMap::from([(
            "leaf".to_string(),
            format!("let items = [{items}]\nfn size() {{ return items.len() }}"),
        )]);
        let mut source = String::new();
        for index in 0..FACADES {
            modules.insert(format!("facade{index}"), "use leaf".to_string());
            source.push_str(&format!("use facade{index} as f{index}\n"));
            source.push_str(&format!("let size{index} = f{index}.size()\n"));
        }
        let mut host = MapModuleHost { modules };
        let instance = BopInstance::load(&source, &mut host, &BopLimits::standard()).unwrap();

        let imports = instance.state.as_ref().unwrap().imports.borrow();
        let ImportSlot::Loaded(leaf) = imports.get("leaf").unwrap() else {
            panic!("leaf should be loaded")
        };
        let leaf_items = &leaf
            .bindings
            .iter()
            .find(|(name, _)| name == "items")
            .unwrap()
            .1;
        for index in 0..FACADES {
            let ImportSlot::Loaded(facade) = imports.get(&format!("facade{index}")).unwrap()
            else {
                panic!("facade should be loaded")
            };
            let facade_items = &facade
                .bindings
                .iter()
                .find(|(name, _)| name == "items")
                .unwrap()
                .1;
            assert!(leaf_items.__shares_backing_with(facade_items));
        }
    }

    #[test]
    fn recursive_module_snapshots_do_not_retain_replaced_instance_receipts() {
        let mut host = MapModuleHost {
            modules: BTreeMap::from([(
                "leaf".to_string(),
                "let items = [[\"abcdefghijklmnopqrstuvwxyz0123456789\"]]\nlet captured = \"zyxwvutsrqponmlkjihgfedcba9876543210\"\nlet callback = fn() { return captured }\nfn clear() { items = []; captured = none; callback = none }"
                    .to_string(),
            )]),
        };
        let mut instance = BopInstance::load(
            "use leaf as leaf\npub fn clear() { leaf.clear() }",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        let loaded_bytes = instance.memory.__used();
        assert!(loaded_bytes > 0);

        instance.call("clear", &[], &mut host).unwrap();
        let cleared_bytes = instance.memory.__used();
        assert!(cleared_bytes < loaded_bytes);

        let mut empty_host = MapModuleHost {
            modules: BTreeMap::from([(
                "leaf".to_string(),
                "let items = []\nlet captured = none\nlet callback = none\nfn clear() { items = []; captured = none; callback = none }"
                    .to_string(),
            )]),
        };
        let empty = BopInstance::load(
            "use leaf as leaf\npub fn clear() { leaf.clear() }",
            &mut empty_host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert_eq!(cleared_bytes, empty.memory.__used());
    }

    struct WrappingModuleHost {
        modules: BTreeMap<String, String>,
    }

    impl BopHost for WrappingModuleHost {
        fn call(
            &mut self,
            name: &str,
            args: &[Value],
            line: u32,
        ) -> Option<Result<Value, BopError>> {
            if name != "wrap" {
                return None;
            }
            Some(
                bop::value::BopModule::try_new(
                    "host.wrapper".to_string(),
                    vec![("child".to_string(), args[0].clone())],
                    Vec::new(),
                    line,
                )
                .map(Value::Module),
            )
        }

        fn resolve_module(&mut self, name: &str) -> Option<Result<String, BopError>> {
            self.modules.get(name).cloned().map(Ok)
        }
    }

    #[test]
    fn module_snapshots_externalize_host_module_binding_graphs() {
        let root = "use leaf as leaf\npub fn clear() { leaf.clear() }";
        let mut host = WrappingModuleHost {
            modules: BTreeMap::from([(
                "leaf".to_string(),
                "let child = [\"abcdefghijklmnopqrstuvwxyz0123456789\"]\nlet wrapped = wrap(child)\nfn clear() { child = none; wrapped = none }"
                    .to_string(),
            )]),
        };
        let mut instance = BopInstance::load(root, &mut host, &BopLimits::standard()).unwrap();
        let loaded_bytes = instance.memory.__used();
        assert!(loaded_bytes > 0);
        instance.call("clear", &[], &mut host).unwrap();
        let cleared_bytes = instance.memory.__used();
        assert!(cleared_bytes < loaded_bytes);

        let mut empty_host = WrappingModuleHost {
            modules: BTreeMap::from([(
                "leaf".to_string(),
                "let child = none\nlet wrapped = none\nfn clear() { child = none; wrapped = none }"
                    .to_string(),
            )]),
        };
        let empty = BopInstance::load(root, &mut empty_host, &BopLimits::standard()).unwrap();
        assert_eq!(cleared_bytes, empty.memory.__used());
    }
}
