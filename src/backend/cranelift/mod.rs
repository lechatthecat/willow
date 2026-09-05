use anyhow::{Result, bail};
use cranelift_codegen::ir::{
    AbiParam, InstBuilder, MemFlagsData, StackSlotData, StackSlotKind, TrapCode, UserFuncName,
    condcodes::{FloatCC, IntCC},
    types,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use std::collections::{HashMap, HashSet};

use crate::backend::abi;
use crate::parser::ast::*;
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::ids::{FunctionId, FunctionMap};
use crate::semantic::intrinsics;
use crate::semantic::symbols::{EnumInfo, InterfaceInfo};
use crate::{BuildMode, CompilerOptions};

mod ast_passes;
mod async_codegen;
mod compile;
pub use compile::{DeclaredModule, DeclaredProgram, ItemBinding, UnitImports};
mod coop;
mod coop_anf;
mod emit;
mod emit_builtins;
mod emit_collections;
mod emit_expr;
mod emit_interface;
mod emit_match;
mod emit_object;
mod emit_option_result;
mod emit_pow;
mod emit_pow_f64;
mod emit_stmt;
mod gc_codegen;
mod lir_gen;
mod option_repr;
mod panic_effect;
mod std_collection;
mod symbols;
mod type_helpers;
mod vtable_layout;
use ast_passes::*;
use coop::*;
use gc_codegen::*;
use option_repr::*;
use std_collection::*;
use symbols::*;
use type_helpers::*;

const USER_MAIN_SYMBOL: &str = "willow_user_main";
/// Generated function that initializes all `static` properties before `main`
/// (willow-qsqf §13.5).
const STATIC_INIT_SYMBOL: &str = "__willow_static_init";
const GC_REF_MASK_BITS: usize = 64;
const OBJECT_FIELD_MASK_CAPACITY: usize = GC_REF_MASK_BITS - 1;
const ASYNC_FRAME_HEADER_WORDS: usize = willow_abi::async_frame::HEADER_WORDS as usize;
const ASYNC_FRAME_GC_SLOT_CAPACITY: usize = GC_REF_MASK_BITS - ASYNC_FRAME_HEADER_WORDS;
const ASYNC_FRAME_LARGE_WARNING_BYTES: usize = 8 * 1024;
const COOP_POLL_PREEMPTED: i64 = willow_abi::RuntimePollResult::Preempted as i64;
const COOP_POLL_PANICKED: i64 = willow_abi::RuntimePollResult::Panicked as i64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncFrameSizeWarning {
    pub source_file: String,
    pub function_name: String,
    pub span: crate::diagnostics::Span,
    pub size_bytes: usize,
}

#[derive(Debug, Clone)]
struct ParamDebug {
    name: String,
    ty: Type,
    mode: ParamMode,
}

#[derive(Default)]
struct ModuleAliasSnapshot {
    func_ids: Vec<(FunctionId, Option<FuncId>)>,
    func_return_types: Vec<(FunctionId, Option<Type>)>,
    fn_types: Vec<(FunctionId, Option<Type>)>,
    func_param_modes: Vec<(FunctionId, Option<Vec<ParamMode>>)>,
    func_param_debug: Vec<(FunctionId, Option<Vec<ParamDebug>>)>,
    function_may_panic: Vec<(FunctionId, Option<bool>)>,
    #[allow(clippy::type_complexity)]
    class_layouts: Vec<(String, Option<Vec<(String, Type)>>)>,
    #[allow(clippy::type_complexity)]
    class_own_fields: Vec<(String, Option<Vec<(String, Type)>>)>,
    class_base: Vec<(String, Option<String>)>,
    class_type_ids: Vec<(String, Option<i64>)>,
    class_vslots: Vec<(String, Option<Vec<String>>)>,
    class_descriptor_ids: Vec<(String, Option<DataId>)>,
    enum_infos: Vec<(String, Option<EnumInfo>)>,
    interface_infos: Vec<(String, Option<InterfaceInfo>)>,
    vtable_ids: Vec<((String, String), Option<DataId>)>,
}

fn insert_with_snapshot<K: Clone + std::hash::Hash + Eq, T: Clone>(
    snapshots: &mut Vec<(K, Option<T>)>,
    map: &mut HashMap<K, T>,
    key: K,
    value: T,
) {
    let old = map.insert(key.clone(), value);
    snapshots.push((key, old));
}

fn restore_snapshots<K: std::hash::Hash + Eq, T>(
    map: &mut HashMap<K, T>,
    snapshots: Vec<(K, Option<T>)>,
) {
    for (key, old) in snapshots.into_iter().rev() {
        match old {
            Some(value) => {
                map.insert(key, value);
            }
            None => {
                map.remove(&key);
            }
        }
    }
}

fn insert_function_with_snapshot<T: Clone>(
    snapshots: &mut Vec<(FunctionId, Option<T>)>,
    map: &mut FunctionMap<T>,
    name: &str,
    value: T,
) {
    let id = FunctionId::free_from_source_name(name);
    let old = map.insert_id(id.clone(), value);
    snapshots.push((id, old));
}

fn restore_function_snapshots<T>(
    map: &mut FunctionMap<T>,
    snapshots: Vec<(FunctionId, Option<T>)>,
) {
    for (id, old) in snapshots.into_iter().rev() {
        match old {
            Some(value) => {
                map.insert_id(id, value);
            }
            None => {
                map.remove_id(&id);
            }
        }
    }
}

/// Bytes before the first virtual method slot in a class descriptor: the
/// `type_id` at offset 0 (willow-fm7t). Slot `k` therefore lives at
/// `CLASS_DESCRIPTOR_HEADER_BYTES + k * 8`.
pub(super) const CLASS_DESCRIPTOR_HEADER_BYTES: u32 = 8;

/// What one unit's bare enum aliases displaced, so the tables can be put back
/// the way the next unit needs them (willow-nm0g).
#[derive(Default)]
pub struct EnumAliasScope {
    /// Each aliased name and the enum it stood in front of, if any.
    enums: Vec<(String, Option<EnumInfo>)>,
    /// Interfaces taken out for the span of the aliases.
    interfaces: Vec<(String, InterfaceInfo)>,
}

pub struct Codegen {
    module: ObjectModule,
    func_ids: FunctionMap<FuncId>,
    func_return_types: FunctionMap<Type>,
    /// Full `Type::Fn(params, ret)` for each declared function — used to type function values.
    fn_types: FunctionMap<Type>,
    /// Parameter passing modes for declared Willow functions, keyed like `func_ids`.
    func_param_modes: FunctionMap<Vec<ParamMode>>,
    /// Source-level parameter names/types/modes for debug reference-call hooks.
    func_param_debug: FunctionMap<Vec<ParamDebug>>,
    /// Conservative recoverable-panic summaries keyed by backend lookup name.
    /// Missing entries are `MAY_PANIC`; only an explicit `false` may remove a
    /// generated depth check or panic-return path (willow-s9ej.8).
    function_may_panic: FunctionMap<bool>,
    /// Imported module access name -> canonical symbol prefix. Every module
    /// the whole build declared, whether or not the unit being compiled
    /// imported it.
    known_modules: HashMap<String, String>,
    /// The module access names the file currently being compiled can actually
    /// see, i.e. the ones its own `import`s name (willow-vtlr). `known_modules`
    /// is the whole build, so an unrelated module declaring a class of the same
    /// bare name used to make that name ambiguous and cost the body its
    /// eligibility; resolution consults these first. Installed per unit exactly
    /// like `builtin_module_aliases`.
    visible_modules: HashSet<String>,
    /// The imports the resolver classified for the unit about to be declared,
    /// handed over by `set_unit_imports` (willow-vtlr, willow-28h8). Taken by
    /// the declaration phase, which installs each half where it belongs.
    unit_imports: compile::UnitImports,
    /// Local alias -> canonical builtin schema module (`import std::fs as
    /// files;` records `files -> fs`), for the file currently being compiled.
    /// The AST path never needs it because `normalize_std_collection_program`
    /// has already folded it into the program; the LIR path lowers from the raw
    /// frontend program and canonicalizes here (willow-nswv).
    builtin_module_aliases: HashMap<String, String>,
    /// Maps each lambda's source span to its generated private function name.
    lambda_names: HashMap<crate::diagnostics::Span, String>,
    /// Source names of async fns lowered as cooperative tasks (constructor +
    /// poll fn). Calling one schedules the task and returns its frame.
    cooperative_leaves: std::collections::HashSet<FunctionId>,
    string_literals: HashMap<String, DataId>,
    string_counter: usize,
    runtime_declared: bool,
    /// Per-class ordered field list: class_name -> [(field_name, type)].
    class_layouts: HashMap<String, Vec<(String, Type)>>,
    /// Build mode for source locations, call stacks, and debug instrumentation.
    build_mode: BuildMode,
    /// Source file path of the current compilation unit, used in diagnostics.
    source_file: String,
    /// Enum info for enum variant construction in generated code.
    enum_infos: HashMap<String, EnumInfo>,
    /// Maps child class name → base class name for inherited method dispatch.
    class_base: HashMap<String, String>,
    /// Maps each class name to a unique integer type_id for runtime dynamic dispatch.
    /// Type ids start at 1; 0 is reserved for null/unknown.
    class_type_ids: HashMap<String, i64>,
    /// The non-static fields each class declares ITSELF, in declaration order
    /// (willow-59gx). Recorded as classes are registered;
    /// [`Codegen::finalize_class_layouts`] turns it into `class_layouts`.
    class_own_fields: HashMap<String, Vec<(String, Type)>>,
    /// The `open`/`override` instance methods each class declares ITSELF, in
    /// declaration order (willow-fm7t). Recorded as classes are registered;
    /// [`Codegen::finalize_class_vslots`] turns it into `class_vslots`.
    class_own_vmethods: HashMap<String, Vec<String>>,
    /// Per-class VIRTUAL METHOD SLOT ORDER: the names of the methods this class
    /// dispatches through its descriptor, in slot order (willow-fm7t).
    ///
    /// A subclass's order EXTENDS its base's, so an inherited method keeps the
    /// ancestor's slot and an `override` rewrites that same slot instead of
    /// appending a new one. A method that is neither `open` nor `override` gets
    /// no slot: it can neither be overridden nor override anything, so a direct
    /// call to it is always right.
    class_vslots: HashMap<String, Vec<String>>,
    /// Maps each class name to its descriptor data symbol — word 0 of every
    /// object of that class (willow-fm7t). Offset 0 of the descriptor is the
    /// class's `type_id`; the virtual method slots follow it in
    /// [`Codegen::class_vslots`] order.
    class_descriptor_ids: HashMap<String, DataId>,
    /// The checker's authoritative type for every checked expression, keyed by
    /// span (willow-mb5). Consulted FIRST by the backend's type queries; the
    /// legacy structural derivation only covers unrecorded (compiler-
    /// synthesized) expressions.
    expr_types: HashMap<crate::diagnostics::Span, Type>,
    /// Lowered-IR functions of the entry program (willow-0g8j): a function in
    /// the supported subset is compiled by walking its LIR instead of the AST.
    lir_functions: HashMap<String, crate::ir::lowered::LirFunction>,
    /// Lifted lambda bodies in lowered IR, keyed by the lambda expression's
    /// span (willow-0g8j.2.2). The LIR cannot know the `$lambda.N` symbol, so
    /// `compile_program` moves these into `lir_functions` once it has assigned
    /// the names.
    lir_lambdas: HashMap<crate::diagnostics::Span, crate::ir::lowered::LirFunction>,
    /// Spans of unqualified enum-variant constructions (`Ok(42)`) → the enum they
    /// resolved to, so an otherwise-function-shaped `Call` is lowered as a
    /// variant allocation. Registered from the type checker (willow-60o.1).
    enum_variant_resolutions: HashMap<crate::diagnostics::Span, String>,
    /// Unqualified match-pattern spans → the enum-variant pattern they were
    /// reinterpreted as (`Ok(v)` → EnumVariantTuple). Registered from the type
    /// checker (willow-60o.1).
    pattern_resolutions: HashMap<crate::diagnostics::Span, Pattern>,
    /// Interface metadata (method order + signatures) for vtable codegen and
    /// interface method dispatch. Registered from the type checker.
    interface_infos: HashMap<String, InterfaceInfo>,
    /// Static vtable data object per `(class, interface)` pair, used to box a
    /// concrete class value into an interface value (willow-xds).
    vtable_ids: HashMap<(String, String), DataId>,
    /// Global storage for each `static [mut] name: T = expr` property, keyed by
    /// (class_key, field) where class_key is the registered (module-qualified)
    /// class name (willow-qsqf). Holds 8 bytes (i64/ptr/f64/bool).
    static_storage: HashMap<(String, String), StaticStorageInfo>,
    /// Static-property initializers in program declaration order — replayed by
    /// the generated `__willow_static_init`, which runs before `main`.
    static_init_order: Vec<StaticInitItem>,
    /// Zero-initialized per-thread cursor/limit and allocation counters used by
    /// the inlined GC bump-allocation fast path.
    gc_tlab_state: DataId,
    async_frame_size_warnings: Vec<AsyncFrameSizeWarning>,
    /// Which source item owns each linker symbol the backend has handed out
    /// (willow-uqzx, catalog item 8). Symbol names are built by flattening `::`
    /// to `__`, which is not injective: `foo::bar` and a declaration literally
    /// named `foo__bar` produce the same string. Recording the owner turns that
    /// into a diagnostic instead of a duplicate-definition ICE.
    symbol_owners: HashMap<String, SymbolOwner>,
    symbol_conflicts: Vec<SymbolConflict>,
}

/// Whether `symbol` belongs to the runtime or to the compiler rather than to
/// user code (willow-uqzx, catalog item 8).
///
/// The runtime library owns `willow_*` and the compiler owns `__willow_*` for
/// its own generated data. A user definition landing on one of those names is
/// not a link error — the generated object simply defines it, and every call
/// the program makes reaches the user's version instead of the runtime's. That
/// is silent, so the name has to be refused up front.
///
/// Three shapes deliberately fall outside the reserved set:
///
/// * `willow_user_main`, which the compiler assigns to `fn main` — reserving it
///   would reject every program.
/// * Anything carrying a mangling separator (`.` or `$`). A runtime symbol is
///   always a plain C identifier, so a joined symbol like `willow_box.get`
///   cannot collide with one however its components are spelled. Only an
///   entry-file free function reaches the linker as a bare name, which makes
///   that the only declaration this check can fire on.
/// * `willow__*` with two underscores, which no runtime symbol uses.
fn is_reserved_symbol(symbol: &str) -> bool {
    if symbol == USER_MAIN_SYMBOL || symbols::is_mangled_symbol(symbol) {
        return false;
    }
    if abi::RUNTIME_SYMBOLS
        .iter()
        .any(|runtime| runtime.name == symbol)
    {
        return true;
    }
    symbol.starts_with("__willow_")
        || (symbol.starts_with("willow_") && !symbol.starts_with("willow__"))
}

/// The source item that claimed a linker symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolOwner {
    /// How to describe the item to the user, e.g. "function `bar` in module `foo`".
    pub item: String,
    pub source_file: String,
    pub span: crate::diagnostics::Span,
}

/// Why a declaration cannot have the linker symbol it asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolConflictKind {
    /// The symbol belongs to the runtime ABI or to the compiler's own internal
    /// namespace. Defining it in user code silently replaces the runtime's
    /// version for every call the generated program makes.
    Reserved,
    /// Another source item already claimed this symbol.
    Duplicate { previous: SymbolOwner },
}

/// A declaration that cannot be given the linker symbol its name maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolConflict {
    pub symbol: String,
    pub kind: SymbolConflictKind,
    pub owner: SymbolOwner,
}

/// Codegen metadata for one static property's global storage.
#[derive(Clone)]
struct StaticStorageInfo {
    data_id: DataId,
    ty: Type,
}

/// One static-property initializer to replay in `__willow_static_init`.
#[derive(Clone)]
struct StaticInitItem {
    class_key: String,
    field: String,
    init: Expr,
    ty: Type,
}

