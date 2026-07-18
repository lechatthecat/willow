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
use crate::semantic::ids::{FunctionId, FunctionMap};
use crate::semantic::symbols::{EnumInfo, InterfaceInfo};
use crate::{BuildMode, CompilerOptions};

mod ast_passes;
mod async_codegen;
mod compile;
mod coop;
mod emit;
mod emit_builtins;
mod emit_collections;
mod emit_expr;
mod emit_interface;
mod emit_match;
mod emit_object;
mod emit_option_result;
mod emit_stmt;
mod lir_gen;
mod std_collection;
mod symbols;
mod type_helpers;
use ast_passes::*;
use coop::*;
use std_collection::*;
use symbols::*;
use type_helpers::*;

const USER_MAIN_SYMBOL: &str = "willow_user_main";
/// Generated function that initializes all `static` properties before `main`
/// (willow-qsqf §13.5).
const STATIC_INIT_SYMBOL: &str = "__willow_static_init";
const GC_REF_MASK_BITS: usize = 64;
const OBJECT_FIELD_MASK_CAPACITY: usize = GC_REF_MASK_BITS - 1;
const ASYNC_FRAME_HEADER_WORDS: usize = 2;
const ASYNC_FRAME_GC_SLOT_CAPACITY: usize = GC_REF_MASK_BITS - ASYNC_FRAME_HEADER_WORDS;
const ASYNC_FRAME_LARGE_WARNING_BYTES: usize = 8 * 1024;
const COOP_POLL_PREEMPTED: i64 = 3;

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
    #[allow(clippy::type_complexity)]
    class_layouts: Vec<(String, Option<Vec<(String, Type)>>)>,
    class_base: Vec<(String, Option<String>)>,
    class_type_ids: Vec<(String, Option<i64>)>,
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
    /// Imported module access name -> canonical symbol prefix.
    known_modules: HashMap<String, String>,
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
    /// Build mode: controls whether debug nil checks are emitted.
    build_mode: BuildMode,
    /// Source file path of the current compilation unit, used in nil-check diagnostics.
    source_file: String,
    /// Enum info for enum variant construction in generated code.
    enum_infos: HashMap<String, EnumInfo>,
    /// Maps child class name → base class name for inherited method dispatch.
    class_base: HashMap<String, String>,
    /// Maps each class name to a unique integer type_id for runtime dynamic dispatch.
    /// Type ids start at 1; 0 is reserved for null/unknown.
    class_type_ids: HashMap<String, i64>,
    /// Maps each lambda's source span to its type-checker-inferred return type.
    /// Populated via register_lambda_return_types before compilation starts.
    lambda_return_types: HashMap<crate::diagnostics::Span, Type>,
    /// Full type-checker-inferred fn types for lambdas, including parameter
    /// types inferred from call-site context.
    lambda_fn_types: HashMap<crate::diagnostics::Span, Type>,
    /// Resolved types of async-fn `let` locals (keyed by span) so the backend
    /// can frame-back unannotated live-across-await locals (willow-lpn.5c).
    async_local_types: HashMap<crate::diagnostics::Span, Type>,
    /// The checker's authoritative type for every checked expression, keyed by
    /// span (willow-mb5). Consulted FIRST by the backend's type queries; the
    /// legacy structural derivation only covers unrecorded (compiler-
    /// synthesized) expressions.
    expr_types: HashMap<crate::diagnostics::Span, Type>,
    /// Lowered-IR functions of the entry program (willow-0g8j): a function in
    /// the supported subset is compiled by walking its LIR instead of the AST.
    lir_functions: HashMap<String, crate::ir::lowered::LirFunction>,
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
    async_frame_size_warnings: Vec<AsyncFrameSizeWarning>,
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
        let flags = settings::Flags::new(flag_builder);
        let isa = isa_builder.finish(flags)?;
        let obj_builder =
            ObjectBuilder::new(isa, "willow", cranelift_module::default_libcall_names())?;
        let module = ObjectModule::new(obj_builder);
        Ok(Self {
            module,
            func_ids: FunctionMap::default(),
            func_return_types: FunctionMap::default(),
            fn_types: FunctionMap::default(),
            func_param_modes: FunctionMap::default(),
            func_param_debug: FunctionMap::default(),
            known_modules: HashMap::new(),
            lambda_names: HashMap::new(),
            cooperative_leaves: std::collections::HashSet::new(),
            string_literals: HashMap::new(),
            string_counter: 0,
            runtime_declared: false,
            class_layouts: HashMap::new(),
            build_mode: opts.target.build_mode,
            source_file: String::new(),
            enum_infos: HashMap::new(),
            class_base: HashMap::new(),
            class_type_ids: HashMap::new(),
            lambda_return_types: HashMap::new(),
            lambda_fn_types: HashMap::new(),
            async_local_types: HashMap::new(),
            expr_types: HashMap::new(),
            lir_functions: HashMap::new(),
            enum_variant_resolutions: HashMap::new(),
            pattern_resolutions: HashMap::new(),
            interface_infos: HashMap::new(),
            vtable_ids: HashMap::new(),
            static_storage: HashMap::new(),
            static_init_order: Vec::new(),
            async_frame_size_warnings: Vec::new(),
        })
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

    /// Register enum info so the backend can lower enum variant construction.
    pub fn register_enum_info(&mut self, name: String, info: EnumInfo) {
        self.enum_infos.insert(name, info);
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
    }

    pub fn register_async_local_types(&mut self, types: HashMap<crate::diagnostics::Span, Type>) {
        self.async_local_types = types;
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

    /// Register the type-checker-inferred return types for all lambdas in the program.
    /// Must be called before compile_program / compile_module so that declare_lambda
    /// can emit correct signatures for unannotated lambdas.
    pub fn register_lambda_return_types(&mut self, types: HashMap<crate::diagnostics::Span, Type>) {
        self.lambda_return_types = types;
    }

    /// Register complete inferred fn types for lambdas whose parameter types
    /// were supplied by call-site context rather than source annotations.
    pub fn register_lambda_fn_types(&mut self, types: HashMap<crate::diagnostics::Span, Type>) {
        self.lambda_fn_types = types;
    }

    /// No-op: generic enums are now registered via `register_enum_info` from the
    /// prelude, exactly like user-defined enums.  Kept for call-site compatibility.
    pub fn register_builtin_generic_enums(&mut self) {}

    /// Bind a single-item import: the local name aliases the module function's
    /// mangled symbol (`{module}__{item}`), so an unqualified call to `local`
    /// lowers to the module function. Must be called after the module is
    /// compiled. No-op if the symbol is absent (the type checker already
    /// reported the error).
    pub fn register_item_import(&mut self, local: &str, module: &str, item: &str) {
        let module_prefix = self
            .known_modules
            .get(module)
            .cloned()
            .unwrap_or_else(|| module_symbol_prefix(module));
        let mangled = format!("{module_prefix}__{item}");
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
            return;
        }

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
            // Methods: alias every per-method table from
            // `{module_prefix}__{item}__M` to `{local}__M` (func id AND return
            // type / fn type / param modes / debug, so dispatch + return typing
            // resolve under the local name).
            let method_prefix = format!("{module_prefix}__{item}__");
            let method_symbols: Vec<String> = self
                .func_ids
                .ids()
                .map(ToString::to_string)
                .filter(|name| name.starts_with(&method_prefix))
                .collect();
            for full in method_symbols {
                let suffix = full.strip_prefix(&method_prefix).unwrap();
                let alias = format!("{local}__{suffix}");
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
                    self.func_param_debug.insert(alias, pd);
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
        restore_snapshots(&mut self.class_layouts, aliases.class_layouts);
        restore_snapshots(&mut self.class_base, aliases.class_base);
        restore_snapshots(&mut self.class_type_ids, aliases.class_type_ids);
        restore_snapshots(&mut self.enum_infos, aliases.enum_infos);
        restore_snapshots(&mut self.interface_infos, aliases.interface_infos);
        restore_snapshots(&mut self.vtable_ids, aliases.vtable_ids);
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

    /// Reserve a GC-traced frame slot to hold the callee frame of a call-await
    /// (`await <coop-leaf-call>`) across the awaiter's suspension, keyed by the
    /// await span. The slot is GC-managed so the collector keeps the callee frame
    /// (a willow_alloc_typed object) alive — and traces its GC contents — after
    /// the scheduler drops the callee's own root on completion (willow-lpn.5.3.1).
    fn coop_collect_callee_frame_slot(
        &self,
        expr: &Expr,
        out: &mut Vec<AsyncFrameSlot>,
        seen: &mut HashSet<crate::diagnostics::Span>,
    ) {
        // Reserve a GC-traced callee-frame slot for every direct-call-form await
        // that suspends cooperatively. A leaf call uses `emit_coop_call_await`; a
        // non-leaf call (imported async) and a method/static-call await use
        // `emit_coop_task_await`, whose resume path RELOADS the task frame from
        // this slot instead of re-emitting the call — without the slot it would
        // re-run the call on resume (willow-0a6k.6).
        let await_span = await_callee_frame_slot_span(expr, &self.cooperative_leaves);
        if let Some(await_span) = await_span
            && seen.insert(await_span)
        {
            out.push(AsyncFrameSlot {
                key: await_span,
                name: "__callee_frame".to_string(),
                ty: Type::Named("__coop_callee_frame".to_string()),
            });
        }
    }

    fn coop_collect_let_slots(
        &self,
        block: &Block,
        out: &mut Vec<AsyncFrameSlot>,
        seen: &mut HashSet<crate::diagnostics::Span>,
    ) {
        for stmt in &block.stmts {
            match stmt {
                Stmt::Break(_) | Stmt::Continue(_) => {}
                Stmt::Let(l) => {
                    let ty =
                        l.ty.clone()
                            .or_else(|| self.async_local_types.get(&l.span).cloned());
                    if let Some(ty) = ty
                        && seen.insert(l.span)
                    {
                        out.push(AsyncFrameSlot {
                            key: l.span,
                            name: l.name.clone(),
                            ty,
                        });
                    }
                    self.coop_collect_callee_frame_slot(&l.init, out, seen);
                }
                Stmt::Assign(s) => {
                    self.coop_collect_callee_frame_slot(&s.value, out, seen);
                }
                Stmt::StaticFieldAssign(s) => {
                    self.coop_collect_callee_frame_slot(&s.value, out, seen);
                }
                Stmt::FieldAssign(s) => {
                    self.coop_collect_callee_frame_slot(&s.value, out, seen);
                }
                Stmt::IndexAssign(s) => {
                    self.coop_collect_callee_frame_slot(&s.value, out, seen);
                }
                Stmt::SuperInit(s) => {
                    for arg in &s.args {
                        self.coop_collect_callee_frame_slot(&arg.expr, out, seen);
                    }
                }
                Stmt::Expr(es) => {
                    if let Expr::Select(sel) = &es.expr {
                        // A cooperative `select` needs a frame slot per recv binding
                        // (so it survives the case body's own suspensions) plus the
                        // slots its case bodies declare (willow-7aj).
                        for case in &sel.cases {
                            match &case.kind {
                                SelectCaseKind::Recv { binding, channel } => {
                                    self.coop_collect_callee_frame_slot(channel, out, seen);
                                    if binding != "_"
                                        && let Some(elem_ty) =
                                            self.async_local_types.get(&case.span).cloned()
                                        && seen.insert(case.span)
                                    {
                                        out.push(AsyncFrameSlot {
                                            key: case.span,
                                            name: binding.clone(),
                                            ty: elem_ty,
                                        });
                                    }
                                }
                                SelectCaseKind::Send { channel, value } => {
                                    self.coop_collect_callee_frame_slot(channel, out, seen);
                                    self.coop_collect_callee_frame_slot(value, out, seen);
                                }
                                SelectCaseKind::Default => {}
                            }
                            self.coop_collect_let_slots(&case.body, out, seen);
                        }
                    } else {
                        self.coop_collect_callee_frame_slot(&es.expr, out, seen);
                    }
                }
                Stmt::Return(s) => {
                    if let Some(value) = &s.value {
                        self.coop_collect_callee_frame_slot(value, out, seen);
                    }
                }
                Stmt::If(s) => {
                    self.coop_collect_let_slots(&s.then_block, out, seen);
                    if let Some(e) = &s.else_block {
                        self.coop_collect_let_slots(e, out, seen);
                    }
                }
                Stmt::While(s) => self.coop_collect_let_slots(&s.body, out, seen),
                Stmt::For(s) => {
                    for (key, name) in [
                        (s.iter_frame_key(), "__for_iter".to_string()),
                        (s.index_frame_key(), "__for_index".to_string()),
                        (s.name_span, s.name.clone()),
                    ] {
                        if let Some(ty) = self.async_local_types.get(&key).cloned()
                            && seen.insert(key)
                        {
                            out.push(AsyncFrameSlot { key, name, ty });
                        }
                    }
                    self.coop_collect_let_slots(&s.body, out, seen);
                }
            }
        }
    }

    // ── Class helpers ─────────────────────────────────────────────────────────

    fn register_class_layout(&mut self, c: &ClassDecl) {
        // Prepend any inherited fields from the base class (base fields come first
        // so the field-offset layout is compatible with the base class layout).
        let mut fields: Vec<(String, Type)> = if let Some(base_path) = &c.base_class {
            let base_name = base_path.name();
            self.class_layouts
                .get(base_name)
                .cloned()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // Add fields declared directly on this class (child fields follow base fields).
        for f in &c.fields {
            if f.is_static {
                continue;
            }
            if !fields.iter().any(|(n, _)| n == &f.name) {
                fields.push((f.name.clone(), f.ty.clone()));
            }
        }
        self.class_layouts.insert(c.name.clone(), fields);
        if let Some(base_path) = &c.base_class {
            self.class_base
                .insert(c.name.clone(), base_path.name().to_string());
        }
        // Assign a unique type_id for runtime dynamic dispatch (word 0 of every object).
        let next_id = self.class_type_ids.len() as i64 + 1;
        self.class_type_ids.entry(c.name.clone()).or_insert(next_id);
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

struct FuncGen<'a, 'b> {
    builder: &'a mut FunctionBuilder<'b>,
    module: &'a mut ObjectModule,
    /// Innermost-first enclosing-loop context for break/continue emission
    /// (willow-kzka): (exit block, continue target, GC-root count at loop
    /// entry — an early exit pops roots down to this baseline).
    loop_stack: Vec<(
        cranelift_codegen::ir::Block,
        cranelift_codegen::ir::Block,
        usize,
    )>,
    func_ids: &'a FunctionMap<FuncId>,
    func_return_types: &'a FunctionMap<Type>,
    fn_types: &'a FunctionMap<Type>,
    func_param_modes: &'a FunctionMap<Vec<ParamMode>>,
    func_param_debug: &'a FunctionMap<Vec<ParamDebug>>,
    known_modules: &'a HashMap<String, String>,
    lambda_names: &'a HashMap<crate::diagnostics::Span, String>,
    cooperative_leaves: &'a std::collections::HashSet<FunctionId>,
    string_literals: &'a HashMap<String, DataId>,
    class_layouts: &'a HashMap<String, Vec<(String, Type)>>,
    static_storage: &'a HashMap<(String, String), StaticStorageInfo>,
    enum_infos: &'a HashMap<String, EnumInfo>,
    class_base: &'a HashMap<String, String>,
    /// Maps class name → unique type_id (i64) stored at word 0 of every class object.
    class_type_ids: &'a HashMap<String, i64>,
    /// Type-checker-inferred return types for lambdas without explicit annotations.
    lambda_return_types: &'a HashMap<crate::diagnostics::Span, Type>,
    /// Full inferred fn types for lambdas, including contextual parameter types.
    lambda_fn_types: &'a HashMap<crate::diagnostics::Span, Type>,
    /// Interface metadata for method dispatch + boxing.
    interface_infos: &'a HashMap<String, InterfaceInfo>,
    /// Static `(class, interface)` vtable data objects for class→interface boxing.
    vtable_ids: &'a HashMap<(String, String), DataId>,
    /// Resolved types of async-fn locals (keyed by span) for frame-backing
    /// unannotated live-across-await locals (willow-lpn.5c).
    async_local_types: &'a HashMap<crate::diagnostics::Span, Type>,
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
    /// Build mode: controls whether debug nil checks are emitted.
    build_mode: BuildMode,
    /// Source file path used in nil-check runtime diagnostics.
    source_file: &'a str,
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
/// (`willow_async_frame_alloc` lays out `[state(word0) | slot_count(word1) | data slot 0..]`).
const ASYNC_FRAME_HEADER_BYTES: i32 = 16;

/// Byte offset of data slot `n` from the async frame base.
/// Async-task frame slot indices used with [`async_frame_slot_offset`].
/// Every async/task frame begins with these fixed slots after its header:
/// slot 0 holds the task's RESULT value, slot 1 holds its scheduler TASK ID.
const FRAME_SLOT_RESULT: usize = 0;
const FRAME_SLOT_TASK_ID: usize = 1;

fn async_frame_slot_offset(n: usize) -> i32 {
    ASYNC_FRAME_HEADER_BYTES + (n as i32) * 8
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Look up a declared runtime/user function id by symbol name, with a clear
    /// panic if it was never declared (e.g. a backend symbol missing from
    /// `abi.rs`) instead of an opaque index-out-of-bounds.
    fn func_id(&self, name: &str) -> FuncId {
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
                self.builder.ins().stack_store(val, slot, 0);
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
        self.builder
            .ins()
            .store(MemFlagsData::new(), val, base, offset);
        self.vars.insert(
            name.to_string(),
            VarStorage::Frame {
                offset,
                ty: ty.clone(),
            },
        );
    }

    /// Allocate and GC-root a heap async frame for this function if it has at
    /// least one GC-managed value parameter that must survive `await`
    /// (willow-lpn.5a). Returns the frame layout when a frame was allocated, so
    /// the caller can frame-back the relevant parameters. Eager execution is
    /// unchanged; the frame is the GC-safe home for live-across-await values.
    /// Like the free `collect_async_frame_slots`, but also includes UNANNOTATED
    /// `let` locals using the type-checker-resolved types in `async_local_types`
    /// (willow-lpn.5c). Order: params, then locals in source order, deduped.
    fn collect_async_frame_slots_resolved(
        &self,
        params: &[Param],
        body: &Block,
    ) -> Vec<AsyncFrameSlot> {
        let mut slots: Vec<AsyncFrameSlot> = params
            .iter()
            .map(|p| AsyncFrameSlot {
                key: p.span,
                name: p.name.clone(),
                ty: p.ty.clone(),
            })
            .collect();
        // Dedup by the binding's span (unique per param/`let`), NOT by name, so
        // that nested shadowed locals get their own slots (willow-lpn.11).
        let mut seen: HashSet<crate::diagnostics::Span> = slots.iter().map(|s| s.key).collect();
        self.collect_let_slots_resolved(body, &mut slots, &mut seen);
        slots
    }

    fn collect_let_slots_resolved(
        &self,
        block: &Block,
        out: &mut Vec<AsyncFrameSlot>,
        seen: &mut HashSet<crate::diagnostics::Span>,
    ) {
        for stmt in &block.stmts {
            match stmt {
                Stmt::Let(l) => {
                    // Annotated locals carry their type; unannotated ones use the
                    // type-checker-resolved type recorded for their span.
                    let ty =
                        l.ty.clone()
                            .or_else(|| self.async_local_types.get(&l.span).cloned());
                    if let Some(ty) = ty
                        && seen.insert(l.span)
                    {
                        out.push(AsyncFrameSlot {
                            key: l.span,
                            name: l.name.clone(),
                            ty,
                        });
                    }
                }
                Stmt::If(s) => {
                    self.collect_let_slots_resolved(&s.then_block, out, seen);
                    if let Some(else_block) = &s.else_block {
                        self.collect_let_slots_resolved(else_block, out, seen);
                    }
                }
                Stmt::While(s) => self.collect_let_slots_resolved(&s.body, out, seen),
                Stmt::For(s) => self.collect_let_slots_resolved(&s.body, out, seen),
                _ => {}
            }
        }
    }

    fn setup_async_frame(
        &mut self,
        params: &[Param],
        body: &Block,
    ) -> Result<Option<AsyncFrameLayout>> {
        let slots = self.collect_async_frame_slots_resolved(params, body);
        let layout = AsyncFrameLayout::try_new(slots, self.enum_infos)?;

        // The GC-managed slots (params + annotated locals) are the ones we
        // frame-back. Only allocate a frame when there is at least one —
        // async fns without GC state are unaffected (no extra allocation).
        let mut offsets: HashMap<crate::diagnostics::Span, i32> = HashMap::new();
        for (i, slot) in layout.slots.iter().enumerate() {
            if layout.slot_is_gc_ref(i) {
                offsets.insert(slot.key, async_frame_slot_offset(i));
            }
        }
        if offsets.is_empty() {
            return Ok(None);
        }

        let slot_count = self
            .builder
            .ins()
            .iconst(types::I64, layout.slot_count() as i64);
        let mask = self
            .builder
            .ins()
            .iconst(types::I64, layout.gc_slot_mask as i64);
        let alloc_id = self.func_id("willow_async_frame_alloc");
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[slot_count, mask]);
        let frame = self.builder.inst_results(call)[0];
        // Root the frame for the function's duration (popped on return with the
        // other parameter roots via the gc_root_count mechanism).
        self.emit_push_root(frame);
        self.async_frame = Some(frame);
        self.async_frame_offsets = offsets;
        Ok(Some(layout))
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
        self.builder.ins().stack_store(val, slot, 0);
        VarStorage::Stack {
            slot,
            ty: ty.clone(),
        }
    }

    fn load_var(&mut self, storage: &VarStorage) -> cranelift_codegen::ir::Value {
        match storage {
            VarStorage::Value { var, .. } => self.builder.use_var(*var),
            VarStorage::Stack { slot, ty } => {
                self.builder.ins().stack_load(clif_type(ty), *slot, 0)
            }
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
                self.builder.ins().stack_store(val, *slot, 0);
            }
            VarStorage::ReferencePtr { var, ty } => {
                let ptr = self.builder.use_var(*var);
                self.store_indirect_reference(ptr, val, ty);
            }
            VarStorage::Frame { offset, .. } => {
                let base = self
                    .async_frame
                    .expect("frame-backed var requires an allocated async frame");
                self.builder
                    .ins()
                    .store(MemFlagsData::new(), val, base, *offset);
            }
        }
    }

    fn store_indirect_reference(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        val: cranelift_codegen::ir::Value,
        ty: &Type,
    ) {
        self.emit_reference_write_barrier_hook(ptr, val, ty);
        self.builder.ins().store(MemFlagsData::new(), val, ptr, 0);
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
        // Unwrap a nullable target: a non-nil class value boxes the same way.
        let target_inner = match target_ty {
            Type::Nullable(inner) => inner.as_ref(),
            other => other,
        };
        // The interface name comes from either a plain interface (`Animal`) or a
        // generic interface instantiation (`Box<String>`); type args do not
        // change the vtable, so boxing is identical (willow-1js.1).
        let iface_name = match target_inner {
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
        let value_inner = match value_ty {
            Type::Nullable(inner) => inner.as_ref(),
            other => other,
        };
        if let Type::Named(class_name) = value_inner
            && self.class_layouts.contains_key(class_name)
        {
            return self.emit_interface_box(value, class_name, iface_name);
        }
        value
    }

    /// Explicit parameter types (aligned with call arguments, no `self`) for a
    /// declared function/lambda mangled name. `None` if not a known function.
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
                // Built-in primitive `toString()` -> String (willow-fvfc).
                if m.method == "toString"
                    && m.args.is_empty()
                    && matches!(obj_ty, Type::I64 | Type::F64 | Type::Bool | Type::String)
                {
                    return Type::String;
                }
                if m.method == "join"
                    && let Some(result_ty) = join_handle_result_type(&obj_ty)
                {
                    return result_ty;
                }
                if m.method == "recv"
                    && let Some(element_ty) = channel_element_type(&obj_ty)
                {
                    return element_ty;
                }
                if let Type::Named(n) = &obj_ty
                    && (n == "AtomicI64" || n == "AtomicBool")
                {
                    let elem = if n == "AtomicI64" {
                        Type::I64
                    } else {
                        Type::Bool
                    };
                    match m.method.as_str() {
                        "load" | "swap" => return elem,
                        "add" | "sub" => return Type::I64,
                        "store" => return Type::Void,
                        _ => {}
                    }
                }
                if let Type::Generic(n, margs) = &obj_ty
                    && (n == "Mutex" || n == "RwLock")
                    && margs.len() == 1
                {
                    match m.method.as_str() {
                        "get" | "read" => return margs[0].clone(),
                        "set" | "write" => return Type::Void,
                        _ => {}
                    }
                }
                if let Type::Array(elem) = &obj_ty {
                    match m.method.as_str() {
                        "len" => return Type::I64,
                        "pop" => return (**elem).clone(),
                        "push" => return Type::Void,
                        "freeze" => {
                            return Type::Generic(
                                "FrozenArray".to_string(),
                                vec![(**elem).clone()],
                            );
                        }
                        _ => {}
                    }
                }
                if let Type::Generic(name, fargs) = &obj_ty
                    && name == "FrozenArray"
                    && fargs.len() == 1
                    && m.method == "len"
                {
                    return Type::I64;
                }
                if let Type::Generic(name, margs) = &obj_ty {
                    if name == "Map" && margs.len() == 2 {
                        match m.method.as_str() {
                            "get" => {
                                return Type::Generic("Option".to_string(), vec![margs[1].clone()]);
                            }
                            "len" => return Type::I64,
                            "contains" => return Type::Bool,
                            "freeze" => {
                                return Type::Generic("FrozenMap".to_string(), margs.clone());
                            }
                            _ => return Type::Void,
                        }
                    }
                    if name == "FrozenMap" && margs.len() == 2 {
                        match m.method.as_str() {
                            "get" => {
                                return Type::Generic("Option".to_string(), vec![margs[1].clone()]);
                            }
                            "contains" => return Type::Bool,
                            "len" => return Type::I64,
                            _ => return Type::Void,
                        }
                    }
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
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
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
                // `Mutex::new(v)` / `RwLock::new(v)`: element type is the explicit
                // type argument or, when omitted, inferred from the argument
                // (willow-dgwo.3).
                if (class_name == "Mutex" || class_name == "RwLock") && s.method == "new" {
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
                    let mangled = format!("{}__{}", module_prefix, s.method);
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
                if let Some(ty) = self.lambda_fn_types.get(&l.span) {
                    return ty.clone();
                }
                let params: Vec<Type> = l.params.iter().filter_map(|p| p.ty.clone()).collect();
                let ret = l
                    .return_type
                    .clone()
                    .or_else(|| self.lambda_return_types.get(&l.span).cloned())
                    .unwrap_or_else(|| {
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
        let mut current = Some(class.to_string());
        let mut seen = std::collections::HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                break;
            }
            if let Some(info) = self.static_storage.get(&(name.clone(), field.to_string())) {
                return Some(info.clone());
            }
            current = self.class_base.get(&name).cloned();
        }
        None
    }
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
    // Object layout: word 0 = type_id (not a GC ref), words 1..N = fields.
    // Bit i in the mask corresponds to word i; field[idx] lives at word (idx+1).
    let mut mask = 0u64;
    for (idx, (field_name, ty)) in layout.iter().enumerate() {
        if !is_gc_managed(ty, enum_infos) {
            continue;
        }
        let word = idx + 1;
        if word >= GC_REF_MASK_BITS {
            bail!(
                "class `{class_name}` field `{field_name}` is a GC-managed reference at payload word {word}, outside gc_ref_mask coverage; word 0 is class_type_id, so only the first {OBJECT_FIELD_MASK_CAPACITY} fields can be represented without a trace function"
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
// it lays out `[state | slot_count | data slot 0 | data slot 1 | …]` and shifts
// `gc_slot_mask` past the 2-word header internally. This stage is the compiler
// side: compute, for an async fn, the ordered data-slot layout and the GC
// reference mask the runtime needs to trace only the heap-reference slots.
//
// Slot-emission, live-across-await selection, and the suspend/resume state
// machine are Stage 5 (willow-lpn.5); it consumes `AsyncFrameLayout`. Here the
// mask computation is exact and the slot collector is the conservative initial
// layout (parameters + annotated `let` locals).

/// One data slot of an async fn's heap frame (excludes the fixed
/// `state`/`slot_count` header words, which are never GC references).
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
        Type::Generic(name, args) if name == "FrozenArray" && args.len() == 1 => args[0].clone(),
        Type::Generic(name, args) if name == "Range" && args.as_slice() == [Type::I64] => Type::I64,
        _ => Type::Void,
    }
}

fn try_propagate_payload_type(ty: &Type) -> Type {
    match ty {
        Type::Generic(name, args) if (name == "Result" || name == "Option") && !args.is_empty() => {
            args[0].clone()
        }
        _ => Type::I64,
    }
}

/// The error type `E` of a `Result<T, E>`, used by `?` automatic error
/// conversion (willow-1ow).
fn result_err_type(ty: &Type) -> Option<Type> {
    match ty {
        Type::Generic(name, args) if name == "Result" && args.len() == 2 => Some(args[1].clone()),
        _ => None,
    }
}

/// The error payload type `E` if `f` returns `Result<void, E>`, else `None`.
/// Such a function (when it is `main`) lowers to a void `willow_user_main` that
/// inspects the result and exits accordingly (willow-exg).
fn main_result_err_type(f: &FunctionDecl) -> Option<Type> {
    match &f.return_type {
        Type::Generic(n, args) if n == "Result" && args.len() == 2 && args[0] == Type::Void => {
            Some(args[1].clone())
        }
        _ => None,
    }
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
        Expr::Nil(_) => Type::Nil,
        Expr::String(_, _) => Type::String,
        Expr::Var(name, _) => vars
            .get(name.as_str())
            .map(|storage| storage.ty().clone())
            .unwrap_or(Type::I64),
        Expr::Binary(b) => match &b.op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
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
            if m.method == "join"
                && let Some(result_ty) = join_handle_result_type(&obj_ty)
            {
                return result_ty;
            }
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
                    "freeze" => {
                        return Type::Generic("FrozenArray".to_string(), vec![(**elem).clone()]);
                    }
                    _ => {}
                }
            }
            if let Type::Generic(name, fargs) = &obj_ty
                && name == "FrozenArray"
                && fargs.len() == 1
                && m.method == "len"
            {
                return Type::I64;
            }
            if let Type::Generic(name, margs) = &obj_ty {
                if name == "Map" && margs.len() == 2 {
                    match m.method.as_str() {
                        "get" => {
                            return Type::Generic("Option".to_string(), vec![margs[1].clone()]);
                        }
                        "len" => return Type::I64,
                        "contains" => return Type::Bool,
                        "freeze" => return Type::Generic("FrozenMap".to_string(), margs.clone()),
                        _ => return Type::Void,
                    }
                }
                if name == "FrozenMap" && margs.len() == 2 {
                    match m.method.as_str() {
                        "get" => {
                            return Type::Generic("Option".to_string(), vec![margs[1].clone()]);
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
            let mangled = format!("{}__{}", s.class, s.method);
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
            if let Type::Generic(name, args) = &inner_ty
                && (name == "Result" || name == "Option")
                && !args.is_empty()
            {
                return args[0].clone();
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
            Type::Generic(name, args) if name == "FrozenArray" && args.len() == 1 => {
                args.into_iter().next().unwrap()
            }
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

    match (&then_ty, &else_ty) {
        (Type::Nil, Type::Nil) => Type::Nil,
        (Type::Nullable(_), Type::Nil) => then_ty.clone(),
        (Type::Nil, Type::Nullable(_)) => else_ty.clone(),
        (Type::Nil, other) => Type::Nullable(Box::new(other.clone())),
        (other, Type::Nil) => Type::Nullable(Box::new(other.clone())),
        (Type::Nullable(inner), other) if inner.as_ref() == other => then_ty.clone(),
        (other, Type::Nullable(inner)) if inner.as_ref() == other => else_ty.clone(),
        _ => then_ty.clone(),
    }
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
        Expr::Nil(_) => Type::Nil,
        Expr::Var(name, _) => param_types.get(name.as_str()).cloned().unwrap_or(Type::I64),
        Expr::Binary(b) => match &b.op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
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
    match obj_ty {
        Type::Generic(name, args) if name == "Option" => {
            let inner = args.first().cloned().unwrap_or(Type::Void);
            match method {
                "is_some" | "is_none" => Some(Type::Bool),
                "unwrap" | "expect" | "unwrap_or" => Some(inner),
                "map" => {
                    if let Some(Type::Fn(_, ret)) = first_arg_ty {
                        Some(Type::Generic("Option".to_string(), vec![*ret.clone()]))
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
        Type::Generic(name, args) if name == "Result" => {
            let ok_ty = args.first().cloned().unwrap_or(Type::Void);
            let err_ty = args.get(1).cloned().unwrap_or(Type::Void);
            match method {
                "is_ok" | "is_err" => Some(Type::Bool),
                "unwrap" | "expect" | "unwrap_or" => Some(ok_ty.clone()),
                "unwrap_err" => Some(err_ty.clone()),
                "map" => {
                    if let Some(Type::Fn(_, ret)) = first_arg_ty {
                        Some(Type::Generic(
                            "Result".to_string(),
                            vec![*ret.clone(), err_ty],
                        ))
                    } else {
                        Some(obj_ty.clone())
                    }
                }
                "map_err" => {
                    if let Some(Type::Fn(_, ret)) = first_arg_ty {
                        Some(Type::Generic(
                            "Result".to_string(),
                            vec![ok_ty, *ret.clone()],
                        ))
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
mod tests {
    use super::*;
    use crate::diagnostics::Span;

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
    fn unit_async_codegen_02d_await_yield_is_suspend_point() {
        let await_yield = Expr::Await(Box::new(AwaitExpr {
            expr: Expr::Call(Box::new(CallExpr {
                callee: "yield".to_string(),
                args: vec![],
                span: Span::dummy(),
            })),
            span: Span::dummy(),
        }));
        assert!(is_await_yield(&await_yield));
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
        // Word 0 is class_type_id, so fields occupy mask bits 1..63.
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

    // 10. `T?` of a GC reference type is traced (mark non-nil; runtime skips nil).
    #[test]
    fn async_frame_10_nullable_ref_slot_traced() {
        let ty = Type::Nullable(Box::new(Type::Named("Node".to_string())));
        assert_eq!(frame_layout(&[("maybe", ty)]).gc_slot_mask, 0b1);
    }

    // 11. `T?` of a primitive type is NOT traced.
    #[test]
    fn async_frame_11_nullable_primitive_slot_not_traced() {
        let ty = Type::Nullable(Box::new(Type::I64));
        assert_eq!(frame_layout(&[("maybe", ty)]).gc_slot_mask, 0);
    }

    // 12. Nested `T??` of a GC reference is traced.
    #[test]
    fn async_frame_12_nested_nullable_ref_traced() {
        let ty = Type::Nullable(Box::new(Type::Nullable(Box::new(Type::String))));
        assert_eq!(frame_layout(&[("m", ty)]).gc_slot_mask, 0b1);
    }

    // 13. Future/Channel are opaque runtime pointers (no GcHeader) and are NOT
    //     traced from a frame slot; Task/JoinHandle are GC async frames and ARE traced.
    #[test]
    fn async_frame_13_runtime_pointer_generics_and_joinhandle() {
        let future = Type::Generic("Future".to_string(), vec![Type::I64]);
        let channel = Type::Generic("Channel".to_string(), vec![Type::String]);
        let task = Type::Generic("Task".to_string(), vec![Type::I64]);
        let join = Type::Generic("JoinHandle".to_string(), vec![Type::Void]);
        assert_eq!(frame_layout(&[("f", future)]).gc_slot_mask, 0);
        assert_eq!(frame_layout(&[("c", channel)]).gc_slot_mask, 0);
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
    //     allocator applies the 2-word header shift, not the compiler).
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

    #[test]
    fn unit_async_codegen_11_coop_main_allows_await_in_while_with_assignment() {
        let i_span = Span::new(1, 1, 1, 1);
        let await_sleep = Expr::Await(Box::new(AwaitExpr {
            expr: Expr::Call(Box::new(CallExpr {
                callee: "sleep".to_string(),
                args: vec![CallArg::value(Expr::Integer(1, Span::dummy()))],
                span: Span::dummy(),
            })),
            span: Span::dummy(),
        }));
        let f = FunctionDecl {
            name: "main".to_string(),
            public: false,
            is_async: true,
            params: Vec::new(),
            return_type: Type::Void,
            body: Block {
                stmts: vec![
                    Stmt::Let(LetStmt {
                        name: "i".to_string(),
                        mutable: true,
                        ty: Some(Type::I64),
                        init: Expr::Integer(0, Span::dummy()),
                        span: i_span,
                    }),
                    Stmt::While(WhileStmt {
                        cond: Expr::Binary(Box::new(BinaryExpr {
                            op: BinOp::Lt,
                            lhs: Expr::Var("i".to_string(), Span::dummy()),
                            rhs: Expr::Integer(2, Span::dummy()),
                            span: Span::dummy(),
                        })),
                        body: Block {
                            stmts: vec![
                                Stmt::Expr(ExprStmt {
                                    expr: await_sleep,
                                    span: Span::dummy(),
                                }),
                                Stmt::Assign(AssignStmt {
                                    name: "i".to_string(),
                                    value: Expr::Binary(Box::new(BinaryExpr {
                                        op: BinOp::Add,
                                        lhs: Expr::Var("i".to_string(), Span::dummy()),
                                        rhs: Expr::Integer(1, Span::dummy()),
                                        span: Span::dummy(),
                                    })),
                                    span: Span::dummy(),
                                }),
                            ],
                            span: Span::dummy(),
                        },
                        span: Span::dummy(),
                    }),
                ],
                span: Span::dummy(),
            },
            span: Span::dummy(),
        };

        assert!(cooperative_main_eligible(
            &f,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new()
        ));
    }

    #[test]
    fn unit_async_codegen_11b_coop_main_allows_await_yield() {
        let await_yield = Expr::Await(Box::new(AwaitExpr {
            expr: Expr::Call(Box::new(CallExpr {
                callee: "yield".to_string(),
                args: vec![],
                span: Span::dummy(),
            })),
            span: Span::dummy(),
        }));
        let f = FunctionDecl {
            name: "main".to_string(),
            public: false,
            is_async: true,
            params: Vec::new(),
            return_type: Type::Void,
            body: Block {
                stmts: vec![Stmt::Expr(ExprStmt {
                    expr: await_yield,
                    span: Span::dummy(),
                })],
                span: Span::dummy(),
            },
            span: Span::dummy(),
        };

        assert!(cooperative_main_eligible(
            &f,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new()
        ));
    }

    #[test]
    fn unit_async_codegen_12_coop_main_allows_await_in_for_loop() {
        let xs_span = Span::new(1, 2, 1, 1);
        let item_span = Span::new(2, 3, 2, 5);
        let await_sleep = Expr::Await(Box::new(AwaitExpr {
            expr: Expr::Call(Box::new(CallExpr {
                callee: "sleep".to_string(),
                args: vec![CallArg::value(Expr::Integer(1, Span::dummy()))],
                span: Span::dummy(),
            })),
            span: Span::dummy(),
        }));
        let for_stmt = ForStmt {
            name: "item".to_string(),
            name_span: item_span,
            iterable: Expr::Var("xs".to_string(), Span::new(2, 7, 2, 13)),
            body: Block {
                stmts: vec![Stmt::Expr(ExprStmt {
                    expr: await_sleep,
                    span: Span::dummy(),
                })],
                span: Span::dummy(),
            },
            span: Span::new(2, 20, 2, 1),
        };
        let mut async_local_types = HashMap::new();
        async_local_types.insert(for_stmt.iter_frame_key(), Type::Array(Box::new(Type::I64)));
        async_local_types.insert(for_stmt.index_frame_key(), Type::I64);
        async_local_types.insert(for_stmt.name_span, Type::I64);
        let f = FunctionDecl {
            name: "main".to_string(),
            public: false,
            is_async: true,
            params: Vec::new(),
            return_type: Type::Void,
            body: Block {
                stmts: vec![
                    Stmt::Let(LetStmt {
                        name: "xs".to_string(),
                        mutable: false,
                        ty: Some(Type::Array(Box::new(Type::I64))),
                        init: Expr::ArrayLiteral(
                            vec![
                                Expr::Integer(1, Span::dummy()),
                                Expr::Integer(2, Span::dummy()),
                            ],
                            Span::dummy(),
                        ),
                        span: xs_span,
                    }),
                    Stmt::For(for_stmt),
                ],
                span: Span::dummy(),
            },
            span: Span::dummy(),
        };

        assert!(cooperative_main_eligible(
            &f,
            &async_local_types,
            &HashMap::new(),
            &HashSet::new()
        ));
    }

    #[test]
    fn unit_async_codegen_13_coop_main_allows_await_in_range_for_loop() {
        let item_span = Span::new(1, 3, 1, 5);
        let await_sleep = Expr::Await(Box::new(AwaitExpr {
            expr: Expr::Call(Box::new(CallExpr {
                callee: "sleep".to_string(),
                args: vec![CallArg::value(Expr::Integer(1, Span::dummy()))],
                span: Span::dummy(),
            })),
            span: Span::dummy(),
        }));
        let for_stmt = ForStmt {
            name: "n".to_string(),
            name_span: item_span,
            iterable: Expr::Range(Box::new(RangeExpr {
                start: Expr::Integer(1, Span::new(1, 10, 1, 10)),
                end: Expr::Integer(4, Span::new(1, 13, 1, 13)),
                span: Span::new(1, 14, 1, 10),
            })),
            body: Block {
                stmts: vec![Stmt::Expr(ExprStmt {
                    expr: await_sleep,
                    span: Span::dummy(),
                })],
                span: Span::dummy(),
            },
            span: Span::new(1, 20, 1, 1),
        };
        let mut async_local_types = HashMap::new();
        async_local_types.insert(for_stmt.iter_frame_key(), Type::I64);
        async_local_types.insert(for_stmt.index_frame_key(), Type::I64);
        async_local_types.insert(for_stmt.name_span, Type::I64);
        let f = FunctionDecl {
            name: "main".to_string(),
            public: false,
            is_async: true,
            params: Vec::new(),
            return_type: Type::Void,
            body: Block {
                stmts: vec![Stmt::For(for_stmt)],
                span: Span::dummy(),
            },
            span: Span::dummy(),
        };

        assert!(cooperative_main_eligible(
            &f,
            &async_local_types,
            &HashMap::new(),
            &HashSet::new()
        ));
    }

    // ── Cooperative await routing + callee-frame-slot reservation (0a6k.6) ──
    //
    // Unit coverage for the two decision helpers behind the imported-async
    // cooperative-await fix:
    //   * `is_leaf_call_await` — only a leaf direct call routes to the dedicated
    //     call-await; everything else (incl. non-leaf/imported calls) takes the
    //     general cooperative task-await rather than block-driving.
    //   * `await_callee_frame_slot_span` — every direct-call-form await reserves
    //     a callee-frame slot so resume RELOADS the frame instead of re-running
    //     the call.

    fn leaves(names: &[&str]) -> HashSet<FunctionId> {
        names.iter().map(|name| FunctionId::free(*name)).collect()
    }

    fn await_with(inner: Expr, await_span: Span) -> Expr {
        Expr::Await(Box::new(AwaitExpr {
            expr: inner,
            span: await_span,
        }))
    }

    fn call(callee: &str) -> Expr {
        Expr::Call(Box::new(CallExpr {
            callee: callee.to_string(),
            args: vec![],
            span: Span::dummy(),
        }))
    }

    fn method_call() -> Expr {
        Expr::MethodCall(Box::new(MethodCallExpr {
            object: Expr::Var("t".to_string(), Span::dummy()),
            method: "poll".to_string(),
            args: vec![],
            span: Span::dummy(),
        }))
    }

    fn static_call() -> Expr {
        Expr::StaticCall(Box::new(StaticCallExpr {
            class: "worker".to_string(),
            type_args: vec![],
            method: "make_value".to_string(),
            args: vec![],
            span: Span::dummy(),
        }))
    }

    #[test]
    fn unit_coop_await_01_leaf_direct_call_is_leaf_await() {
        let expr = await_with(call("local_async"), Span::dummy());
        assert!(is_leaf_call_await(&expr, &leaves(&["local_async"])));
    }

    #[test]
    fn unit_coop_await_02_non_leaf_direct_call_is_not_leaf_await() {
        // An item-imported async fn is absent from `cooperative_leaves`, so it is
        // NOT a leaf await — it takes the task-await (cooperative) path.
        let expr = await_with(call("imported_async"), Span::dummy());
        assert!(!is_leaf_call_await(&expr, &leaves(&["local_async"])));
    }

    #[test]
    fn unit_coop_await_03_aliased_item_import_is_not_leaf_await() {
        // `import worker::make_value as mv;` — the callee is the alias `mv`,
        // which is never a leaf, so it routes cooperatively.
        let expr = await_with(call("mv"), Span::dummy());
        assert!(!is_leaf_call_await(&expr, &leaves(&["local_async"])));
    }

    #[test]
    fn unit_coop_await_04_method_call_await_is_not_leaf_await() {
        let expr = await_with(method_call(), Span::dummy());
        assert!(!is_leaf_call_await(&expr, &leaves(&["local_async"])));
    }

    #[test]
    fn unit_coop_await_05_static_call_await_is_not_leaf_await() {
        let expr = await_with(static_call(), Span::dummy());
        assert!(!is_leaf_call_await(&expr, &leaves(&["worker"])));
    }

    #[test]
    fn unit_coop_await_06_await_of_var_is_not_leaf_await() {
        let expr = await_with(Expr::Var("t".to_string(), Span::dummy()), Span::dummy());
        assert!(!is_leaf_call_await(&expr, &leaves(&["t"])));
    }

    #[test]
    fn unit_coop_await_07_bare_call_without_await_is_not_leaf_await() {
        // The expression must be an `await`; a bare call is not.
        let expr = call("local_async");
        assert!(!is_leaf_call_await(&expr, &leaves(&["local_async"])));
    }

    #[test]
    fn unit_coop_await_08_leaf_call_reserves_slot_at_await_span() {
        let await_span = Span::new(10, 20, 3, 5);
        let expr = await_with(call("local_async"), await_span);
        assert_eq!(
            await_callee_frame_slot_span(&expr, &leaves(&["local_async"])),
            Some(await_span)
        );
    }

    #[test]
    fn unit_coop_await_09_non_leaf_call_reserves_slot_at_await_span() {
        // The double-call guard: an imported async await must still reserve a
        // slot, keyed by the await's span (not the inner call's).
        let await_span = Span::new(30, 40, 7, 9);
        let expr = await_with(call("imported_async"), await_span);
        assert_eq!(
            await_callee_frame_slot_span(&expr, &leaves(&["local_async"])),
            Some(await_span)
        );
    }

    #[test]
    fn unit_coop_await_10_method_call_await_reserves_slot() {
        let await_span = Span::new(1, 2, 1, 1);
        let expr = await_with(method_call(), await_span);
        assert_eq!(
            await_callee_frame_slot_span(&expr, &HashSet::new()),
            Some(await_span)
        );
    }

    #[test]
    fn unit_coop_await_11_static_call_await_reserves_slot() {
        let await_span = Span::new(2, 3, 1, 1);
        let expr = await_with(static_call(), await_span);
        assert_eq!(
            await_callee_frame_slot_span(&expr, &HashSet::new()),
            Some(await_span)
        );
    }

    #[test]
    fn unit_coop_await_12_await_of_var_reserves_no_slot() {
        // Awaiting a non-call value (e.g. a `Task` local) needs no callee-frame
        // slot — the value is already frame-backed.
        let expr = await_with(Expr::Var("t".to_string(), Span::dummy()), Span::dummy());
        assert_eq!(await_callee_frame_slot_span(&expr, &HashSet::new()), None);
    }

    #[test]
    fn unit_coop_await_13_non_await_reserves_no_slot() {
        let expr = call("imported_async");
        assert_eq!(
            await_callee_frame_slot_span(&expr, &leaves(&["local_async"])),
            None
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