impl Codegen {
    /// Look up a declared runtime/user function id by symbol name, with a clear
    /// panic if it was never declared (e.g. a backend symbol missing from
    /// `abi.rs`) instead of an opaque index-out-of-bounds.
    fn func_id(&self, name: &str) -> FuncId {
        if crate::backend::abi::runtime_symbol(name).is_some_and(|symbol| {
            symbol
                .effects()
                .contains(crate::backend::abi::RuntimeEffects::MAY_PANIC)
        }) {
            panic!(
                "backend: MAY_PANIC runtime symbol `{name}` cannot be emitted from a raw Codegen call"
            );
        }
        *self
            .func_ids
            .get(name)
            .unwrap_or_else(|| panic!("backend: undeclared runtime symbol `{name}`"))
    }
    pub fn new(opts: &CompilerOptions) -> Result<Self> {
        let isa_builder = cranelift_native::builder().map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut flag_builder = settings::builder();
        match opts.target.build_mode {
            BuildMode::Debug => flag_builder.set("opt_level", "none")?,
            BuildMode::Release => flag_builder.set("opt_level", "speed")?,
        }
        let tls_model = if cfg!(target_os = "windows") {
            "coff"
        } else if cfg!(target_os = "macos") {
            "macho"
        } else {
            "elf_gd"
        };
        flag_builder.set("tls_model", tls_model)?;
        let flags = settings::Flags::new(flag_builder);
        let isa = isa_builder.finish(flags)?;
        // Willow's ABI is 64-bit throughout: every reference — GC handle,
        // string, array, class object, async frame, function address — is a
        // fixed 64-bit word on both sides of the runtime boundary
        // (`type_helpers::FN_ADDR_TYPE`, `backend::abi`). On a 32-bit host that
        // is silently wrong, and `func_addr` would fail deep inside Cranelift
        // with no mention of the real cause. Say it here instead. Lifting the
        // restriction is `willow-d9lm`, not a matter of relaxing this check.
        if isa.pointer_bits() != 64 {
            anyhow::bail!(
                "unsupported target `{}`: willow requires a 64-bit target, but this one has \
                 {}-bit pointers (the runtime ABI passes every reference as a 64-bit word)",
                isa.triple(),
                isa.pointer_bits(),
            );
        }
        let obj_builder =
            ObjectBuilder::new(isa, "willow", cranelift_module::default_libcall_names())?;
        let mut module = ObjectModule::new(obj_builder);
        let gc_tlab_state =
            module.declare_data("__willow_gc_tlab_state", Linkage::Local, true, true)?;
        let mut tlab_data = DataDescription::new();
        // Explicit zeroed bytes (not `define_zeroinit`, which lowers to BSS/TLS
        // and emits an `UninitializedTls` section the object writer rejects).
        tlab_data.define(vec![0u8; 32].into_boxed_slice());
        tlab_data.set_align(8);
        module.define_data(gc_tlab_state, &tlab_data)?;
        let mut class_layouts = HashMap::new();
        class_layouts.insert(
            "PanicInfo".to_string(),
            crate::semantic::builtin_types::panic_info_fields()
                .into_iter()
                .map(|(name, ty)| (name.to_string(), ty))
                .collect(),
        );
        let mut codegen = Self {
            module,
            func_ids: FunctionMap::default(),
            func_return_types: FunctionMap::default(),
            fn_types: FunctionMap::default(),
            func_param_modes: FunctionMap::default(),
            func_param_debug: FunctionMap::default(),
            function_may_panic: FunctionMap::default(),
            known_modules: HashMap::new(),
            visible_modules: HashSet::new(),
            unit_imports: compile::UnitImports::default(),
            builtin_module_aliases: HashMap::new(),
            lambda_names: HashMap::new(),
            cooperative_leaves: std::collections::HashSet::new(),
            string_literals: HashMap::new(),
            string_counter: 0,
            runtime_declared: false,
            class_layouts,
            build_mode: opts.target.build_mode,
            source_file: String::new(),
            enum_infos: HashMap::new(),
            class_base: HashMap::new(),
            class_type_ids: HashMap::new(),
            class_own_fields: HashMap::new(),
            class_own_vmethods: HashMap::new(),
            class_vslots: HashMap::new(),
            class_descriptor_ids: HashMap::new(),
            expr_types: HashMap::new(),
            lir_functions: HashMap::new(),
            lir_lambdas: HashMap::new(),
            enum_variant_resolutions: HashMap::new(),
            pattern_resolutions: HashMap::new(),
            interface_infos: HashMap::new(),
            vtable_ids: HashMap::new(),
            static_storage: HashMap::new(),
            static_init_order: Vec::new(),
            gc_tlab_state,
            async_frame_size_warnings: Vec::new(),
            symbol_owners: HashMap::new(),
            symbol_conflicts: Vec::new(),
        };
        // Define the private, self-contained floating exponentiation helpers
        // once per output object. They are local symbols and import no libm.
        codegen.declare_native_pow_f64()?;
        Ok(codegen)
    }

    fn record_async_frame_size_warning(
        &mut self,
        function_name: &str,
        span: crate::diagnostics::Span,
        layout: &AsyncFrameLayout,
    ) {
        let size_bytes = (ASYNC_FRAME_HEADER_WORDS + layout.slot_count()) * 8;
        if size_bytes >= ASYNC_FRAME_LARGE_WARNING_BYTES {
            self.async_frame_size_warnings.push(AsyncFrameSizeWarning {
                source_file: self.source_file.clone(),
                function_name: function_name.to_string(),
                span,
                size_bytes,
            });
        }
    }

    pub fn take_async_frame_size_warnings(&mut self) -> Vec<AsyncFrameSizeWarning> {
        std::mem::take(&mut self.async_frame_size_warnings)
    }

    /// Symbol conflicts recorded so far. The driver renders these instead of the
    /// generic codegen error, because they are user errors with a source
    /// location, not internal compiler failures.
    pub fn take_symbol_conflicts(&mut self) -> Vec<SymbolConflict> {
        std::mem::take(&mut self.symbol_conflicts)
    }

    /// Record `symbol` as belonging to `item`, or fail if it is already spoken
    /// for (willow-uqzx, catalog item 8).
    ///
    /// Two ways a user declaration can lose this race. It can land in the
    /// runtime's namespace, in which case the generated object defines a symbol
    /// the runtime library also defines and every runtime call in the program
    /// reaches the user's version instead — a miscompile with no diagnostic at
    /// all. Or it can collide with another user declaration, because `::` is
    /// flattened to `__` and nothing stops a name from containing `__` already;
    /// that one currently surfaces as a Cranelift duplicate-definition ICE with
    /// no source location.
    ///
    /// Both are reported here, with a span, and stop the compile.
    pub(super) fn claim_symbol(
        &mut self,
        symbol: &str,
        item: impl Into<String>,
        span: crate::diagnostics::Span,
    ) -> Result<()> {
        let owner = SymbolOwner {
            item: item.into(),
            source_file: self.source_file.clone(),
            span,
        };

        let kind = if is_reserved_symbol(symbol) {
            Some(SymbolConflictKind::Reserved)
        } else {
            self.symbol_owners
                .get(symbol)
                .cloned()
                .map(|previous| SymbolConflictKind::Duplicate { previous })
        };

        if let Some(kind) = kind {
            self.symbol_conflicts.push(SymbolConflict {
                symbol: symbol.to_string(),
                kind,
                owner,
            });
            // Stop immediately. Continuing would declare a symbol whose
            // `func_ids` entry now points at the wrong function, and the
            // resulting signature mismatch would abort inside Cranelift before
            // the driver could render this conflict.
            anyhow::bail!("symbol conflict on `{symbol}`");
        }

        self.symbol_owners.insert(symbol.to_string(), owner);
        Ok(())
    }

    /// Register enum info so the backend can lower enum variant construction.
    pub fn register_enum_info(&mut self, name: String, info: EnumInfo) {
        self.enum_infos.insert(name, info);
    }

    /// Install a unit's bare enum names for the length of that unit's own
    /// compilation, handing back what they displaced (willow-nm0g).
    ///
    /// The enum table is one flat namespace for the whole build, but a bare
    /// name is only unambiguous inside the unit that wrote it: one module's
    /// `enum Point` must not answer for another unit's `class Point` (the enum
    /// table is consulted first, so a live object would read as a tag and go
    /// untraced), and two modules that each declare `enum Kind` must each see
    /// their own. So the names go in around the unit and come back out again.
    ///
    /// An interface of the same name comes OUT for that span. The interface
    /// table is flat in the same way, and it is consulted first: with another
    /// unit's `interface Point` standing, the declaring module's own
    /// `Point::Near` reads as an interface and is refused. The alias is this
    /// unit's answer for the name, so nothing else may answer for it here.
    pub fn install_enum_aliases(&mut self, aliases: &[(String, EnumInfo)]) -> EnumAliasScope {
        let mut scope = EnumAliasScope::default();
        for (name, info) in aliases {
            scope.enums.push((
                name.clone(),
                self.enum_infos.insert(name.clone(), info.clone()),
            ));
            if let Some(interface) = self.interface_infos.remove(name.as_str()) {
                scope.interfaces.push((name.clone(), interface));
            }
        }
        scope
    }

    /// Undo an [`Codegen::install_enum_aliases`], putting back whatever each
    /// name held.
    pub fn restore_enum_aliases(&mut self, scope: EnumAliasScope) {
        for (name, info) in scope.enums {
            match info {
                Some(info) => {
                    self.enum_infos.insert(name, info);
                }
                None => {
                    self.enum_infos.remove(&name);
                }
            }
        }
        for (name, info) in scope.interfaces {
            self.interface_infos.insert(name, info);
        }
    }

    /// Register interface metadata for vtable generation and method dispatch.
    pub fn register_interface_info(&mut self, name: String, info: InterfaceInfo) {
        self.interface_infos.insert(name, info);
    }

    /// Register resolved async-fn local types (willow-lpn.5c) for frame-backing
    /// unannotated live-across-await locals.
    pub fn register_expr_types(&mut self, types: HashMap<crate::diagnostics::Span, Type>) {
        self.expr_types = types;
    }

    pub fn register_lir_functions(&mut self, lir: crate::ir::lowered::LirProgram) {
        self.lir_functions = lir
            .functions
            .into_iter()
            .map(|f| (f.name.clone(), f))
            .collect();
        self.lir_lambdas = lir
            .lambdas
            .into_iter()
            .map(|l| (l.span, l.function))
            .collect();
    }

    /// Register unqualified enum-variant construction resolutions (willow-60o.1).
    pub fn register_enum_variant_resolutions(
        &mut self,
        resolutions: HashMap<crate::diagnostics::Span, String>,
    ) {
        self.enum_variant_resolutions = resolutions;
    }

    /// Register unqualified match-pattern reinterpretations (willow-60o.1).
    pub fn register_pattern_resolutions(
        &mut self,
        resolutions: HashMap<crate::diagnostics::Span, Pattern>,
    ) {
        self.pattern_resolutions = resolutions;
    }

    /// Merge one module's own checker tables into the backend (willow-9vvn).
    ///
    /// Every `register_*` above installs the ENTRY file's tables wholesale, but
    /// a module is type-checked in its own scope, so the entry checker resolved
    /// nothing inside a module body. Without this, an unqualified `Boxy(n)`
    /// pattern in a module reaches `emit_match` unresolved: it stays a
    /// `ClassDowncast`, which takes the wrong arm or panics outright.
    ///
    /// These tables are all keyed by `Span`, and a span carries the `file_id`
    /// of the file it came from, so a module's keys cannot collide with the
    /// entry file's or with another module's. That is why this extends the maps
    /// instead of replacing them, and why it must run after the entry
    /// registrations.
    ///
    /// It must also run BEFORE that module's `declare_module`, not just before
    /// its bodies: `declare_lambda` reads `expr_types` to give a lifted lambda
    /// its signature, so a module lambda declared ahead of the merge is declared
    /// `fn(i64) -> i64` whatever it really is, and every use of it is then
    /// refused for a signature mismatch (willow-9yhi).
    pub fn merge_module_checker_tables(&mut self, checker: &crate::semantic::TypeChecker) {
        self.expr_types
            .extend(checker.expr_types.iter().map(|(k, v)| (*k, v.clone())));
        self.enum_variant_resolutions.extend(
            checker
                .enum_variant_resolutions
                .iter()
                .map(|(k, v)| (*k, v.clone())),
        );
        self.pattern_resolutions.extend(
            checker
                .pattern_resolutions
                .iter()
                .map(|(k, v)| (*k, v.clone())),
        );
    }

    /// The type as the enum TABLES spell it: every enum name replaced by the one
    /// identity it answers to build-wide.
    ///
    /// The unit being declared may write an enum its own way — `Level` for
    /// `signal::Level`, or `Grade` under `import signal::Level as Grade;` — and
    /// those spellings live only for that unit's own declaration phase
    /// ([`Codegen::install_enum_aliases`]). The signature tables outlive it and
    /// are compared against HIR types the checker already normalized to the
    /// identity, so recording the written spelling would leave the two halves
    /// unable to agree that `Grade` and `signal::Level` are one type
    /// (willow-0g8j.3). A name that is not an enum is its own identity and
    /// passes through.
    pub(super) fn canonical_enum_type(&self, ty: &Type) -> Type {
        let identity = |name: &String| -> String {
            self.enum_infos
                .get(name.as_str())
                .map(|info| info.name.clone())
                .unwrap_or_else(|| name.clone())
        };
        match ty {
            Type::Named(name) => Type::Named(identity(name)),
            Type::Generic(name, args) => Type::Generic(
                identity(name),
                args.iter().map(|a| self.canonical_enum_type(a)).collect(),
            ),
            Type::Array(element) => Type::Array(Box::new(self.canonical_enum_type(element))),
            Type::Fn(params, ret) => Type::Fn(
                params.iter().map(|p| self.canonical_enum_type(p)).collect(),
                Box::new(self.canonical_enum_type(ret)),
            ),
            _ => ty.clone(),
        }
    }

    /// No-op: generic enums are now registered via `register_enum_info` from the
    /// prelude, exactly like user-defined enums.  Kept for call-site compatibility.
    pub fn register_builtin_generic_enums(&mut self) {}

    /// Hand the back end the imports the module resolver classified for the
    /// next unit to be declared (willow-vtlr, willow-28h8).
    ///
    /// Must be called before that unit's `declare_module`/`declare_program`,
    /// which takes them: the visible-module half decides which module a bare
    /// class name in this file may come from, and the item half is bound now
    /// and rebound before this unit's bodies.
    pub fn set_unit_imports(&mut self, imports: compile::UnitImports) {
        self.unit_imports = imports;
    }

    /// Rebind the FUNCTION half of one unit's single-item imports, in import
    /// order.
    ///
    /// `func_ids` is global and keyed by the local name, so the binding that
    /// stands is whichever unit bound it last: two files that both say
    /// `import <module>::add;` for different modules share the name `add`, and
    /// the unit whose bodies are being lowered has to hold it (willow-28h8).
    ///
    /// Only the function half is rebound. A direct TYPE import aliases whole
    /// compiled tables — layouts, vtables, interface info — that the classes
    /// declared against them are already compiled to, and re-aliasing those
    /// between two units' bodies changes dispatch under compiled code.
    fn rebind_item_import_functions(&mut self, items: &[compile::ItemBinding]) {
        for item in items {
            self.bind_item_import_function(&item.local, &item.module, &item.item);
        }
    }

    /// Bind a single-item import: the local name aliases the module function's
    /// mangled symbol (`{module}__{item}`), so an unqualified call to `local`
    /// lowers to the module function. Must be called after the module is
    /// compiled. No-op if the symbol is absent (the type checker already
    /// reported the error).
    pub fn register_item_import(&mut self, local: &str, module: &str, item: &str) {
        if self.bind_item_import_function(local, module, item) {
            return;
        }
        let module_prefix = self
            .known_modules
            .get(module)
            .cloned()
            .unwrap_or_else(|| module_symbol_prefix(module));

        // Direct TYPE import (willow-64gs): alias the compiled tables of the
        // module-qualified type (`module::Item`) under the unqualified `local`
        // name, so the entry's use of `local` resolves to the module's symbols.
        let qualified = format!("{module}::{item}");
        if let Some(layout) = self.class_layouts.get(&qualified).cloned() {
            self.class_layouts.insert(local.to_string(), layout);
            if let Some(&id) = self.class_type_ids.get(&qualified) {
                self.class_type_ids.insert(local.to_string(), id);
            }
            if let Some(base) = self.class_base.get(&qualified).cloned() {
                self.class_base.insert(local.to_string(), base);
            }
            // The virtual slot order and the descriptor symbol travel with the
            // type, exactly as the type_id does: a directly imported class is
            // ONE runtime class under two spellings, and both must reach the
            // same descriptor (willow-fm7t; the aliasing trap willow-au5k hit).
            if let Some(own) = self.class_own_fields.get(&qualified).cloned() {
                self.class_own_fields.insert(local.to_string(), own);
            }
            if let Some(own) = self.class_own_vmethods.get(&qualified).cloned() {
                self.class_own_vmethods.insert(local.to_string(), own);
            }
            if let Some(slots) = self.class_vslots.get(&qualified).cloned() {
                self.class_vslots.insert(local.to_string(), slots);
            }
            if let Some(&descriptor) = self.class_descriptor_ids.get(&qualified) {
                self.class_descriptor_ids
                    .insert(local.to_string(), descriptor);
            }
            // Methods: alias every per-method table from
            // `{module_prefix}__{item}__M` to `{local}__M` (func id AND return
            // type / fn type / param modes / debug, so dispatch + return typing
            // resolve under the local name).
            let method_prefix = class_member_prefix(&module_item_symbol(&module_prefix, item));
            let method_symbols: Vec<String> = self
                .func_ids
                .ids()
                .map(ToString::to_string)
                .filter(|name| name.starts_with(&method_prefix))
                .collect();
            for full in method_symbols {
                let suffix = full.strip_prefix(&method_prefix).unwrap();
                let alias = class_member_symbol(local, suffix);
                if let Some(&id) = self.func_ids.get(&full) {
                    self.func_ids.insert(alias.clone(), id);
                }
                if let Some(rt) = self.func_return_types.get(&full).cloned() {
                    self.func_return_types.insert(alias.clone(), rt);
                }
                if let Some(ft) = self.fn_types.get(&full).cloned() {
                    self.fn_types.insert(alias.clone(), ft);
                }
                if let Some(modes) = self.func_param_modes.get(&full).cloned() {
                    self.func_param_modes.insert(alias.clone(), modes);
                }
                if let Some(pd) = self.func_param_debug.get(&full).cloned() {
                    self.func_param_debug.insert(alias.clone(), pd);
                }
                if let Some(may_panic) = self.function_may_panic.get(&full).copied() {
                    self.function_may_panic.insert(alias, may_panic);
                }
            }
            // Vtables: (`module::Item`, iface) -> (`local`, iface).
            let vt_aliases: Vec<((String, String), DataId)> = self
                .vtable_ids
                .iter()
                .filter(|((cls, _), _)| cls == &qualified)
                .map(|((_, iface), &d)| ((local.to_string(), iface.clone()), d))
                .collect();
            for (k, d) in vt_aliases {
                self.vtable_ids.insert(k, d);
            }
        }
        if let Some(info) = self.interface_infos.get(&qualified).cloned() {
            self.interface_infos.insert(local.to_string(), info);
        }
        if let Some(info) = self.enum_infos.get(&qualified).cloned() {
            self.enum_infos.insert(local.to_string(), info);
        }
    }

    /// The function half of [`Codegen::register_item_import`]: bind `local` to
    /// the mangled symbol of `module`'s `item`, with the signature tables that
    /// travel with it. Returns whether such a function exists.
    fn bind_item_import_function(&mut self, local: &str, module: &str, item: &str) -> bool {
        let module_prefix = self
            .known_modules
            .get(module)
            .cloned()
            .unwrap_or_else(|| module_symbol_prefix(module));
        let mangled = module_item_symbol(&module_prefix, item);
        if let Some(&id) = self.func_ids.get(&mangled) {
            self.func_ids.insert(local, id);
            if let Some(rt) = self.func_return_types.get(&mangled).cloned() {
                self.func_return_types.insert(local, rt);
            }
            if let Some(ft) = self.fn_types.get(&mangled).cloned() {
                self.fn_types.insert(local, ft);
            }
            if let Some(modes) = self.func_param_modes.get(&mangled).cloned() {
                self.func_param_modes.insert(local, modes);
            }
            if let Some(params) = self.func_param_debug.get(&mangled).cloned() {
                self.func_param_debug.insert(local, params);
            }
            if let Some(may_panic) = self.function_may_panic.get(&mangled).copied() {
                self.function_may_panic.insert(local, may_panic);
            }
            return true;
        }

        false
    }

    fn alias_function_symbol(
        &mut self,
        alias: &str,
        canonical: &str,
        aliases: &mut ModuleAliasSnapshot,
    ) {
        if let Some(&id) = self.func_ids.get(canonical) {
            insert_function_with_snapshot(&mut aliases.func_ids, &mut self.func_ids, alias, id);
        }
        if let Some(ret) = self.func_return_types.get(canonical).cloned() {
            insert_function_with_snapshot(
                &mut aliases.func_return_types,
                &mut self.func_return_types,
                alias,
                ret,
            );
        }
        if let Some(ty) = self.fn_types.get(canonical).cloned() {
            insert_function_with_snapshot(&mut aliases.fn_types, &mut self.fn_types, alias, ty);
        }
        if let Some(modes) = self.func_param_modes.get(canonical).cloned() {
            insert_function_with_snapshot(
                &mut aliases.func_param_modes,
                &mut self.func_param_modes,
                alias,
                modes,
            );
        }
        if let Some(params) = self.func_param_debug.get(canonical).cloned() {
            insert_function_with_snapshot(
                &mut aliases.func_param_debug,
                &mut self.func_param_debug,
                alias,
                params,
            );
        }
        if let Some(may_panic) = self.function_may_panic.get(canonical).copied() {
            insert_function_with_snapshot(
                &mut aliases.function_may_panic,
                &mut self.function_may_panic,
                alias,
                may_panic,
            );
        }
    }

    fn alias_class_symbol(
        &mut self,
        alias: &str,
        canonical: &str,
        aliases: &mut ModuleAliasSnapshot,
    ) {
        if let Some(layout) = self.class_layouts.get(canonical).cloned() {
            insert_with_snapshot(
                &mut aliases.class_layouts,
                &mut self.class_layouts,
                alias.to_string(),
                layout,
            );
        }
        if let Some(own) = self.class_own_fields.get(canonical).cloned() {
            insert_with_snapshot(
                &mut aliases.class_own_fields,
                &mut self.class_own_fields,
                alias.to_string(),
                own,
            );
        }
        if let Some(base) = self.class_base.get(canonical).cloned() {
            insert_with_snapshot(
                &mut aliases.class_base,
                &mut self.class_base,
                alias.to_string(),
                base,
            );
        }
        if let Some(type_id) = self.class_type_ids.get(canonical).copied() {
            insert_with_snapshot(
                &mut aliases.class_type_ids,
                &mut self.class_type_ids,
                alias.to_string(),
                type_id,
            );
        }
        if let Some(slots) = self.class_vslots.get(canonical).cloned() {
            insert_with_snapshot(
                &mut aliases.class_vslots,
                &mut self.class_vslots,
                alias.to_string(),
                slots,
            );
        }
        if let Some(descriptor) = self.class_descriptor_ids.get(canonical).copied() {
            insert_with_snapshot(
                &mut aliases.class_descriptor_ids,
                &mut self.class_descriptor_ids,
                alias.to_string(),
                descriptor,
            );
        }
        // Alias the class's (class, interface) vtables under the local name too, so
        // a module body that boxes its own class to an interface internally finds
        // the vtable (`(mod::Cls, mod::Iface)` -> `(Cls, mod::Iface)`); the entry's
        // `register_item_import` does the same for direct imports (willow-64gs.1).
        let vt_aliases: Vec<((String, String), DataId)> = self
            .vtable_ids
            .iter()
            .filter(|((cls, _), _)| cls == canonical)
            .map(|((_, iface), &d)| ((alias.to_string(), iface.clone()), d))
            .collect();
        for (key, data_id) in vt_aliases {
            insert_with_snapshot(&mut aliases.vtable_ids, &mut self.vtable_ids, key, data_id);
        }
    }

    fn restore_module_aliases(&mut self, aliases: ModuleAliasSnapshot) {
        restore_function_snapshots(&mut self.func_ids, aliases.func_ids);
        restore_function_snapshots(&mut self.func_return_types, aliases.func_return_types);
        restore_function_snapshots(&mut self.fn_types, aliases.fn_types);
        restore_function_snapshots(&mut self.func_param_modes, aliases.func_param_modes);
        restore_function_snapshots(&mut self.func_param_debug, aliases.func_param_debug);
        restore_function_snapshots(&mut self.function_may_panic, aliases.function_may_panic);
        restore_snapshots(&mut self.class_layouts, aliases.class_layouts);
        restore_snapshots(&mut self.class_own_fields, aliases.class_own_fields);
        restore_snapshots(&mut self.class_base, aliases.class_base);
        restore_snapshots(&mut self.class_type_ids, aliases.class_type_ids);
        restore_snapshots(&mut self.class_vslots, aliases.class_vslots);
        restore_snapshots(&mut self.class_descriptor_ids, aliases.class_descriptor_ids);
        restore_snapshots(&mut self.enum_infos, aliases.enum_infos);
        restore_snapshots(&mut self.interface_infos, aliases.interface_infos);
        restore_snapshots(&mut self.vtable_ids, aliases.vtable_ids);
    }

    /// While compiling a module body, bind the types this unit IMPORTED by
    /// single-item import under the local names it spells them by
    /// (`import proto::Describable;` -> `Describable`), for the length of this
    /// unit's bodies (willow-0g8j.3).
    ///
    /// The declaration phase deliberately does not do this — aliasing whole
    /// compiled tables per module changes what the classes declared after it
    /// are compiled against (willow-28h8) — and only the function half is
    /// rebound before the bodies. The type half is needed here all the same:
    /// the interface table is one flat build-wide namespace keyed by the
    /// CANONICAL name, so a module body that names an imported interface found
    /// nothing under `Describable`, and both back ends then read the value as a
    /// class. With one implementation that silently called it directly; with
    /// two it reached `emit_interface_dispatch`'s "no virtual slot but N
    /// candidate implementations" invariant and aborted the compile.
    ///
    /// Classes need no such alias: [`resolve_class_key`] already resolves a
    /// bare class name against every module's tables. Enums have their own, per
    /// unit and from that unit's own checker
    /// ([`Codegen::install_enum_aliases`]).
    ///
    /// Installed BEFORE [`Codegen::alias_module_local_types`] so a module's own
    /// declaration still wins over anything it imported under the same name.
    fn alias_item_import_types(
        &mut self,
        items: &[compile::ItemBinding],
        aliases: &mut ModuleAliasSnapshot,
    ) {
        for item in items {
            let qualified = format!("{}::{}", item.module, item.item);
            if let Some(info) = self.interface_infos.get(&qualified).cloned() {
                insert_with_snapshot(
                    &mut aliases.interface_infos,
                    &mut self.interface_infos,
                    item.local.clone(),
                    info,
                );
            }
        }
    }

    /// While compiling a module body, bind the module's own enums and interfaces
    /// under their unqualified local names (`module::Color` -> `Color`) so a
    /// function/method that references its own type internally resolves the
    /// registered info (enum variant tags, interface vtables) instead of silently
    /// falling back to variant tag 0 / an unboxed value (willow-64gs.1).
    fn alias_module_local_types(
        &mut self,
        program: &Program,
        mod_name: &str,
        aliases: &mut ModuleAliasSnapshot,
    ) {
        for item in &program.items {
            match item {
                Item::Enum(e) => {
                    let qualified = format!("{mod_name}::{}", e.name);
                    if let Some(info) = self.enum_infos.get(&qualified).cloned() {
                        insert_with_snapshot(
                            &mut aliases.enum_infos,
                            &mut self.enum_infos,
                            e.name.clone(),
                            info,
                        );
                    }
                }
                Item::Interface(i) => {
                    let qualified = format!("{mod_name}::{}", i.name);
                    if let Some(info) = self.interface_infos.get(&qualified).cloned() {
                        insert_with_snapshot(
                            &mut aliases.interface_infos,
                            &mut self.interface_infos,
                            i.name.clone(),
                            info,
                        );
                    }
                }
                Item::Function(_) | Item::Class(_) => {}
            }
        }
    }

    fn class_method_symbol(&self, class_name: &str, method_name: &str) -> String {
        class_method_symbol_name(&self.known_modules, class_name, method_name)
    }

    // ── Class helpers ─────────────────────────────────────────────────────────

    fn register_class_layout(&mut self, c: &ClassDecl) {
        let own: Vec<(String, Type)> = c
            .fields
            .iter()
            .filter(|f| !f.is_static)
            .map(|f| (f.name.clone(), f.ty.clone()))
            .collect();
        // A provisional layout so nothing that runs before
        // `finalize_class_layouts` sees a missing class, and the whole answer
        // for a class with no base.
        self.class_layouts.insert(c.name.clone(), own.clone());
        self.class_own_fields.insert(c.name.clone(), own);
        if let Some(base_path) = &c.base_class {
            // `TypePath::name()` deliberately returns only the final segment,
            // which is right for diagnostics but not for backend identity. A
            // qualified base must keep its module/alias prefix so it matches
            // the key `declare_module` registered (`lib::Parcel`), otherwise
            // `finalize_class_layouts` drops every inherited field (willow-b929).
            let base_name = match base_path {
                TypePath::Local(name) => name.clone(),
                TypePath::Qualified(parts) => parts.join("::"),
            };
            self.class_base.insert(c.name.clone(), base_name);
        }
        // Assign a unique type_id for runtime dynamic dispatch. It lives at
        // offset 0 of the class DESCRIPTOR, which word 0 of every object of the
        // class points at (willow-fm7t).
        let next_id = self.class_type_ids.len() as i64 + 1;
        self.class_type_ids.entry(c.name.clone()).or_insert(next_id);
        self.register_class_own_vmethods(c);
    }

    /// Record the `open`/`override` instance methods `c` declares itself.
    ///
    /// Only `open` and `override` methods get a slot. A plain method can
    /// neither be overridden nor override anything, so its callee is fixed at
    /// compile time and a direct call is always correct; leaving it out keeps
    /// descriptors to the methods that actually vary. Static methods and
    /// constructors have no receiver to dispatch on at all.
    fn register_class_own_vmethods(&mut self, c: &ClassDecl) {
        let own: Vec<String> = c
            .methods
            .iter()
            // `!is_static` is the receiver test: `has_self` records only the
            // explicit (legacy) `self` spelling, and an implicit-self
            // `open`/`override` method has a receiver just the same. Requiring
            // it here left that method with no slot, and virtual dispatch then
            // found two implementations and nowhere to dispatch through
            // (willow-h7hv).
            .filter(|m| !m.is_static && (m.is_open || m.is_override))
            .map(|m| m.name.clone())
            .collect();
        self.class_own_vmethods.insert(c.name.clone(), own);
    }

    /// Turn the per-class own-field lists into each class's full field layout,
    /// walking `class_base` from the ROOT down.
    ///
    /// Base fields come FIRST, so a subclass's layout extends its base's and
    /// the offset of an inherited field is the same through a base-typed
    /// reference as through the subclass's own. A name a class redeclares keeps
    /// the ancestor's slot rather than adding a second one.
    ///
    /// Done as a separate pass rather than during registration because classes
    /// arrive in DECLARATION order, and a subclass may be declared before its
    /// base (willow-59gx). The previous two-pass scheme -- own fields for
    /// everyone, then rebuild each class from its base's entry in declaration
    /// order -- got one level right by accident and dropped the grandparent's
    /// fields at two, because the base it read had not been rebuilt yet.
    fn finalize_class_layouts(&mut self) {
        let classes: Vec<String> = self.class_own_fields.keys().cloned().collect();
        for class_name in classes {
            let chain = self.ancestor_chain(&class_name);
            let mut fields: Vec<(String, Type)> = Vec::new();
            for ancestor in chain.iter().rev() {
                let Some(own) = self.class_own_fields.get(ancestor) else {
                    continue;
                };
                for (name, ty) in own {
                    if !fields.iter().any(|(n, _)| n == name) {
                        fields.push((name.clone(), ty.clone()));
                    }
                }
            }
            self.class_layouts.insert(class_name, fields);
        }
    }

    /// `class_name` followed by its bases, nearest first.
    ///
    /// A cyclic `extends` -- already a checker error, but reachable here when
    /// the backend is driven directly -- stops at the repeat rather than
    /// looping forever.
    fn ancestor_chain(&self, class_name: &str) -> Vec<String> {
        let mut chain = vec![class_name.to_string()];
        let mut seen: HashSet<String> = HashSet::from([class_name.to_string()]);
        while let Some(base) = self.class_base.get(chain.last().expect("non-empty")) {
            if !seen.insert(base.clone()) {
                break;
            }
            chain.push(base.clone());
        }
        chain
    }

    /// Turn the per-class `open`/`override` lists into each class's full slot
    /// order, walking `class_base` from the ROOT down.
    ///
    /// Starting from the base's order and appending only names it does not
    /// already carry is what makes inheritance and `override` fall out for
    /// free: an inherited method keeps the ancestor's slot index, and an
    /// `override` is the same name at the same index, so it REPLACES the entry
    /// rather than adding one. `declare_one_class_descriptor` then fills each
    /// slot with `resolve_class_method_func_id`, which walks to the ancestor
    /// for a method this class did not redeclare.
    ///
    /// Done as a separate pass rather than during registration because classes
    /// arrive in DECLARATION order, and a subclass may be declared before its
    /// base. Walking the chain here makes the result order-independent.
    fn finalize_class_vslots(&mut self) {
        let classes: Vec<String> = self.class_own_vmethods.keys().cloned().collect();
        for class_name in classes {
            // Root-most ancestor first, so each level appends onto the order it
            // inherits.
            let chain = self.ancestor_chain(&class_name);
            let mut slots: Vec<String> = Vec::new();
            for ancestor in chain.iter().rev() {
                let Some(own) = self.class_own_vmethods.get(ancestor) else {
                    continue;
                };
                for method in own {
                    if !slots.contains(method) {
                        slots.push(method.clone());
                    }
                }
            }
            self.class_vslots.insert(class_name, slots);
        }
    }

    fn validate_gc_ref_mask_layouts(&self) -> Result<()> {
        for (class_name, layout) in &self.class_layouts {
            try_gc_ref_mask_for_layout(class_name, layout, &self.enum_infos)?;
        }
        Ok(())
    }

    /// Find the func_id for `class_name::method_name`, searching the class and
    /// then its ancestors (an inherited method satisfies the interface).
    fn resolve_class_method_func_id(&self, class_name: &str, method_name: &str) -> Option<FuncId> {
        let mut search = Some(class_name.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = search {
            if !seen.insert(name.clone()) {
                break;
            }
            let mangled = class_method_symbol_name(&self.known_modules, &name, method_name);
            if let Some(&fid) = self.func_ids.get(&mangled) {
                return Some(fid);
            }
            search = self.class_base.get(&name).cloned();
        }
        None
    }

    pub fn embed_runtime_metadata(&mut self, metadata: &str) -> Result<()> {
        let data_id = self.module.declare_data(
            "willow_runtime_metadata_v1",
            Linkage::Export,
            false,
            false,
        )?;
        let mut data = DataDescription::new();
        let mut bytes = b"willow_runtime_metadata_v1\n".to_vec();
        bytes.extend_from_slice(metadata.as_bytes());
        bytes.push(0);
        data.define(bytes.into_boxed_slice());
        self.module.define_data(data_id, &data)?;
        Ok(())
    }

    pub fn finish(self) -> Result<Vec<u8>> {
        let obj = self.module.finish();
        Ok(obj.emit()?)
    }
}

/// Code executed by one queued defer. Direct calls use a synthetic statement
/// whose operands read hidden registration-time slots; match expressions and
/// blocks retain their deferred AST body (willow-oorh).
#[derive(Clone)]
pub(super) enum DeferredAction {
    Stmt(Box<Stmt>),
    Block(Block),
    HirExpr(crate::ir::typed_ast::HirExpr),
    HirBlock(Vec<crate::ir::typed_ast::HirStmt>),
}

/// One queued defer: the deferred action, the async
/// registration-flag offset (None for sync), and the hidden frame bindings to
/// re-insert at flush time — coop loop bodies restore `vars`, so the names
/// must be rebound before the flush emits (async only; sync uses stack slots
/// still in scope).
#[derive(Clone)]
pub(super) struct DeferEntry {
    id: usize,
    action: DeferredAction,
    flag_offset: Option<i32>,
    sync_flag_slot: Option<cranelift_codegen::ir::StackSlot>,
    bindings: Vec<(String, i32, Type)>,
    vars_at_registration: HashMap<String, VarStorage>,
    /// The deferred AST contains a direct call to the compiler-known
    /// `recover()` builtin. Calls hidden behind another function/lambda do not
    /// grant recovery capability (willow-s9ej.3).
    recovery_capable: bool,
}

#[derive(Clone)]
pub(super) struct PanicScope {
    cleanup: cranelift_codegen::ir::Block,
    resume: cranelift_codegen::ir::Block,
    root_depth_at_entry: cranelift_codegen::ir::Value,
    defer_depth: usize,
    vars_before: HashMap<String, VarStorage>,
    /// Native-stack root depth at cooperative scope entry. `None` for an
    /// ordinary synchronous function.
    coop_root_depth_at_entry: Option<usize>,
}

/// A `lock` critical section currently being emitted (willow-38w.1.4). The
/// lock is held for exactly the CFG paths that leave the section, so release is
/// driven from the same unwinding the `defer` machinery already performs.
#[derive(Clone)]
pub(super) struct CoopLockScope {
    /// Mutex/read/write selects the runtime state machine and whether cleanup
    /// commits the frame-backed binding before release.
    mode: LockMode,
    /// Frame offset of the evaluated lock handle. The release hook zeroes it,
    /// so no path can commit or release the same acquisition twice, and the
    /// cancel entry reads it to find a lock the cancelled task still holds.
    handle_offset: i32,
    /// Frame offset of this acquisition's registration token.
    token_offset: i32,
    /// Frame offset of the protected value's binding slot — the value committed
    /// back on a clean exit.
    value_offset: i32,
    /// Frame offset of the acquisition phase word (`LockStmt::phase_frame_key`):
    /// non-zero exactly while the binding slot holds a value this section
    /// loaded and has not committed yet.
    phase_offset: i32,
    /// Protected value type: the binding slot's load type, and whether the
    /// commit needs a GC write barrier.
    value_ty: Type,
    /// `defer_stack` depth of the critical section's own defer frame. Release
    /// happens immediately after that frame unwinds, so the section's defers
    /// run holding the lock and the enclosing scopes' defers run without it.
    defer_depth: usize,
}

/// A `lock` site an async fn's cancel entry has to clean up (willow-38w.1.4):
/// enough to drop a pending wait, commit a value the cancelled task had
/// already loaded, and release a lock it still owns.
///
/// It carries the same phase / value / ordering information as
/// [`CoopLockScope`] because the cancel entry has to reproduce the clean-exit
/// contract — commit through the write barrier, then release, then drop the
/// frame root — at the right point in the defer sequence, and the frame is all
/// it has to work from.
#[derive(Clone)]
pub(super) struct AsyncLockSite {
    mode: LockMode,
    handle_offset: i32,
    token_offset: i32,
    phase_offset: i32,
    value_offset: i32,
    value_ty: Type,
    /// Position in the function's cleanup sequence, shared with
    /// [`AsyncDeferSite::order`]: assigned where the critical section OPENS, so
    /// reverse order runs the section's own defers first, then this release,
    /// then the defers registered around it.
    order: usize,
}

/// One `defer` site inside an async fn: the deferred action,
/// the frame offset of its registration flag, and any hidden frame-backed
/// operand bindings the action references.
#[derive(Clone)]
pub(super) struct AsyncDeferSite {
    action: DeferredAction,
    flag_offset: i32,
    bindings: Vec<(String, i32, Type)>,
    recovery_capable: bool,
    /// Position in the function's cleanup sequence, shared with
    /// [`AsyncLockSite::order`] so the cancel entry can interleave the two
    /// kinds of cleanup instead of running all defers and then all releases.
    order: usize,
}

struct FuncGen<'a, 'b> {
    builder: &'a mut FunctionBuilder<'b>,
    module: &'a mut ObjectModule,
    gc_tlab_state: DataId,
    /// (exit block, continue target, GC-root count at loop entry, defer-frame
    /// depth at loop entry — break/continue flush frames deeper than this).
    loop_stack: Vec<(
        cranelift_codegen::ir::Block,
        cranelift_codegen::ir::Block,
        usize,
        usize,
    )>,
    /// Lexical scope frames of registered `defer` actions: synthetic
    /// statements whose operands were already evaluated into hidden locals,
    /// plus (async only) the frame offset of the registration FLAG consumed
    /// before cleanup begins (willow-s9ej.1).
    defer_stack: Vec<Vec<DeferEntry>>,
    defer_counter: usize,
    /// Pre-zeroed registration flags for synchronous defer sites in the
    /// lexical block currently being emitted, keyed by source span.
    sync_defer_flags: HashMap<crate::diagnostics::Span, cranelift_codegen::ir::StackSlot>,
    /// Synchronous lexical scopes that are valid recovery continuations.
    /// Async panic unwinding is deliberately deferred to willow-s9ej.6.
    panic_scopes: Vec<PanicScope>,
    /// Defer registrations already consumed on the cleanup path currently
    /// being emitted. A nested panic must not execute them again.
    unavailable_defer_ids: HashSet<usize>,
    /// Number of compiler-generated panic-defer entries surrounding the code
    /// currently being emitted. A nested panic abandons each before raising
    /// its own panic record.
    panic_defer_codegen_depth: usize,
    /// Direct `recover()` calls lower to the runtime capability only while a
    /// recovery-capable deferred AST body is being emitted. Helpers/lambdas
    /// are separate functions and therefore start at zero.
    recover_eligible_depth: usize,
    /// Scope resume blocks that have an incoming recovery edge.
    panic_recovery_targets: HashSet<cranelift_codegen::ir::Block>,
    /// Shared abnormal return used by synchronous non-boundary functions.
    /// Callees return an ABI-safe neutral value while panic state remains
    /// active; callers branch before observing that value (willow-s9ej.4).
    panic_return_block: Option<cranelift_codegen::ir::Block>,
    /// Shadow-root depth inherited from the caller at function entry.
    panic_function_root_depth: Option<cranelift_codegen::ir::Value>,
    /// Debug call-chain frames installed by this generated function and not
    /// yet popped on the source path currently being emitted.
    callstack_frame_depth: usize,
    /// Source span of the statement being emitted. Debug builds publish it as
    /// the runtime fault site before every runtime call that can raise, so a
    /// fault with no location of its own (array bounds, a blocked channel op,
    /// an awaited cancelled task) still records `file:line:column` in its
    /// `PanicInfo` (willow-s9ej.7).
    fault_site_span: Option<crate::diagnostics::Span>,
    /// Async defer sites recorded while emitting a poll fn — consumed by the
    /// generated cancel entry.
    collected_defer_sites: Vec<AsyncDeferSite>,
    /// `lock` critical sections enclosing the code currently being emitted,
    /// outermost first (willow-38w.1.4).
    lock_scopes: Vec<CoopLockScope>,
    /// Every `lock` site seen while emitting a poll fn — consumed by the
    /// generated cancel entry, which has no lexical context of its own.
    collected_lock_sites: Vec<AsyncLockSite>,
    /// Monotonic sequence shared by `collected_defer_sites` and
    /// `collected_lock_sites`, so the cancel entry can merge them back into one
    /// reverse-lexical cleanup order (willow-38w.1.4 review).
    collected_cleanup_order: usize,
    func_ids: &'a FunctionMap<FuncId>,
    func_return_types: &'a FunctionMap<Type>,
    fn_types: &'a FunctionMap<Type>,
    func_param_modes: &'a FunctionMap<Vec<ParamMode>>,
    func_param_debug: &'a FunctionMap<Vec<ParamDebug>>,
    function_may_panic: &'a FunctionMap<bool>,
    known_modules: &'a HashMap<String, String>,
    /// The module access names the file being compiled imports (willow-vtlr).
    /// Read only to resolve a bare module class name, and read there because
    /// eligibility resolves it from the same set.
    visible_modules: &'a HashSet<String>,
    /// Local alias -> canonical builtin schema module, for the file being
    /// compiled (willow-nswv). Only the LIR path reads it.
    builtin_module_aliases: &'a HashMap<String, String>,
    lambda_names: &'a HashMap<crate::diagnostics::Span, String>,
    cooperative_leaves: &'a std::collections::HashSet<FunctionId>,
    string_literals: &'a HashMap<String, DataId>,
    class_layouts: &'a HashMap<String, Vec<(String, Type)>>,
    static_storage: &'a HashMap<(String, String), StaticStorageInfo>,
    enum_infos: &'a HashMap<String, EnumInfo>,
    class_base: &'a HashMap<String, String>,
    /// Maps class name → unique type_id (i64). Since willow-fm7t the id is no
    /// longer stored inline in the object: word 0 points at the class
    /// DESCRIPTOR, which holds the id at its own offset 0.
    class_type_ids: &'a HashMap<String, i64>,
    /// Maps class name → its descriptor data symbol, the value stored in word 0
    /// of every object of that class (willow-fm7t).
    class_descriptor_ids: &'a HashMap<String, DataId>,
    /// Per-class virtual method slot order, indexed by slot (willow-fm7t).
    class_vslots: &'a HashMap<String, Vec<String>>,
    /// Interface metadata for method dispatch + boxing.
    interface_infos: &'a HashMap<String, InterfaceInfo>,
    /// Static `(class, interface)` vtable data objects for class→interface boxing.
    vtable_ids: &'a HashMap<(String, String), DataId>,
    /// Checker-recorded types of all checked expressions (willow-mb5); the
    /// backend's primary type source.
    expr_types: &'a HashMap<crate::diagnostics::Span, Type>,
    /// When emitting a cooperative poll fn: the async frame pointer, so a
    /// `return` inside nested statement control flow (e.g. a statement-position
    /// match arm, willow-zvkv) stores the result and returns the Ready status
    /// instead of a future pointer.
    coop_frame: Option<cranelift_codegen::ir::Value>,
    /// Byte offset of the poll frame's `__result` slot, when it has one.
    coop_result_offset: Option<i32>,
    /// Spans of unqualified enum-variant constructions → resolved enum name,
    /// so the call is lowered as a variant allocation (willow-60o.1).
    enum_variant_resolutions: &'a HashMap<crate::diagnostics::Span, String>,
    /// Unqualified match-pattern spans → the enum-variant pattern they were
    /// reinterpreted as, so the arm lowers as a variant match (willow-60o.1).
    pattern_resolutions: &'a HashMap<crate::diagnostics::Span, Pattern>,
    /// Base pointer of this function's heap async frame, if one was allocated
    /// (async fns with values that must survive `await`; willow-lpn.5a).
    async_frame: Option<cranelift_codegen::ir::Value>,
    /// For an async fn with a frame: maps each GC-managed frame-backed name
    /// (param or annotated local) to its byte offset in the frame (willow-lpn.5b).
    async_frame_offsets: HashMap<crate::diagnostics::Span, i32>,
    /// LIR-owned async frame slots. This is the identity map used by the LIR
    /// poll emitter; source spans remain available only for diagnostics and
    /// the legacy AST emitter.
    lir_frame_offsets: HashMap<crate::ir::lowered::LirLocalId, i32>,
    lir_defer_offsets: HashMap<crate::ir::lowered::LirDeferId, i32>,
    /// The cooperative LIR emitter splits a value-position `await` out of its
    /// statement and parks BEFORE emitting the rest of the expression, because
    /// a Cranelift value computed ahead of the park does not survive the poll
    /// fn's return. While the statement is being emitted this holds that
    /// await's span and the value the resume produced, so the `Await` node
    /// reads back what was already awaited instead of awaiting again
    /// (willow-0g8j.2.11).
    lir_hoisted_await: Option<(crate::diagnostics::Span, cranelift_codegen::ir::Value)>,
    /// When compiling `fn main() -> Result<void, E>`: the error payload type `E`.
    /// Each return inspects the Result and exits accordingly (willow-exg).
    main_result_err_ty: Option<Type>,
    vars: HashMap<String, VarStorage>,
    return_type: Type,
    current_class: Option<&'a str>,
    is_async: bool,
    terminated: bool,
    /// Number of GC roots currently on the root stack for this function invocation.
    gc_root_count: usize,
    /// Cooperative poll functions may keep a GC local in a native stack slot
    /// when liveness proves that its value does not cross a suspension. Track
    /// those binding roots separately so every poll return can pop them and a
    /// resumed poll can register the fresh invocation's corresponding slots.
    /// `None` outside a cooperative poll function.
    coop_shadow_roots: Option<CoopShadowRoots>,
    /// Build mode for source locations, call stacks, and debug instrumentation.
    build_mode: BuildMode,
    /// Source file path used in runtime diagnostics.
    source_file: &'a str,
    /// Locals of this function whose address is taken by some `&place`
    /// argument. They are bound to a stack slot at their declaration instead of
    /// to an SSA variable, so the slot exists on every path and is written
    /// exactly where the binding says (willow-0g8j.2.17).
    address_taken: HashSet<String>,
}

#[derive(Default)]
struct CoopShadowRoots {
    /// Binding-root slots active at the current source position, in shadow-stack
    /// order. Temporary expression roots are deliberately excluded: reaching a
    /// suspension while one is active is a codegen invariant violation.
    active: Vec<cranelift_codegen::ir::StackSlot>,
    /// Every binding-root slot allocated in the poll function. Dispatch clears
    /// these slots before examining the saved state, so resume-time tracing sees
    /// null rather than stale bytes from the new native stack frame.
    all: Vec<cranelift_codegen::ir::StackSlot>,
}

struct CoopSuspendPoint {
    resume: cranelift_codegen::ir::Block,
    roots: Vec<cranelift_codegen::ir::StackSlot>,
}

#[derive(Clone)]
enum VarStorage {
    Value {
        var: Variable,
        ty: Type,
    },
    Stack {
        slot: cranelift_codegen::ir::StackSlot,
        ty: Type,
    },
    ReferencePtr {
        var: Variable,
        ty: Type,
    },
    /// A slot inside the heap async frame (willow-lpn.5a). `offset` is the byte
    /// offset of the slot from the frame base; the frame base lives in
    /// `FuncGen.async_frame`. Used for values that must survive `await`.
    Frame {
        offset: i32,
        ty: Type,
    },
}

impl VarStorage {
    fn ty(&self) -> &Type {
        match self {
            VarStorage::Value { ty, .. }
            | VarStorage::Stack { ty, .. }
            | VarStorage::ReferencePtr { ty, .. }
            | VarStorage::Frame { ty, .. } => ty,
        }
    }
}

/// Async-frame layout constants — must match `crates/willow_runtime/src/async_frame.rs`
/// (`willow_async_frame_alloc` lays out
/// `[state(word0) | slot_count(word1) | status(word2) | data slot 0..]`).
const ASYNC_FRAME_HEADER_BYTES: i32 =
    willow_abi::async_frame::header_bytes(std::mem::size_of::<usize>() as u32) as i32;

/// Byte offset of data slot `n` from the async frame base.
/// Async-task frame slot indices used with [`async_frame_slot_offset`].
/// Every async/task frame begins with these fixed slots after its header:
/// slot 0 holds the task's RESULT value, slot 1 holds its scheduler TASK ID.
const FRAME_SLOT_RESULT: usize = 0;
const FRAME_SLOT_TASK_ID: usize = 1;

/// Frame-header status word bits — must match the `WILLOW_FRAME_STATUS_*`
/// constants in `crates/willow_runtime/src/async_frame.rs` (willow-ezs.1.3).
/// Bits 0..2 are the terminal code (0 pending, 1 completed, 2 cancelled,
/// 3 panicked); higher bits carry sticky flags such as cancel-requested.
const WILLOW_FRAME_STATUS_TERMINAL_MASK: i64 = willow_abi::frame_status::TERMINAL_MASK;
const WILLOW_FRAME_STATUS_CANCELLED: i64 = willow_abi::FrameTerminalStatus::Cancelled as i64;

fn async_frame_slot_offset(n: usize) -> i32 {
    ASYNC_FRAME_HEADER_BYTES + (n as i32) * 8
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Cranelift 0.134 requires the target pointer type when lowering the
    /// implicit address calculation performed by stack-slot loads/stores.
    fn stack_load(
        &mut self,
        ty: cranelift_codegen::ir::Type,
        slot: cranelift_codegen::ir::StackSlot,
    ) -> cranelift_codegen::ir::Value {
        let ptr_ty = self.module.target_config().pointer_type();
        self.builder.ins().stack_load(ptr_ty, ty, slot, 0)
    }

    fn stack_store(
        &mut self,
        value: cranelift_codegen::ir::Value,
        slot: cranelift_codegen::ir::StackSlot,
    ) {
        let ptr_ty = self.module.target_config().pointer_type();
        self.builder.ins().stack_store(ptr_ty, value, slot, 0);
    }

    /// Look up a declared runtime/user function id by symbol name, with a clear
    /// panic if it was never declared (e.g. a backend symbol missing from
    /// `abi.rs`) instead of an opaque index-out-of-bounds.
    fn func_id(&self, name: &str) -> FuncId {
        if crate::backend::abi::runtime_symbol(name).is_some_and(|symbol| {
            symbol
                .effects()
                .contains(crate::backend::abi::RuntimeEffects::MAY_PANIC)
        }) {
            panic!(
                "backend: MAY_PANIC runtime symbol `{name}` must be emitted through the runtime-call API"
            );
        }
        *self
            .func_ids
            .get(name)
            .unwrap_or_else(|| panic!("backend: undeclared runtime symbol `{name}`"))
    }
    fn bind_param(
        &mut self,
        name: &str,
        ty: &Type,
        mode: &ParamMode,
        val: cranelift_codegen::ir::Value,
    ) {
        match mode {
            ParamMode::Value if is_gc_managed(ty, self.enum_infos) => {
                // GC-managed value parameters must live in a stack slot so the
                // GC can find and trace them during any allocation in the body.
                let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    0,
                ));
                self.stack_store(val, slot);
                let ptr_ty = self.module.target_config().pointer_type();
                let addr = self.builder.ins().stack_addr(ptr_ty, slot, 0);
                let push_id = self.func_id("willow_push_root");
                let push_ref = self.module.declare_func_in_func(push_id, self.builder.func);
                self.builder.ins().call(push_ref, &[addr]);
                self.gc_root_count += 1;
                self.vars.insert(
                    name.to_string(),
                    VarStorage::Stack {
                        slot,
                        ty: ty.clone(),
                    },
                );
            }
            ParamMode::Value if self.address_taken.contains(name) => {
                // The body passes `&name` somewhere, so the parameter needs an
                // address that exists from entry rather than one conjured at
                // whichever `&` the control flow happens to reach first.
                let storage = self.create_local_stack_slot(ty, val);
                self.vars.insert(name.to_string(), storage);
            }
            ParamMode::Value => {
                let var = self.builder.declare_var(clif_type(ty));
                self.builder.def_var(var, val);
                self.vars.insert(
                    name.to_string(),
                    VarStorage::Value {
                        var,
                        ty: ty.clone(),
                    },
                );
            }
            ParamMode::Reference { .. } => {
                let ptr_ty = self.module.target_config().pointer_type();
                let var = self.builder.declare_var(ptr_ty);
                self.builder.def_var(var, val);
                self.vars.insert(
                    name.to_string(),
                    VarStorage::ReferencePtr {
                        var,
                        ty: ty.clone(),
                    },
                );
            }
        }
    }

    /// Bind a parameter directly into an async frame slot (willow-lpn.5a): store
    /// the incoming value at `offset` and record `Frame` storage so all later
    /// reads/writes go through the heap frame, which survives `await`.
    fn bind_param_framed(
        &mut self,
        name: &str,
        ty: &Type,
        val: cranelift_codegen::ir::Value,
        offset: i32,
    ) {
        let base = self
            .async_frame
            .expect("bind_param_framed requires an allocated async frame");
        self.emit_gc_heap_store(base, offset, val, ty, GcStoreDestination::AsyncFrameSlot);
        self.vars.insert(
            name.to_string(),
            VarStorage::Frame {
                offset,
                ty: ty.clone(),
            },
        );
    }

    fn create_local_stack_slot(
        &mut self,
        ty: &Type,
        val: cranelift_codegen::ir::Value,
    ) -> VarStorage {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        self.stack_store(val, slot);
        VarStorage::Stack {
            slot,
            ty: ty.clone(),
        }
    }

    fn load_var(&mut self, storage: &VarStorage) -> cranelift_codegen::ir::Value {
        match storage {
            VarStorage::Value { var, .. } => self.builder.use_var(*var),
            VarStorage::Stack { slot, ty } => self.stack_load(clif_type(ty), *slot),
            VarStorage::ReferencePtr { var, ty } => {
                let ptr = self.builder.use_var(*var);
                self.builder
                    .ins()
                    .load(clif_type(ty), MemFlagsData::new(), ptr, 0)
            }
            VarStorage::Frame { offset, ty } => {
                let base = self
                    .async_frame
                    .expect("frame-backed var requires an allocated async frame");
                self.builder
                    .ins()
                    .load(clif_type(ty), MemFlagsData::new(), base, *offset)
            }
        }
    }

    fn store_var(&mut self, storage: &VarStorage, val: cranelift_codegen::ir::Value) {
        match storage {
            VarStorage::Value { var, .. } => self.builder.def_var(*var, val),
            VarStorage::Stack { slot, .. } => {
                self.stack_store(val, *slot);
            }
            VarStorage::ReferencePtr { var, ty } => {
                let ptr = self.builder.use_var(*var);
                self.store_indirect_reference(ptr, val, ty);
            }
            VarStorage::Frame { offset, ty } => {
                let base = self
                    .async_frame
                    .expect("frame-backed var requires an allocated async frame");
                self.emit_gc_heap_store(base, *offset, val, ty, GcStoreDestination::AsyncFrameSlot);
            }
        }
    }

    fn store_indirect_reference(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        val: cranelift_codegen::ir::Value,
        ty: &Type,
    ) {
        self.emit_gc_heap_store(ptr, 0, val, ty, GcStoreDestination::IndirectReference);
    }

    /// If `target_ty` is an interface and `value`'s static type is a class that
    /// implements it, box the value; otherwise return it unchanged. Used at the
    /// MVP coercion sites: let init, function args, return, and assignment.
    fn coerce_to_target(
        &mut self,
        value: cranelift_codegen::ir::Value,
        value_ty: &Type,
        target_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        // A canonical `Option<T>` is deliberately not unwrapped here: its
        // payload coercion happens inside explicit `Some(...)` construction.
        // The interface name comes from either a plain interface (`Animal`) or a
        // generic interface instantiation (`Box<String>`); type args do not
        // change the vtable, so boxing is identical (willow-1js.1).
        let iface_name = match target_ty {
            Type::Named(n) | Type::Generic(n, _) => n,
            _ => return value,
        };
        if !self.interface_infos.contains_key(iface_name) {
            return value;
        }
        // Already an interface value (same interface): identity.
        if let Type::Named(vn) | Type::Generic(vn, _) = value_ty
            && vn == iface_name
        {
            return value;
        }
        // Interface → SUPER-interface. The concrete class is not known here, so
        // the target vtable can only come from the source's: the layout embeds
        // each super's table as a contiguous run, and widening advances the
        // vtable pointer to that run (see `vtable_layout`). Offset 0 — the
        // single-`extends` chain, where the super's table is a plain prefix —
        // needs no new box at all. A target that is not a super leaves the value
        // alone; the checker has already rejected that program (willow-1fc6).
        if let Type::Named(vn) | Type::Generic(vn, _) = value_ty
            && self.interface_infos.contains_key(vn)
        {
            return match vtable_layout::super_offset(self.interface_infos, vn, iface_name) {
                Some(0) | None => value,
                Some(offset) => self.emit_interface_rewiden(value, offset),
            };
        }
        if let Type::Named(class_name) = value_ty
            && self.class_layouts.contains_key(class_name)
        {
            return self.emit_interface_box(value, class_name, iface_name);
        }
        value
    }

    /// The declared parameter types of a mangled function/lambda name, exactly
    /// as recorded — for a class method that INCLUDES the hidden leading
    /// `self`, so the result aligns with call arguments only for free
    /// functions. Use [`Self::method_param_types`] to align with a method
    /// call's explicit arguments. `None` if not a known function.
    fn fn_param_types(&self, mangled: &str) -> Option<Vec<Type>> {
        match self.fn_types.get(mangled) {
            Some(Type::Fn(params, _)) => Some(params.clone()),
            _ => None,
        }
    }

    /// Like [`fn_param_types`] but drops the leading `self` parameter so the
    /// result aligns with a method call's explicit arguments.
    fn method_param_types(&self, mangled: &str) -> Option<Vec<Type>> {
        match self.fn_types.get(mangled) {
            Some(Type::Fn(params, _)) if !params.is_empty() => Some(params[1..].to_vec()),
            _ => None,
        }
    }

    /// Determine the AST type of a `let` initialiser, including full `Type::Fn` for
    /// named-function and lambda values so indirect calls later work correctly.
    /// Resolve the Willow AST type of an expression, handling FieldAccess and
    /// MethodCall by looking up class layouts and func_return_types.
    fn ast_type_of(&self, expr: &Expr) -> Type {
        // The checker's recorded type is authoritative (willow-mb5); the
        // structural walk below only types compiler-synthesized expressions
        // whose spans the checker never saw.
        if let Some(ty) = self.expr_types.get(&expr.span()) {
            return ty.clone();
        }
        self.ast_type_of_structural(expr)
    }

    fn ast_type_of_structural(&self, expr: &Expr) -> Type {
        match expr {
            // Static property read → its declared type (willow-qsqf), so e.g.
            // `println(C::prop)` selects the right print function.
            Expr::StaticField(s) => {
                let class = self.static_call_class_name(&s.class);
                self.lookup_static_storage(&class, &s.field)
                    .map(|info| info.ty)
                    .unwrap_or(Type::I64)
            }
            Expr::FieldAccess(obj, field_name, _) => {
                if let Some(class_name) = class_name_for_object_type(&self.ast_type_of(obj))
                    && let Some(layout) = self.class_layouts.get(&class_name)
                    && let Some((_, ty)) = layout.iter().find(|(n, _)| n == field_name)
                {
                    return ty.clone();
                }
                Type::I64
            }
            Expr::MethodCall(m) => {
                let obj_ty = self.ast_type_of(&m.object);
                // Builtin methods resolve to an intrinsic identity and a result
                // type in one table (willow-uqzx, catalog item 7). This walk
                // used to repeat the emitter's string waterfall by hand and had
                // already drifted from it: it knew `Array::freeze` but not
                // `Array::toString`, answered `void` for `Map::toString` through
                // a catch-all arm, and did not know `Task::result` or any
                // `CancellationToken`/`TaskScope` method at all — those all fell
                // through to the `i64` default at the end of this arm.
                if let Some(resolved) = intrinsics::resolve(&obj_ty, &m.method, m.args.len()) {
                    return resolved
                        .return_type(|i| m.args.get(i).map(|arg| self.ast_type_of(&arg.expr)));
                }
                if let Some(ret) = option_result_method_return_type(
                    &obj_ty,
                    &m.method,
                    m.args
                        .first()
                        .map(|a| self.ast_type_of_init(&a.expr))
                        .as_ref(),
                ) {
                    return ret;
                }
                // Interface method call → the interface method's return type.
                if let Type::Named(iface_name) = &obj_ty
                    && let Some(iface) = self.interface_infos.get(iface_name)
                    && let Some(method) = iface.methods.get(&m.method)
                {
                    return method.return_type.clone();
                }
                // Generic interface receiver (`Box<String>`): substitute the
                // interface's type parameters into the method's return type
                // (`fn get(self) -> T` -> `String`) (willow-1js.1).
                if let Type::Generic(iface_name, type_args) = &obj_ty
                    && let Some(iface) = self.interface_infos.get(iface_name)
                    && let Some(method) = iface.methods.get(&m.method)
                {
                    let map: HashMap<String, Type> = iface
                        .type_params
                        .iter()
                        .cloned()
                        .zip(type_args.iter().cloned())
                        .collect();
                    return crate::semantic::symbols::substitute_type(&method.return_type, &map);
                }
                if let Some(class_name) = class_name_for_object_type(&obj_ty) {
                    // Walk hierarchy to find the method return type.
                    let mut search = Some(class_name.clone());
                    let mut seen = std::collections::HashSet::new();
                    while let Some(name) = search {
                        if !seen.insert(name.clone()) {
                            break;
                        }
                        let mangled =
                            class_method_symbol_name(self.known_modules, &name, &m.method);
                        if let Some(ty) = self.func_return_types.get(&mangled) {
                            return ty.clone();
                        }
                        search = self.class_base.get(&name).cloned();
                    }
                }
                Type::I64
            }
            Expr::Binary(b) => match &b.op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem | BinOp::Pow => {
                    self.ast_type_of(&b.lhs)
                }
                _ => Type::Bool,
            },
            Expr::Unary(u) => match &u.op {
                UnaryOp::Neg => self.ast_type_of(&u.expr),
                UnaryOp::Not => Type::Bool,
            },
            // Generic enum constructor: infer the concrete instantiated type using enum_infos.
            Expr::StaticCall(s) => {
                let class_name = self.static_call_class_name(&s.class);
                if let Some(enum_info) = self.enum_infos.get(class_name.as_str())
                    && !enum_info.type_params.is_empty()
                    && let Some(variant) = enum_info.variants.iter().find(|v| v.name == s.method)
                {
                    // Infer type args: for each type parameter, find which payload position
                    // uses it and take the type of the corresponding argument.
                    let type_args: Vec<Type> = enum_info
                        .type_params
                        .iter()
                        .map(|param| {
                            variant
                                .payload_types
                                .iter()
                                .zip(s.args.iter())
                                .find_map(|(payload_ty, arg)| {
                                    if matches!(payload_ty, Type::Named(n) if n == param) {
                                        Some(self.ast_type_of(&arg.expr))
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or(Type::Void)
                        })
                        .collect();
                    return Type::Generic(class_name.clone(), type_args);
                }
                // `Mutex::new(v)` / `RwLock::new(v)` / `BlockingCell::new(v)`:
                // element type is the explicit type argument or, when omitted,
                // inferred from the argument (willow-dgwo.3).
                if matches!(
                    class_name.as_str(),
                    "Mutex" | "RwLock" | "BlockingCell" | "BlockingRwCell"
                ) && s.method == "new"
                {
                    let elem = s.type_args.first().cloned().unwrap_or_else(|| {
                        s.args
                            .first()
                            .map(|a| self.ast_type_of(&a.expr))
                            .unwrap_or(Type::Void)
                    });
                    return Type::Generic(class_name.clone(), vec![elem]);
                }
                if let Some(ty) = builtin_static_return_type(&class_name, &s.type_args, &s.method) {
                    return ty;
                }
                if let Some(module_prefix) = self.known_modules.get(&class_name) {
                    let mangled = module_item_symbol(module_prefix, &s.method);
                    if let Some(ty) = self.func_return_types.get(&mangled) {
                        return ty.clone();
                    }
                }
                let mangled = class_method_symbol_name(self.known_modules, &class_name, &s.method);
                if let Some(ty) = self.func_return_types.get(&mangled) {
                    return ty.clone();
                }
                ast_type_of_expr(expr, &self.vars, self.func_return_types, self.expr_types)
            }
            Expr::Await(a) => task_output_type(&self.ast_type_of(&a.expr))
                .or_else(|| future_output_type(&self.ast_type_of(&a.expr)))
                .unwrap_or_else(|| self.ast_type_of(&a.expr)),
            _ => ast_type_of_expr(expr, &self.vars, self.func_return_types, self.expr_types),
        }
    }

    fn ast_type_of_init(&self, expr: &Expr) -> Type {
        if let Some(ty) = self.expr_types.get(&expr.span()) {
            return ty.clone();
        }
        self.ast_type_of_init_structural(expr)
    }

    fn ast_type_of_init_structural(&self, expr: &Expr) -> Type {
        match expr {
            // Static property read → its declared type (so `let x = C::prop`
            // gets the right storage clif type), willow-qsqf.
            Expr::StaticField(s) => {
                let class = self.static_call_class_name(&s.class);
                self.lookup_static_storage(&class, &s.field)
                    .map(|info| info.ty)
                    .unwrap_or(Type::Void)
            }
            // Named function used as a value → look up its full fn type.
            Expr::Var(name, _) => {
                if let Some(ty) = self.fn_types.get(name.as_str()) {
                    return ty.clone();
                }
                self.ast_type_of(expr)
            }
            // Lambda expression → build the fn type from params and return type.
            // Prefer: explicit annotation > type-checker inferred > expression-body inference > I64.
            Expr::Lambda(l) => {
                if let Some(ty @ Type::Fn(..)) = self.expr_types.get(&l.span) {
                    return ty.clone();
                }
                let params: Vec<Type> = l.params.iter().filter_map(|p| p.ty.clone()).collect();
                let ret = l.return_type.clone().unwrap_or_else(|| {
                    if let crate::parser::ast::LambdaBody::Expr(e) = &l.body {
                        let param_map: HashMap<String, Type> = l
                            .params
                            .iter()
                            .filter_map(|p| p.ty.clone().map(|ty| (p.name.clone(), ty)))
                            .collect();
                        infer_lambda_body_type(e, &param_map, self.func_return_types)
                    } else {
                        Type::I64
                    }
                });
                Type::Fn(params, Box::new(ret))
            }
            _ => self.ast_type_of(expr),
        }
    }

    fn static_call_class_name(&self, class_name: &str) -> String {
        if class_name == "Self" {
            self.current_class.unwrap_or(class_name).to_string()
        } else {
            class_name.to_string()
        }
    }

    /// Convert a raw i64 word back to the appropriate CLIF value for the given type.
    fn coerce_i64_to(
        &mut self,
        raw: cranelift_codegen::ir::Value,
        ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        match ty {
            Type::F64 => self
                .builder
                .ins()
                .bitcast(types::F64, MemFlagsData::new(), raw),
            Type::Bool => self.builder.ins().ireduce(types::I8, raw),
            _ => raw,
        }
    }

    /// Convert a CLIF value of the given type to a raw i64 word (inverse of
    /// [`coerce_i64_to`]). Used to store array elements through the uniform
    /// 64-bit-word array ABI.
    fn coerce_to_i64(
        &mut self,
        val: cranelift_codegen::ir::Value,
        ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        match ty {
            Type::F64 => self
                .builder
                .ins()
                .bitcast(types::I64, MemFlagsData::new(), val),
            Type::Bool => self.builder.ins().uextend(types::I64, val),
            _ => val,
        }
    }

    /// True when `cls` is `ancestor` or transitively extends it.
    fn class_is_a(&self, cls: &str, ancestor: &str) -> bool {
        let mut current = Some(cls.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if name == ancestor {
                return true;
            }
            if !seen.insert(name.clone()) {
                break;
            }
            current = self.class_base.get(&name).cloned();
        }
        false
    }

    /// FuncId of `cls`'s (or the nearest ancestor's) `method`, or `None`.
    fn resolve_method_func_id(&self, cls: &str, method: &str) -> Option<FuncId> {
        let mut current = Some(cls.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                break;
            }
            let mangled = class_method_symbol_name(self.known_modules, &name, method);
            if let Some(&fid) = self.func_ids.get(&mangled) {
                return Some(fid);
            }
            current = self.class_base.get(&name).cloned();
        }
        None
    }

    /// Resolve the concrete payload types for an enum variant.
    /// For generic enums, substitutes type arguments from the scrutinee type.
    fn resolve_variant_payload_types(
        &self,
        enum_name: &str,
        variant: &str,
        scrutinee_ty: &Type,
    ) -> Vec<Type> {
        let Some(enum_info) = self.enum_infos.get(enum_name) else {
            return vec![];
        };
        // Instantiate with type args from the scrutinee if available.
        let type_args: &[Type] = if let Type::Generic(n, args) = scrutinee_ty {
            if n == enum_name { args.as_slice() } else { &[] }
        } else {
            &[]
        };
        let concrete = if enum_info.type_params.is_empty() || type_args.is_empty() {
            enum_info.clone()
        } else {
            enum_info.instantiate(type_args)
        };
        concrete
            .variants
            .iter()
            .find(|v| v.name == variant)
            .map(|v| v.payload_types.clone())
            .unwrap_or_default()
    }

    fn enum_variant_tag(&self, enum_name: &str, variant: &str) -> i64 {
        self.enum_infos
            .get(enum_name)
            .and_then(|e| e.variants.iter().find(|v| v.name == variant))
            .map(|v| v.tag)
            .unwrap_or(0)
    }

    fn enum_is_gc_object_type(&self, enum_name: &str) -> bool {
        self.enum_infos
            .get(enum_name)
            .map(|e| e.variants.iter().any(|v| !v.payload_types.is_empty()))
            .unwrap_or(false)
    }

    /// Load a `ClassName::property` static value from its global storage
    /// (willow-qsqf §13.4). The slot holds 8 bytes; the clif type comes from the
    /// property's declared type.
    /// Find a static property's storage, walking the class hierarchy so an
    /// inherited static (`Child::prop` declared on `Base`) resolves to the
    /// declaring class (willow-qsqf §16.2). Static members are non-virtual.
    fn lookup_static_storage(&self, class: &str, field: &str) -> Option<StaticStorageInfo> {
        lookup_static_storage_in(self.static_storage, self.class_base, class, field)
    }
}

/// [`FuncGen::lookup_static_storage`] over the raw tables, so LIR eligibility
/// can ask the same question before a `FuncGen` exists. One walk, two callers:
/// the walker must not admit a read the emitter would then resolve differently.
fn lookup_static_storage_in(
    static_storage: &HashMap<(String, String), StaticStorageInfo>,
    class_base: &HashMap<String, String>,
    class: &str,
    field: &str,
) -> Option<StaticStorageInfo> {
    let mut current = Some(class.to_string());
    let mut seen = std::collections::HashSet::new();
    while let Some(name) = current {
        if !seen.insert(name.clone()) {
            break;
        }
        if let Some(info) = static_storage.get(&(name.clone(), field.to_string())) {
            return Some(info.clone());
        }
        current = class_base.get(&name).cloned();
    }
    None
}

fn fcmp_to_i8(
    builder: &mut FunctionBuilder<'_>,
    cc: FloatCC,
    lhs: cranelift_codegen::ir::Value,
    rhs: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    builder.ins().fcmp(cc, lhs, rhs)
}

fn icmp_to_i8(
    builder: &mut FunctionBuilder<'_>,
    cc: IntCC,
    lhs: cranelift_codegen::ir::Value,
    rhs: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    // icmp returns I8 in cranelift 0.132
    builder.ins().icmp(cc, lhs, rhs)
}

fn function_call_return_type(f: &FunctionDecl) -> Type {
    if f.is_async {
        Type::Generic("Task".to_string(), vec![f.return_type.clone()])
    } else {
        f.return_type.clone()
    }
}

fn method_call_return_type(m: &MethodDecl) -> Type {
    if m.is_async {
        Type::Generic("Task".to_string(), vec![m.return_type.clone()])
    } else {
        m.return_type.clone()
    }
}

fn param_debug_from_params(params: &[Param]) -> Vec<ParamDebug> {
    params
        .iter()
        .map(|param| ParamDebug {
            name: param.name.clone(),
            ty: param.ty.clone(),
            mode: param.mode.clone(),
        })
        .collect()
}

fn has_reference_args(modes: Option<&[ParamMode]>, args: &[CallArg]) -> bool {
    args.iter().enumerate().any(|(idx, arg)| {
        matches!(
            (modes.and_then(|modes| modes.get(idx)), &arg.mode),
            (
                Some(ParamMode::Reference { .. }),
                CallArgMode::Reference { .. }
            )
        )
    })
}

fn reference_mode_name(mode: &ParamMode) -> &'static str {
    match mode {
        ParamMode::Reference { mutable: true, .. } => "&mut",
        ParamMode::Reference { mutable: false, .. } => "&",
        ParamMode::Value => "value",
    }
}

fn reference_place_kind(expr: &Expr) -> &'static str {
    match expr {
        Expr::Var(_, _) => "local",
        Expr::FieldAccess(_, _, _) => "field",
        Expr::Index(_, _, _) => "array_element",
        _ => "expression",
    }
}

fn reference_place_name(expr: &Expr) -> String {
    match expr {
        Expr::Var(name, _) => name.clone(),
        Expr::FieldAccess(object, field, _) => {
            format!("{}.{}", reference_place_name(object), field)
        }
        Expr::Index(array, index, _) => {
            format!(
                "{}[{}]",
                reference_place_name(array),
                reference_index_name(index)
            )
        }
        _ => "<expression>".to_string(),
    }
}

fn reference_index_name(expr: &Expr) -> String {
    match expr {
        Expr::Integer(value, _) => value.to_string(),
        Expr::Var(name, _) => name.clone(),
        _ => "<expr>".to_string(),
    }
}

fn range_type() -> Type {
    Type::Generic("Range".to_string(), vec![Type::I64])
}

fn channel_runtime_suffix(ty: &Type) -> &'static str {
    match ty {
        Type::I64 => "i64",
        Type::Bool => "bool",
        Type::F64 => "f64",
        _ => "ptr",
    }
}

fn param_abi_type(
    param: &Param,
    pointer_type: cranelift_codegen::ir::Type,
) -> cranelift_codegen::ir::Type {
    match &param.mode {
        ParamMode::Reference { .. } => pointer_type,
        ParamMode::Value => clif_type(&param.ty),
    }
}

fn gc_ref_mask_for_layout(
    class_name: &str,
    layout: &[(String, Type)],
    enum_infos: &HashMap<String, EnumInfo>,
) -> u64 {
    try_gc_ref_mask_for_layout(class_name, layout, enum_infos)
        .expect("class GC ref mask layout should have been validated before codegen")
}

fn try_gc_ref_mask_for_layout(
    class_name: &str,
    layout: &[(String, Type)],
    enum_infos: &HashMap<String, EnumInfo>,
) -> Result<u64> {
    // Object layout: word 0 = the class DESCRIPTOR address, words 1..N = fields.
    // Bit i in the mask corresponds to word i; field[idx] lives at word (idx+1).
    //
    // Word 0 is never a GC reference. It used to hold the inline `type_id` and
    // now holds a pointer to a static data symbol (willow-fm7t) — still not a
    // heap object, so bit 0 stays clear and the collector's view of the payload
    // is unchanged. Nothing about the field offsets or the mask moved with it.
    let mut mask = 0u64;
    for (idx, (field_name, ty)) in layout.iter().enumerate() {
        if !is_gc_managed(ty, enum_infos) {
            continue;
        }
        let word = idx + 1;
        if word >= GC_REF_MASK_BITS {
            bail!(
                "class `{class_name}` field `{field_name}` is a GC-managed reference at payload word {word}, outside gc_ref_mask coverage; word 0 is the class descriptor, so only the first {OBJECT_FIELD_MASK_CAPACITY} fields can be represented without a trace function"
            );
        }
        mask |= 1u64 << word;
    }
    Ok(mask)
}

// ─── Async frame GC metadata (willow-lpn.4) ──────────────────────────────────
//
// An `async fn` whose locals are live across an `await` must spill them into a
// heap-allocated frame (see requirements/willow_async_gc_requirements.md §6–7).
// The runtime frame allocator `willow_async_frame_alloc(slot_count, gc_slot_mask)`
// (crates/willow_runtime/src/async_frame.rs) was built by Stage 3 (willow-lpn.3);
// it lays out `[state | slot_count | status | data slot 0 | data slot 1 | …]`
// and shifts `gc_slot_mask` past the 3-word header internally. This stage is the compiler
// side: compute, for an async fn, the ordered data-slot layout and the GC
// reference mask the runtime needs to trace only the heap-reference slots.
//
// Slot-emission, live-across-await selection, and the suspend/resume state
// machine are Stage 5 (willow-lpn.5); it consumes `AsyncFrameLayout`. Here the
// mask computation is exact and the slot collector is the conservative initial
// layout (parameters + annotated `let` locals).

/// One data slot of an async fn's heap frame (excludes the fixed
/// `state`/`slot_count`/`status` header words, which are never GC references).
#[allow(dead_code)] // Consumed by willow-lpn.5 (async frame emission + state machine).
#[derive(Debug, Clone, PartialEq)]
pub struct AsyncFrameSlot {
    /// Unique key for this binding — the declaration span of the param or `let`.
    /// Frame offsets are keyed by this (NOT the name) so that two same-named
    /// locals in nested scopes get distinct slots (willow-lpn.11).
    pub key: crate::diagnostics::Span,
    pub name: String,
    pub ty: Type,
}

/// GC trace metadata for an async fn frame: the data-slot layout plus the GC
/// reference mask consumed by `willow_async_frame_alloc`. Bit K of
/// `gc_slot_mask` is set iff data slot K holds a GC-managed heap reference.
#[allow(dead_code)] // Consumed by willow-lpn.5 (async frame emission + state machine).
#[derive(Debug, Clone, PartialEq)]
pub struct AsyncFrameLayout {
    pub slots: Vec<AsyncFrameSlot>,
    pub gc_slot_mask: u64,
}

#[allow(dead_code)] // Consumed by willow-lpn.5 (async frame emission + state machine).
impl AsyncFrameLayout {
    /// Build a layout from ordered slots, computing the GC reference mask.
    ///
    /// A slot is a GC reference exactly when `is_gc_managed` is true for its
    /// type, so the same predicate governs frame tracing, shadow-stack rooting,
    /// and object-field masks. In particular: class references, strings,
    /// arrays, with-payload (and generic) enums, and `T?` wrapping any of those
    /// are traced; `i64`/`f64`/`bool`/`void`, fieldless enums (immediate tags),
    /// and `T?` of a primitive are not. Channel/Future are opaque runtime
    /// pointers without a `GcHeader`, so they are NOT marked traceable here
    /// either (tracing them would crash the collector, see willow-lpn.9);
    /// JoinHandle is represented as a GC async-frame pointer and is traceable.
    pub fn new(slots: Vec<AsyncFrameSlot>, enum_infos: &HashMap<String, EnumInfo>) -> Self {
        Self::try_new(slots, enum_infos).unwrap_or_else(|err| panic!("{err}"))
    }

    pub fn try_new(
        slots: Vec<AsyncFrameSlot>,
        enum_infos: &HashMap<String, EnumInfo>,
    ) -> Result<Self> {
        for (k, slot) in slots.iter().enumerate() {
            if k >= ASYNC_FRAME_GC_SLOT_CAPACITY && is_gc_managed(&slot.ty, enum_infos) {
                bail!(
                    "async frame slot `{}` is a GC-managed reference at data slot {k}, outside gc_ref_mask coverage; the runtime frame header uses {ASYNC_FRAME_HEADER_WORDS} payload words, so only the first {ASYNC_FRAME_GC_SLOT_CAPACITY} GC-managed data slots can be represented without a trace function",
                    slot.name
                );
            }
        }
        let gc_slot_mask = slots
            .iter()
            .take(ASYNC_FRAME_GC_SLOT_CAPACITY)
            .enumerate()
            .fold(0u64, |mask, (k, slot)| {
                if is_gc_managed(&slot.ty, enum_infos) {
                    mask | (1u64 << k)
                } else {
                    mask
                }
            });
        Ok(Self {
            slots,
            gc_slot_mask,
        })
    }

    /// Number of data slots (the `slot_count` argument to the runtime allocator).
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Whether data slot `k` holds a GC-managed heap reference.
    pub fn slot_is_gc_ref(&self, k: usize) -> bool {
        k < 64 && (self.gc_slot_mask & (1u64 << k)) != 0
    }
}

/// Collect the conservative initial frame slots for an async fn: parameters in
/// declaration order, then `let`-bound locals discovered by walking the body
/// (including nested `if`/`while` blocks) in source order, deduplicated by the
/// binding's declaration span so shadowed same-name locals get distinct slots.
///
/// Locals whose type is only known by inference (no annotation) are skipped
/// here; Stage 5 (willow-lpn.5) supplies resolved types and the precise
/// live-across-await subset when it emits the frame. The GC reference mask
/// produced from these slots is exact for whatever slots are included.
#[allow(dead_code)] // Consumed by willow-lpn.5 (async frame emission + state machine).
fn collect_async_frame_slots(params: &[Param], body: &Block) -> Vec<AsyncFrameSlot> {
    let mut slots: Vec<AsyncFrameSlot> = params
        .iter()
        .map(|p| AsyncFrameSlot {
            key: p.span,
            name: p.name.clone(),
            ty: p.ty.clone(),
        })
        .collect();
    let mut seen: HashSet<crate::diagnostics::Span> = slots.iter().map(|s| s.key).collect();
    collect_let_slots(body, &mut slots, &mut seen);
    slots
}

/// Walk a block collecting annotated `let` locals into `out` (deduped by span).
#[allow(dead_code)] // Consumed by willow-lpn.5 (async frame emission + state machine).
fn collect_let_slots(
    block: &Block,
    out: &mut Vec<AsyncFrameSlot>,
    seen: &mut HashSet<crate::diagnostics::Span>,
) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Let(l) => {
                if let Some(ty) = &l.ty
                    && seen.insert(l.span)
                {
                    out.push(AsyncFrameSlot {
                        key: l.span,
                        name: l.name.clone(),
                        ty: ty.clone(),
                    });
                }
            }
            Stmt::If(s) => {
                collect_let_slots(&s.then_block, out, seen);
                if let Some(else_block) = &s.else_block {
                    collect_let_slots(else_block, out, seen);
                }
            }
            Stmt::While(s) => collect_let_slots(&s.body, out, seen),
            Stmt::For(s) => collect_let_slots(&s.body, out, seen),
            _ => {}
        }
    }
}

/// The element type of an `Array<T>` or `Range<i64>`, or `Void` for any other
/// type (a recovery path after a type error).
fn array_element_type(ty: &Type) -> Type {
    match ty {
        Type::Array(elem) => (**elem).clone(),
        // FrozenArray<T> indexing yields T (willow-dgwo.7).
        Type::Generic(_, _) if builtin_types::unary_arg(ty, B::FrozenArray).is_some() => {
            builtin_types::unary_arg(ty, B::FrozenArray)
                .unwrap()
                .clone()
        }
        Type::Generic(name, args) if name == "Range" && args.as_slice() == [Type::I64] => Type::I64,
        _ => Type::Void,
    }
}

fn try_propagate_payload_type(ty: &Type) -> Type {
    builtin_types::resolve(ty)
        .filter(|resolved| matches!(resolved.id, B::Result | B::Option))
        .and_then(|resolved| resolved.args.first().cloned())
        .unwrap_or(Type::I64)
}

/// The error type `E` of a `Result<T, E>`, used by `?` automatic error
/// conversion (willow-1ow).
fn result_err_type(ty: &Type) -> Option<Type> {
    builtin_types::binary_args(ty, B::Result).map(|(_, err)| err.clone())
}

/// The error payload type `E` if `f` returns `Result<void, E>`, else `None`.
/// Such a function (when it is `main`) lowers to a void `willow_user_main` that
/// inspects the result and exits accordingly (willow-exg).
fn main_result_err_type(f: &FunctionDecl) -> Option<Type> {
    builtin_types::binary_args(&f.return_type, B::Result)
        .filter(|(ok, _)| **ok == Type::Void)
        .map(|(_, err)| err.clone())
}

fn ast_type_of_expr(
    expr: &Expr,
    vars: &HashMap<String, VarStorage>,
    frt: &FunctionMap<Type>,
    et: &HashMap<crate::diagnostics::Span, Type>,
) -> Type {
    // Checker-recorded types are authoritative (willow-mb5); fall back to the
    // structural walk only for unrecorded (synthesized) expressions.
    if let Some(ty) = et.get(&expr.span()) {
        return ty.clone();
    }
    ast_type_of_expr_structural(expr, vars, frt, et)
}

fn ast_type_of_expr_structural(
    expr: &Expr,
    vars: &HashMap<String, VarStorage>,
    frt: &FunctionMap<Type>,
    et: &HashMap<crate::diagnostics::Span, Type>,
) -> Type {
    match expr {
        Expr::Integer(_, _) => Type::I64,
        Expr::Float(_, _) => Type::F64,
        Expr::Bool(_, _) => Type::Bool,
        Expr::String(_, _) => Type::String,
        Expr::Var(name, _) => vars
            .get(name.as_str())
            .map(|storage| storage.ty().clone())
            .unwrap_or(Type::I64),
        Expr::Binary(b) => match &b.op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem | BinOp::Pow => {
                ast_type_of_expr(&b.lhs, vars, frt, et)
            }
            _ => Type::Bool,
        },
        Expr::Unary(u) => match &u.op {
            UnaryOp::Neg => ast_type_of_expr(&u.expr, vars, frt, et),
            UnaryOp::Not => Type::Bool,
        },
        Expr::Call(c) => frt
            .get(&c.callee)
            .cloned()
            .or_else(|| builtin_call_return_type(&c.callee))
            .unwrap_or(Type::I64),
        Expr::Print(_, _, _) => Type::Void,
        Expr::Ternary(t) => ast_type_of_ternary(t, vars, frt, et),
        Expr::Range(_) => range_type(),
        Expr::Lambda(l) => {
            let params = l
                .params
                .iter()
                .filter_map(|p| p.ty.clone())
                .collect::<Vec<_>>();
            let ret = l.return_type.clone().unwrap_or(Type::I64);
            Type::Fn(params, Box::new(ret))
        }
        Expr::FieldAccess(_, _, _) => Type::Void,
        // Static property type is resolved via FuncGen's static_storage in
        // `ast_type_of_init`; this free function lacks that context.
        Expr::StaticField(_) => Type::Void,
        Expr::MethodCall(m) => {
            let obj_ty = ast_type_of_expr(&m.object, vars, frt, et);
            if m.method == "recv"
                && let Some(element_ty) = channel_element_type(&obj_ty)
            {
                return element_ty;
            }
            if let Type::Array(elem) = &obj_ty {
                match m.method.as_str() {
                    "len" => return Type::I64,
                    "pop" => return (**elem).clone(),
                    "push" => return Type::Void,
                    "freeze" => return B::FrozenArray.apply(vec![(**elem).clone()]),
                    _ => {}
                }
            }
            if builtin_types::unary_arg(&obj_ty, B::FrozenArray).is_some() && m.method == "len" {
                return Type::I64;
            }
            if let Type::Generic(_, margs) = &obj_ty {
                if builtin_types::binary_args(&obj_ty, B::Map).is_some() {
                    match m.method.as_str() {
                        "get" => {
                            return B::Option.apply(vec![margs[1].clone()]);
                        }
                        "len" => return Type::I64,
                        "contains" => return Type::Bool,
                        "freeze" => return B::FrozenMap.apply(margs.clone()),
                        _ => return Type::Void,
                    }
                }
                if builtin_types::binary_args(&obj_ty, B::FrozenMap).is_some() {
                    match m.method.as_str() {
                        "get" => {
                            return B::Option.apply(vec![margs[1].clone()]);
                        }
                        "contains" => return Type::Bool,
                        "len" => return Type::I64,
                        _ => return Type::Void,
                    }
                }
            }
            Type::Void
        }
        Expr::ObjectLiteral(o) => Type::Named(o.class.clone()),
        Expr::New(n) => Type::Named(n.class_name.clone()),
        Expr::Await(a) => task_output_type(&ast_type_of_expr(&a.expr, vars, frt, et))
            .or_else(|| future_output_type(&ast_type_of_expr(&a.expr, vars, frt, et)))
            .unwrap_or_else(|| ast_type_of_expr(&a.expr, vars, frt, et)),
        Expr::Select(_) => Type::Void,
        Expr::StaticCall(s) => {
            if let Some(ty) = builtin_static_return_type(&s.class, &s.type_args, &s.method) {
                return ty;
            }
            // Look up mangled name for module calls.
            let mangled = class_member_symbol(&backend_symbol_component(&s.class), &s.method);
            frt.get(&mangled)
                .or_else(|| frt.get(&s.method))
                .cloned()
                .unwrap_or(Type::I64)
        }
        Expr::Match(m) => {
            // Build augmented var map: include payload bindings from each arm
            // so that `v` in `Option::Some(v) => v` resolves to the correct type.
            let scrutinee_ty = ast_type_of_expr(&m.scrutinee, vars, frt, et);
            for arm in &m.arms {
                // Build a temporary augmented scope for this arm's bindings.
                let mut arm_vars = vars.clone();
                if let Pattern::EnumVariantTuple {
                    enum_name,
                    variant,
                    bindings,
                    ..
                } = &arm.pattern
                {
                    // Derive payload types from the scrutinee's generic type arguments.
                    // This is a positional heuristic: first arg → first payload, etc.
                    // Works correctly for Option<T> (single param) and Result<T,E> (two params).
                    let payload: Vec<Type> =
                        infer_generic_payload_from_scrutinee(enum_name, variant, &scrutinee_ty);
                    for (name, ty) in bindings.iter().zip(payload.iter()) {
                        arm_vars.insert(
                            name.clone(),
                            VarStorage::Value {
                                var: Variable::from_u32(0), // placeholder — ty() is the only field read here
                                ty: ty.clone(),
                            },
                        );
                    }
                }
                let ty = match &arm.body {
                    MatchBody::Expr(e) => ast_type_of_expr(e, &arm_vars, frt, et),
                    MatchBody::Block(_) => Type::Void,
                };
                if ty != Type::Void && ty != Type::Never {
                    return ty;
                }
            }
            Type::I64
        }
        Expr::TryPropagate(inner, _) => {
            // ? extracts the Ok/Some payload from Result<T,E> or Option<T> → type T
            let inner_ty = ast_type_of_expr(inner, vars, frt, et);
            if let Some(payload) = builtin_types::unary_arg(&inner_ty, B::Option) {
                return payload.clone();
            }
            if let Some((payload, _)) = builtin_types::binary_args(&inner_ty, B::Result) {
                return payload.clone();
            }
            Type::I64
        }
        Expr::ArrayLiteral(elements, _) => {
            let elem = elements
                .first()
                .map(|e| ast_type_of_expr(e, vars, frt, et))
                .unwrap_or(Type::Void);
            Type::Array(Box::new(elem))
        }
        Expr::Index(arr, _, _) => match ast_type_of_expr(arr, vars, frt, et) {
            Type::Array(elem) => *elem,
            ty @ Type::Generic(_, _) => builtin_types::unary_arg(&ty, B::FrozenArray)
                .cloned()
                .unwrap_or(Type::I64),
            _ => Type::I64,
        },
    }
}

fn ast_type_of_ternary(
    t: &TernaryExpr,
    vars: &HashMap<String, VarStorage>,
    frt: &FunctionMap<Type>,
    et: &HashMap<crate::diagnostics::Span, Type>,
) -> Type {
    let then_ty = ast_type_of_expr(&t.then_expr, vars, frt, et);
    let else_ty = ast_type_of_expr(&t.else_expr, vars, frt, et);

    if then_ty == else_ty {
        return then_ty;
    }

    then_ty
}

/// Infer the concrete payload types for a generic enum variant from the scrutinee type.
/// This is used in `ast_type_of_expr` where `enum_infos` is not available.
///
/// Works positionally: the first scrutinee type argument maps to the first payload
/// element, the second to the second, etc. This is correct for Option<T> and Result<T,E>.
fn infer_generic_payload_from_scrutinee(
    enum_name: &str,
    variant: &str,
    scrutinee_ty: &Type,
) -> Vec<Type> {
    let (name, args) = match scrutinee_ty {
        Type::Generic(n, a) if n == enum_name => (n.as_str(), a.as_slice()),
        _ => return vec![],
    };
    let _ = name;
    // Heuristic mapping based on variant position:
    // - Variants with a single payload use the type arg at the same enum-level position.
    // We don't have the enum definition here, so we use a simple rule:
    //   first variant with payload → first type arg
    //   second variant with payload → second type arg (if it exists)
    // For Option<T>: Some(T) → [args[0]], None → []
    // For Result<T,E>: Ok(T) → [args[0]], Err(E) → [args[1]]
    // We detect "second variant" by checking if variant is "Err" or the name ends with 2.
    // This is intentionally simple; proper generic instantiation uses enum_infos.
    match (enum_name, variant) {
        (_, "None") => vec![],
        (_, "Ok") | (_, "Some") => args.first().map(|t| vec![t.clone()]).unwrap_or_default(),
        (_, "Err") => args.get(1).map(|t| vec![t.clone()]).unwrap_or_default(),
        _ => {
            // Generic fallback: single arg with first type param
            args.first().map(|t| vec![t.clone()]).unwrap_or_default()
        }
    }
}

/// Infer the return type of a lambda body expression without needing the full
/// VarStorage context. Only handles simple cases; falls back to I64 for complex ones.
fn infer_lambda_body_type(
    expr: &Expr,
    param_types: &HashMap<String, Type>,
    frt: &FunctionMap<Type>,
) -> Type {
    match expr {
        Expr::Integer(_, _) => Type::I64,
        Expr::Float(_, _) => Type::F64,
        Expr::Bool(_, _) => Type::Bool,
        Expr::String(_, _) => Type::String,
        Expr::Var(name, _) => param_types.get(name.as_str()).cloned().unwrap_or(Type::I64),
        Expr::Binary(b) => match &b.op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem | BinOp::Pow => {
                infer_lambda_body_type(&b.lhs, param_types, frt)
            }
            _ => Type::Bool,
        },
        Expr::Unary(u) => match &u.op {
            UnaryOp::Neg => infer_lambda_body_type(&u.expr, param_types, frt),
            UnaryOp::Not => Type::Bool,
        },
        Expr::Call(c) => frt
            .get(&c.callee)
            .cloned()
            .or_else(|| builtin_call_return_type(&c.callee))
            .unwrap_or(Type::I64),
        Expr::Ternary(t) => infer_lambda_body_type(&t.then_expr, param_types, frt),
        _ => Type::I64,
    }
}

/// Compute the return type of an Option/Result method call without requiring
/// full type-checker context. Used by the backend's ast_type_of for MethodCall.
///
/// For higher-order methods (map, and_then, etc.) whose return type depends on the
/// function argument type: if the function argument type is not a Generic (i.e. it's a
/// bare I64 because the lambda has no explicit return annotation), fall back to the
/// receiver type. This is correct when the element type is preserved (common case) and
/// at least tracks the value as Option/Result rather than a bare I64.
fn option_result_method_return_type(
    obj_ty: &Type,
    method: &str,
    first_arg_ty: Option<&Type>,
) -> Option<Type> {
    let builtin = builtin_types::resolve(obj_ty)?;
    match builtin.id {
        B::Option => {
            let args = builtin.args;
            let inner = args.first().cloned().unwrap_or(Type::Void);
            match method {
                "is_some" | "is_none" => Some(Type::Bool),
                "unwrap" | "expect" | "unwrap_or" => Some(inner),
                "map" => {
                    if let Some(Type::Fn(_, ret)) = first_arg_ty {
                        Some(B::Option.apply(vec![*ret.clone()]))
                    } else {
                        Some(obj_ty.clone())
                    }
                }
                "and_then" | "or_else" => {
                    if let Some(Type::Fn(_, ret)) = first_arg_ty {
                        let ret_ty = *ret.clone();
                        // If f's return is Generic (Option/Result), trust it.
                        // Otherwise fall back to the receiver type so the result
                        // is tracked as Option rather than a bare I64.
                        if matches!(ret_ty, Type::Generic(..)) {
                            Some(ret_ty)
                        } else {
                            Some(obj_ty.clone())
                        }
                    } else {
                        Some(obj_ty.clone())
                    }
                }
                _ => None,
            }
        }
        B::Result => {
            let args = builtin.args;
            let ok_ty = args.first().cloned().unwrap_or(Type::Void);
            let err_ty = args.get(1).cloned().unwrap_or(Type::Void);
            match method {
                "is_ok" | "is_err" => Some(Type::Bool),
                "unwrap" | "expect" | "unwrap_or" => Some(ok_ty.clone()),
                "unwrap_err" => Some(err_ty.clone()),
                "map" => {
                    if let Some(Type::Fn(_, ret)) = first_arg_ty {
                        Some(B::Result.apply(vec![*ret.clone(), err_ty]))
                    } else {
                        Some(obj_ty.clone())
                    }
                }
                "map_err" => {
                    if let Some(Type::Fn(_, ret)) = first_arg_ty {
                        Some(B::Result.apply(vec![ok_ty, *ret.clone()]))
                    } else {
                        Some(obj_ty.clone())
                    }
                }
                "and_then" | "or_else" => {
                    if let Some(Type::Fn(_, ret)) = first_arg_ty {
                        let ret_ty = *ret.clone();
                        if matches!(ret_ty, Type::Generic(..)) {
                            Some(ret_ty)
                        } else {
                            Some(obj_ty.clone())
                        }
                    } else {
                        Some(obj_ty.clone())
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod symbol_namespace_tests {
    use super::*;

    /// Perspective 1: every symbol the runtime ABI actually exports is
    /// reserved. This is the set whose hijacking is silent, so it is the one
    /// that must be complete rather than approximated.
    #[test]
    fn unit_symbols_01_every_runtime_abi_symbol_is_reserved() {
        for symbol in abi::RUNTIME_SYMBOLS {
            assert!(
                is_reserved_symbol(symbol.name),
                "runtime symbol `{}` must be reserved",
                symbol.name
            );
        }
    }

    /// Perspective 2: the reservation covers the whole `willow_` namespace, not
    /// only the symbols that exist today. A runtime symbol added later must not
    /// silently start colliding with user code that already compiled.
    #[test]
    fn unit_symbols_02_willow_prefix_is_reserved_even_when_unused() {
        assert!(is_reserved_symbol("willow_not_a_real_runtime_symbol"));
        assert!(is_reserved_symbol("willow_x"));
    }

    /// Perspective 3: the compiler's own generated data lives in `__willow_`.
    #[test]
    fn unit_symbols_03_compiler_internal_namespace_is_reserved() {
        assert!(is_reserved_symbol("__willow_static_init"));
        assert!(is_reserved_symbol("__willow_str_0"));
        assert!(is_reserved_symbol("__willow_gc_tlab_state"));
    }

    /// Perspective 4: `fn main` is what the compiler assigns
    /// `willow_user_main`, so reserving that name would reject every program.
    #[test]
    fn unit_symbols_04_user_main_symbol_is_not_reserved() {
        assert!(!is_reserved_symbol(USER_MAIN_SYMBOL));
    }

    /// Perspective 5: `willow__` with two underscores is user space. No runtime
    /// symbol has that shape, so a free function spelled that way stays legal —
    /// the reservation must not swallow it.
    #[test]
    fn unit_symbols_05_double_underscore_after_willow_is_user_space() {
        assert!(!is_reserved_symbol("willow__foo"));
        assert!(!is_reserved_symbol("willow__static__x"));
        assert!(!is_reserved_symbol("willow__Inner__method"));
    }

    /// Perspective 6: the reservation is prefix-anchored, not a substring
    /// search. A name that merely contains `willow_` is user space.
    #[test]
    fn unit_symbols_06_reservation_is_anchored_at_the_start() {
        assert!(!is_reserved_symbol("my_willow_helper"));
        assert!(!is_reserved_symbol("Wrapper__willow_thing"));
    }

    /// Perspective 7: case matters. `Willow_` is a perfectly ordinary user
    /// name.
    #[test]
    fn unit_symbols_07_reservation_is_case_sensitive() {
        assert!(!is_reserved_symbol("Willow_helper"));
        assert!(!is_reserved_symbol("WILLOW_HELPER"));
    }

    /// Perspective 8: a name that shares a prefix with `willow` but is not
    /// followed by the separator is unrelated.
    #[test]
    fn unit_symbols_08_prefix_requires_the_separator() {
        assert!(!is_reserved_symbol("willowfoo"));
        assert!(!is_reserved_symbol("willow"));
    }

    /// Perspective 9: a single leading underscore is not the compiler's
    /// namespace; only `__willow_` is.
    #[test]
    fn unit_symbols_09_single_underscore_prefix_is_user_space() {
        assert!(!is_reserved_symbol("_willow_thing"));
        assert!(!is_reserved_symbol("_private_helper"));
    }

    /// Perspective 10: ordinary user symbols, including the mangled shapes the
    /// backend produces for modules, classes, and static properties, are never
    /// reserved. A false positive here would reject working programs.
    #[test]
    fn unit_symbols_10_ordinary_mangled_shapes_are_not_reserved() {
        for symbol in [
            "main",
            "add",
            "math.add",
            "Point.area",
            "command_line_args.run",
            "Config.default_size$static",
            "Shape$as$Named$vtable",
        ] {
            assert!(!is_reserved_symbol(symbol), "`{symbol}` must stay usable");
        }
    }

    /// Perspective 51: a mangled symbol is never reserved, even when its first
    /// component reads like a runtime name (willow-uqzx, catalog item 8 phase
    /// 2). A runtime symbol is always a bare C identifier, so a joined symbol
    /// cannot be one — and before the scheme became injective, a class named
    /// `willow_box` was rejected for a collision that could not happen.
    #[test]
    fn unit_symbols_51_mangled_symbols_are_outside_the_reserved_namespace() {
        for symbol in [
            "willow_box.get",
            "willow_array_new.helper",
            "willow_gc_collect$vtable",
            "__willow_static_init.x",
            "willow_runtime.Value.print",
        ] {
            assert!(
                !is_reserved_symbol(symbol),
                "`{symbol}` carries a separator and cannot be a runtime symbol"
            );
        }
    }

    /// Perspective 52: the narrowing in perspective 51 is exactly that — a
    /// narrowing. The bare forms of the same names are still reserved, and a
    /// bare name is what an entry-file free function actually produces.
    #[test]
    fn unit_symbols_52_bare_runtime_names_are_still_reserved() {
        assert!(is_reserved_symbol("willow_array_new"));
        assert!(is_reserved_symbol("willow_gc_collect"));
        assert!(is_reserved_symbol("willow_box"));
        assert!(is_reserved_symbol("__willow_static_init"));
    }

    /// Perspective 11: the empty string is not reserved. Nothing produces it,
    /// but the predicate must not panic or over-match on it.
    #[test]
    fn unit_symbols_11_empty_symbol_is_not_reserved() {
        assert!(!is_reserved_symbol(""));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Span;

    /// The backend refuses a non-64-bit target rather than emitting code whose
    /// references are wider than the machine's pointers. The compiler always
    /// targets the host, so the check that matters here is that the host it
    /// accepted really is 64-bit — and that the function-address width the
    /// emitter bakes in agrees with that host's pointer width.
    #[test]
    fn the_accepted_target_is_64_bit_and_agrees_with_the_function_address_width() {
        let isa_builder = cranelift_native::builder().expect("host ISA");
        let isa = isa_builder
            .finish(settings::Flags::new(settings::builder()))
            .expect("host ISA flags");
        assert_eq!(
            isa.pointer_bits(),
            64,
            "willow only supports 64-bit targets; `Codegen::new` rejects the rest"
        );
        assert_eq!(
            isa.pointer_type(),
            type_helpers::FN_ADDR_TYPE,
            "a function address must be exactly as wide as a pointer on an accepted target"
        );
    }

    #[test]
    fn unit_async_codegen_01_sleep_builtin_returns_future_void() {
        assert_eq!(
            builtin_call_return_type("sleep"),
            Some(Type::Generic("Future".to_string(), vec![Type::Void]))
        );
    }

    #[test]
    fn unit_async_codegen_02_sleep_builtin_lowers_to_runtime_sleep() {
        assert_eq!(
            builtin_call_runtime_name("sleep"),
            Some("willow_runtime_sleep")
        );
    }

    #[test]
    fn unit_async_codegen_02b_yield_builtin_returns_future_void() {
        assert_eq!(
            builtin_call_return_type("yield"),
            Some(Type::Generic("Future".to_string(), vec![Type::Void]))
        );
    }

    #[test]
    fn unit_async_codegen_02c_yield_builtin_lowers_to_runtime_yield() {
        assert_eq!(
            builtin_call_runtime_name("yield"),
            Some("willow_runtime_yield")
        );
    }

    #[test]
    fn unit_async_codegen_03_channel_new_returns_channel_void_placeholder() {
        assert_eq!(
            builtin_static_return_type("Channel", &[], "new"),
            Some(Type::Generic("Channel".to_string(), vec![Type::Void]))
        );
    }

    #[test]
    fn unit_async_codegen_06_channel_new_with_type_arg_returns_typed_channel() {
        assert_eq!(
            builtin_static_return_type("Channel", &[Type::I64], "new"),
            Some(Type::Generic("Channel".to_string(), vec![Type::I64]))
        );
    }

    #[test]
    fn unit_async_codegen_04_channel_element_type_extracts_generic_argument() {
        assert_eq!(
            channel_element_type(&Type::Generic("Channel".to_string(), vec![Type::I64])),
            Some(Type::I64)
        );
        assert_eq!(channel_element_type(&Type::I64), None);
    }

    #[test]
    fn unit_async_codegen_05_channel_runtime_suffix_selects_primitive_or_pointer_abi() {
        assert_eq!(channel_runtime_suffix(&Type::I64), "i64");
        assert_eq!(channel_runtime_suffix(&Type::Bool), "bool");
        assert_eq!(channel_runtime_suffix(&Type::F64), "f64");
        assert_eq!(channel_runtime_suffix(&Type::String), "ptr");
        assert_eq!(
            channel_runtime_suffix(&Type::Named("Node".to_string())),
            "ptr"
        );
    }

    #[test]
    fn unit_async_codegen_07_future_uses_runtime_pointer_abi() {
        assert_eq!(
            clif_type(&Type::Generic("Future".to_string(), vec![Type::I64])),
            types::I64
        );
        assert_eq!(
            clif_type(&Type::Generic("Future".to_string(), vec![Type::Void])),
            types::I64
        );
    }

    #[test]
    fn class_gc_ref_mask_allows_first_63_fields() {
        let layout: Vec<(String, Type)> = (0..OBJECT_FIELD_MASK_CAPACITY)
            .map(|i| (format!("f{i}"), Type::String))
            .collect();
        let mask = try_gc_ref_mask_for_layout("ManyRefs", &layout, &HashMap::new()).unwrap();
        // Word 0 is the class descriptor, so fields occupy mask bits 1..63.
        assert_eq!(mask, u64::MAX << 1);
    }

    #[test]
    fn class_gc_ref_mask_rejects_gc_field_beyond_coverage() {
        let mut layout: Vec<(String, Type)> = (0..OBJECT_FIELD_MASK_CAPACITY)
            .map(|i| (format!("n{i}"), Type::I64))
            .collect();
        layout.push(("late".to_string(), Type::String));

        let err = try_gc_ref_mask_for_layout("TooWide", &layout, &HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("TooWide"), "{err}");
        assert!(err.contains("late"), "{err}");
        assert!(err.contains("outside gc_ref_mask coverage"), "{err}");
    }

    // ── Async frame GC metadata (willow-lpn.4) ──────────────────────────────
    //
    // Each test is one perspective on the GC reference mask the compiler must
    // hand to willow_async_frame_alloc: which frame slots are heap references.

    /// Helper: build a layout from `(name, ty)` slots with no enum registry.
    fn frame_layout(slots: &[(&str, Type)]) -> AsyncFrameLayout {
        let enum_infos: HashMap<String, EnumInfo> = HashMap::new();
        frame_layout_with(slots, &enum_infos)
    }

    fn frame_layout_with(
        slots: &[(&str, Type)],
        enum_infos: &HashMap<String, EnumInfo>,
    ) -> AsyncFrameLayout {
        let slots = slots
            .iter()
            .enumerate()
            .map(|(i, (n, t))| AsyncFrameSlot {
                // Distinct dummy spans so each test slot has a unique key.
                key: crate::diagnostics::Span::new(i, i, 0, 0),
                name: (*n).to_string(),
                ty: t.clone(),
            })
            .collect();
        AsyncFrameLayout::new(slots, enum_infos)
    }

    /// Helper: an EnumInfo registry with one enum of the given (name, payload) variants.
    fn enum_infos_with(name: &str, variants: &[(&str, Vec<Type>)]) -> HashMap<String, EnumInfo> {
        let mut map = HashMap::new();
        map.insert(
            name.to_string(),
            EnumInfo {
                name: name.to_string(),
                public: true,
                type_params: vec![],
                declaration_span: Span::dummy(),
                variants: variants
                    .iter()
                    .enumerate()
                    .map(|(i, (vn, pts))| crate::semantic::symbols::EnumVariantInfo {
                        name: (*vn).to_string(),
                        payload_types: pts.clone(),
                        tag: i as i64,
                        declaration_span: Span::dummy(),
                    })
                    .collect(),
            },
        );
        map
    }

    // 1. Empty frame → no slots, empty mask.
    #[test]
    fn async_frame_01_empty_layout_has_zero_mask() {
        let layout = frame_layout(&[]);
        assert_eq!(layout.slot_count(), 0);
        assert_eq!(layout.gc_slot_mask, 0);
    }

    // 2–4. Scalar slots are never GC references.
    #[test]
    fn async_frame_02_i64_slot_not_traced() {
        assert_eq!(frame_layout(&[("a", Type::I64)]).gc_slot_mask, 0);
    }

    #[test]
    fn async_frame_03_bool_slot_not_traced() {
        assert_eq!(frame_layout(&[("a", Type::Bool)]).gc_slot_mask, 0);
    }

    #[test]
    fn async_frame_04_f64_slot_not_traced() {
        assert_eq!(frame_layout(&[("a", Type::F64)]).gc_slot_mask, 0);
    }

    // 5. void slot is not traced.
    #[test]
    fn async_frame_05_void_slot_not_traced() {
        assert_eq!(frame_layout(&[("a", Type::Void)]).gc_slot_mask, 0);
    }

    // 6. A class reference (named, non-enum) is traced.
    #[test]
    fn async_frame_06_class_slot_traced() {
        let layout = frame_layout(&[("node", Type::Named("Node".to_string()))]);
        assert_eq!(layout.gc_slot_mask, 0b1);
        assert!(layout.slot_is_gc_ref(0));
    }

    // 7. A string slot is traced (GC-managed WillowString).
    #[test]
    fn async_frame_07_string_slot_traced() {
        assert_eq!(frame_layout(&[("s", Type::String)]).gc_slot_mask, 0b1);
    }

    // 8–9. Arrays of any element type are traced (handle + buffer are heap objects).
    #[test]
    fn async_frame_08_array_of_scalar_slot_traced() {
        let ty = Type::Array(Box::new(Type::I64));
        assert_eq!(frame_layout(&[("xs", ty)]).gc_slot_mask, 0b1);
    }

    #[test]
    fn async_frame_09_array_of_ref_slot_traced() {
        let ty = Type::Array(Box::new(Type::String));
        assert_eq!(frame_layout(&[("xs", ty)]).gc_slot_mask, 0b1);
    }

    // 10. `T?` is canonical Option<T>; a reference payload may use the
    // nullable-pointer niche and is traced (the runtime skips zero).
    #[test]
    fn async_frame_10_optional_ref_slot_traced() {
        let ty = Type::Generic("Option".to_string(), vec![Type::Named("Node".to_string())]);
        assert_eq!(frame_layout(&[("maybe", ty)]).gc_slot_mask, 0b1);
    }

    // 11. Option<i64> needs a tagged enum allocation and is therefore traced.
    #[test]
    fn async_frame_11_optional_primitive_slot_traced() {
        let ty = Type::Generic("Option".to_string(), vec![Type::I64]);
        assert_eq!(frame_layout(&[("maybe", ty)]).gc_slot_mask, 0b1);
    }

    // 12. Nested Option preserves both absence levels in a boxed outer value.
    #[test]
    fn async_frame_12_nested_optional_ref_traced() {
        let ty = Type::Generic(
            "Option".to_string(),
            vec![Type::Generic("Option".to_string(), vec![Type::String])],
        );
        assert_eq!(frame_layout(&[("m", ty)]).gc_slot_mask, 0b1);
    }

    // 13. Future is an opaque runtime pointer (no GcHeader) and is NOT traced
    //     from a frame slot; Channel became a GC OBJECT (willow-p4er) and
    //     Task/JoinHandle are GC async frames — all three ARE traced.
    #[test]
    fn async_frame_13_runtime_pointer_generics_and_joinhandle() {
        let future = Type::Generic("Future".to_string(), vec![Type::I64]);
        let channel = Type::Generic("Channel".to_string(), vec![Type::String]);
        let task = Type::Generic("Task".to_string(), vec![Type::I64]);
        let join = Type::Generic("JoinHandle".to_string(), vec![Type::Void]);
        assert_eq!(frame_layout(&[("f", future)]).gc_slot_mask, 0);
        assert_eq!(frame_layout(&[("c", channel)]).gc_slot_mask, 0b1);
        assert_eq!(frame_layout(&[("t", task)]).gc_slot_mask, 0b1);
        assert_eq!(frame_layout(&[("j", join)]).gc_slot_mask, 0b1);
    }

    // 14. Option<i64> (a generic enum carrying payload) is a heap object → traced.
    #[test]
    fn async_frame_14_option_generic_enum_traced() {
        let ty = Type::Generic("Option".to_string(), vec![Type::I64]);
        assert_eq!(frame_layout(&[("o", ty)]).gc_slot_mask, 0b1);
    }

    // 15. Result<String,i64> is a heap object → traced.
    #[test]
    fn async_frame_15_result_generic_enum_traced() {
        let ty = Type::Generic("Result".to_string(), vec![Type::String, Type::I64]);
        assert_eq!(frame_layout(&[("r", ty)]).gc_slot_mask, 0b1);
    }

    // 16. A fieldless enum lowers to an immediate tag → NOT traced.
    #[test]
    fn async_frame_16_fieldless_enum_not_traced() {
        let enums = enum_infos_with(
            "Color",
            &[("Red", vec![]), ("Green", vec![]), ("Blue", vec![])],
        );
        let layout = frame_layout_with(&[("c", Type::Named("Color".to_string()))], &enums);
        assert_eq!(layout.gc_slot_mask, 0);
    }

    // 17. A with-payload enum is heap-allocated → traced.
    #[test]
    fn async_frame_17_payload_enum_traced() {
        let enums = enum_infos_with("Shape", &[("Dot", vec![]), ("Circle", vec![Type::I64])]);
        let layout = frame_layout_with(&[("s", Type::Named("Shape".to_string()))], &enums);
        assert_eq!(layout.gc_slot_mask, 0b1);
    }

    // 18. Mixed slots: only the GC-reference slots set their bit, by slot index.
    #[test]
    fn async_frame_18_mixed_slots_mask_by_index() {
        let layout = frame_layout(&[
            ("count", Type::I64),                      // slot 0 — not traced
            ("node", Type::Named("Node".to_string())), // slot 1 — traced
            ("ok", Type::Bool),                        // slot 2 — not traced
            ("name", Type::String),                    // slot 3 — traced
        ]);
        assert_eq!(layout.gc_slot_mask, 0b1010);
        assert!(!layout.slot_is_gc_ref(0));
        assert!(layout.slot_is_gc_ref(1));
        assert!(!layout.slot_is_gc_ref(2));
        assert!(layout.slot_is_gc_ref(3));
        assert_eq!(layout.slot_count(), 4);
    }

    // 19. The mask is slot-relative: a reference at slot K sets bit K (the runtime
    //     allocator applies the 3-word header shift, not the compiler).
    #[test]
    fn async_frame_19_mask_is_slot_relative() {
        let layout = frame_layout(&[
            ("a", Type::I64),
            ("b", Type::I64),
            ("c", Type::I64),
            ("ref", Type::String), // slot 3
        ]);
        assert_eq!(layout.gc_slot_mask, 1u64 << 3);
    }

    // 20. GC slots beyond runtime mask coverage are rejected, not truncated.
    #[test]
    fn async_frame_20_gc_slots_beyond_runtime_mask_are_rejected() {
        let mut slots: Vec<(&str, Type)> = Vec::new();
        for _ in 0..ASYNC_FRAME_GC_SLOT_CAPACITY {
            slots.push(("r", Type::String));
        }
        let layout = frame_layout(&slots);
        assert_eq!(
            layout.gc_slot_mask,
            (1u64 << ASYNC_FRAME_GC_SLOT_CAPACITY) - 1
        );

        let too_many_slots: Vec<AsyncFrameSlot> = (0..=ASYNC_FRAME_GC_SLOT_CAPACITY)
            .map(|i| AsyncFrameSlot {
                key: crate::diagnostics::Span::new(i, i, 0, 0),
                name: format!("r{i}"),
                ty: Type::String,
            })
            .collect();
        let err = AsyncFrameLayout::try_new(too_many_slots, &HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside gc_ref_mask coverage"), "{err}");
    }

    // 21. The collector lists parameters first, then annotated `let` locals,
    //     including ones declared inside nested blocks. Each binding is keyed by
    //     its (distinct) declaration span (willow-lpn.11).
    #[test]
    fn async_frame_21_collector_params_then_nested_lets() {
        let params = vec![Param {
            name: "x".to_string(),
            ty: Type::Named("Node".to_string()),
            mode: ParamMode::Value,
            span: Span::new(1, 1, 1, 1),
            type_span: Span::dummy(),
        }];
        // body: let y: String = ...; while ... { let z: i64 = ...; }
        let body = Block {
            stmts: vec![
                Stmt::Let(LetStmt {
                    name: "y".to_string(),
                    mutable: false,
                    ty: Some(Type::String),
                    init: Expr::Integer(0, Span::dummy()),
                    span: Span::new(2, 2, 2, 1),
                }),
                Stmt::While(WhileStmt {
                    cond: Expr::Bool(true, Span::dummy()),
                    body: Block {
                        stmts: vec![Stmt::Let(LetStmt {
                            name: "z".to_string(),
                            mutable: false,
                            ty: Some(Type::I64),
                            init: Expr::Integer(0, Span::dummy()),
                            span: Span::new(3, 3, 3, 1),
                        })],
                        span: Span::dummy(),
                    },
                    span: Span::dummy(),
                }),
            ],
            span: Span::dummy(),
        };
        let slots = collect_async_frame_slots(&params, &body);
        let names: Vec<&str> = slots.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["x", "y", "z"]);

        // And the mask over those slots: x (Node) and y (String) are refs, z (i64) is not.
        let enum_infos: HashMap<String, EnumInfo> = HashMap::new();
        let layout = AsyncFrameLayout::new(slots, &enum_infos);
        assert_eq!(layout.gc_slot_mask, 0b011);
    }

    // 22. Unannotated `let` locals are skipped by the conservative collector
    //     (their inferred types are supplied by Stage 5, willow-lpn.5).
    #[test]
    fn async_frame_22_collector_skips_unannotated_lets() {
        let body = Block {
            stmts: vec![Stmt::Let(LetStmt {
                name: "inferred".to_string(),
                mutable: false,
                ty: None,
                init: Expr::Integer(1, Span::dummy()),
                span: Span::dummy(),
            })],
            span: Span::dummy(),
        };
        let slots = collect_async_frame_slots(&[], &body);
        assert!(slots.is_empty());
    }

    #[test]
    fn unit_async_codegen_08_async_function_call_returns_task_type() {
        let function = FunctionDecl {
            name: "work".to_string(),
            public: false,
            is_async: true,
            params: Vec::new(),
            return_type: Type::I64,
            body: Block {
                stmts: Vec::new(),
                span: crate::diagnostics::Span::dummy(),
            },
            span: crate::diagnostics::Span::dummy(),
        };

        assert_eq!(
            function_call_return_type(&function),
            Type::Generic("Task".to_string(), vec![Type::I64])
        );
    }

    #[test]
    fn unit_async_codegen_09_future_ready_runtime_selects_by_value_type() {
        assert_eq!(
            future_ready_runtime_name(&Type::Void),
            "willow_future_ready_void"
        );
        assert_eq!(
            future_ready_runtime_name(&Type::I64),
            "willow_future_ready_i64"
        );
        assert_eq!(
            future_ready_runtime_name(&Type::Bool),
            "willow_future_ready_bool"
        );
        assert_eq!(
            future_ready_runtime_name(&Type::F64),
            "willow_future_ready_f64"
        );
        assert_eq!(
            future_ready_runtime_name(&Type::String),
            "willow_future_ready_ptr"
        );
    }

    #[test]
    fn unit_async_codegen_10_future_await_runtime_selects_by_output_type() {
        assert_eq!(
            future_await_runtime_name(&Type::Void),
            "willow_future_await_void"
        );
        assert_eq!(
            future_await_runtime_name(&Type::I64),
            "willow_future_await_i64"
        );
        assert_eq!(
            future_await_runtime_name(&Type::Bool),
            "willow_future_await_bool"
        );
        assert_eq!(
            future_await_runtime_name(&Type::F64),
            "willow_future_await_f64"
        );
        assert_eq!(
            future_await_runtime_name(&Type::Named("Node".to_string())),
            "willow_future_await_ptr"
        );
    }
}

// ── Reference debug string collection helpers ────────────────────────────────

// ── String literal collection helpers ─────────────────────────────────────────

// ── Lambda collection helpers ─────────────────────────────────────────────────

// ── Spawn-site collection helpers ────────────────────────────────────────────
// Returns (span, tramp_name, callee_name) for every Expr::Spawn in the program.

// ── Nil-check string pre-scan ─────────────────────────────────────────────────
// Collect all field names and method names referenced in the program so their
// string literals can be pre-declared before any function is compiled.
